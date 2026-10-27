//! Getting past git's refusals with real git: an untrusted folder, a stale lock file, an
//! unknown identity.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::Duration;

use gitweft_git::gix::bstr::BString;
use gitweft_git::ops::{CommitOptions, ConfigOrigin, commit, identity};
use gitweft_git::read::{DiffOptions, DiffSource, file_diff};
use gitweft_git::repair::{
    inspect_lock, remove_lock, set_identity, trust_directory, untrusted_folder, untrusted_path,
};
use gitweft_git::{CancelToken, GitCli, GitErrorKind, ObjectId};
use gitweft_testkit::Sandbox;

const IDENTITY_ENV: [&str; 5] = [
    "GIT_AUTHOR_NAME",
    "GIT_AUTHOR_EMAIL",
    "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
    "EMAIL",
];

/// The sandbox's isolation without its identity: no `GIT_AUTHOR_*`/`GIT_COMMITTER_*`, and
/// `EMAIL` empty (git treats that as unset). `None` when this test process itself has an
/// identity in its environment, which a `GitCli` can't take away.
fn git_without_identity(sb: &Sandbox) -> Option<GitCli> {
    if let Some(key) = IDENTITY_ENV[..4]
        .iter()
        .find(|key| std::env::var_os(key).is_some())
    {
        eprintln!("skipped: {key} is set in the test environment");
        return None;
    }
    let home = sb.path();
    let mut git = GitCli::discover().unwrap();
    git.set_env("GIT_CONFIG_NOSYSTEM", "1");
    git.set_env("HOME", home);
    git.set_env("XDG_CONFIG_HOME", home.join(".config"));
    git.set_env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"));
    git.set_env("GIT_CEILING_DIRECTORIES", home.parent().unwrap_or(home));
    git.set_env("EMAIL", "");
    Some(git)
}

fn config_get(git: &GitCli, repo: &Path, scope: &str, key: &str) -> Option<String> {
    git.command(repo)
        .args(["config", scope, "--get", key])
        .run(&CancelToken::new())
        .ok()
        .map(|out| out.stdout_str().trim().to_owned())
}

fn same_file(a: &Path, b: &Path) -> bool {
    a.canonicalize().unwrap() == b.canonicalize().unwrap()
}

#[test]
fn trusting_a_folder_git_refuses() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let cancel = CancelToken::new();
    // Every repository now counts as someone else's; safe.directory is still consulted.
    let mut git = sb.git().clone();
    git.set_env("GIT_TEST_ASSUME_DIFFERENT_OWNER", "1");
    let status = || git.command(&repo).args(["status"]).run(&cancel);

    let err = status().unwrap_err();
    assert_eq!(err.kind, GitErrorKind::DubiousOwnership, "{}", err.stderr);
    let path = untrusted_path(&err.stderr).unwrap();
    assert!(same_file(&path, &repo), "{path:?} in {}", err.stderr);

    trust_directory(&git, &path, &cancel).unwrap();
    status().unwrap();
    // A second time adds nothing.
    trust_directory(&git, &path, &cancel).unwrap();
    let listed = git
        .command(sb.path())
        .args(["config", "--global", "--get-all", "safe.directory"])
        .run(&cancel)
        .unwrap();
    assert_eq!(
        listed.stdout_str().lines().collect::<Vec<_>>(),
        [path.to_str().unwrap()]
    );
    // Written to the sandbox's global config, not the user's.
    let written = std::fs::read_to_string(sb.path().join(".gitconfig")).unwrap();
    assert!(written.contains("safe"), "{written}");
    assert!(written.contains(path.to_str().unwrap()), "{written}");
}

#[test]
fn trusting_adds_to_other_trusted_folders() {
    let sb = Sandbox::new();
    let cancel = CancelToken::new();
    let (a, b) = (sb.init_repo("a"), sb.init_repo("b"));
    trust_directory(sb.git(), &a, &cancel).unwrap();
    trust_directory(sb.git(), &b, &cancel).unwrap();
    trust_directory(sb.git(), &a, &cancel).unwrap();
    let listed = sb.run(
        sb.path(),
        &["config", "--global", "--get-all", "safe.directory"],
    );
    assert_eq!(
        listed.lines().collect::<Vec<_>>(),
        [a.to_str().unwrap(), b.to_str().unwrap()]
    );
}

/// A copy of `sleep` named `git`, so /proc reports it as a git process; killed on drop.
struct FakeGit(Child);

impl FakeGit {
    fn start(sb: &Sandbox, cwd: &Path) -> Self {
        let program = sb.path().join("bin").join("git");
        if !program.exists() {
            std::fs::create_dir_all(program.parent().unwrap()).unwrap();
            let sleep = ["/usr/bin/sleep", "/bin/sleep"]
                .into_iter()
                .find(|p| Path::new(p).exists())
                .expect("sleep is installed");
            std::fs::copy(sleep, &program).unwrap();
        }
        // Another test's thread forking while the copy was still open for writing keeps a
        // write handle to it for a moment, and exec fails with "Text file busy": try again.
        for _ in 0..100 {
            // Not git at all: a stand-in process for /proc to find, never run through GitCli.
            #[allow(clippy::disallowed_methods)]
            match std::process::Command::new(&program)
                .arg("60")
                .current_dir(cwd)
                .spawn()
            {
                Ok(child) => return Self(child),
                Err(err) if err.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(err) => panic!("starting the stand-in git: {err}"),
            }
        }
        panic!("the stand-in git stayed busy");
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for FakeGit {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn inspecting_a_lock() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let git_dir = repo.join(".git");
    let lock = git_dir.join("index.lock");
    let dirs = [repo.clone(), git_dir.clone()];

    let info = inspect_lock(&lock, &dirs);
    assert!(!info.exists);
    assert_eq!(info.age, None);
    assert_eq!(info.path, lock);

    std::fs::write(&lock, "").unwrap();
    let info = inspect_lock(&lock, &dirs);
    assert!(info.exists);
    assert!(info.age.unwrap() < Duration::from_secs(60), "{info:?}");
    assert_eq!(info.holders, []);

    // A git working in the repository (in a subfolder) holds it; one elsewhere doesn't.
    std::fs::create_dir(repo.join("sub")).unwrap();
    let inside = FakeGit::start(&sb, &repo.join("sub"));
    let outside = FakeGit::start(&sb, sb.path());
    let holders = inspect_lock(&lock, &dirs).holders;
    assert_eq!(holders.len(), 1, "{holders:?}");
    assert_eq!(holders[0].pid, inside.pid());
    assert!(
        holders[0].command.ends_with("bin/git 60"),
        "{:?}",
        holders[0].command
    );
    assert!(holders.iter().all(|h| h.pid != outside.pid()));

    // Its working directory in the git dir counts too.
    let in_git_dir = FakeGit::start(&sb, &git_dir);
    let pids: Vec<u32> = inspect_lock(&lock, &dirs)
        .holders
        .iter()
        .map(|h| h.pid)
        .collect();
    let mut expected = vec![inside.pid(), in_git_dir.pid()];
    expected.sort();
    assert_eq!(pids, expected);

    drop((inside, in_git_dir));
    assert_eq!(inspect_lock(&lock, &dirs).holders, []);
}

#[test]
fn removing_a_stale_lock() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    let git_dir = repo.join(".git");
    std::fs::write(git_dir.join("index.lock"), "").unwrap();
    let cancel = CancelToken::new();
    let add = || sb.git().command(&repo).args(["add", "a.txt"]).run(&cancel);

    let err = add().unwrap_err();
    let GitErrorKind::LockHeld { path } = &err.kind else {
        panic!("{:?}: {}", err.kind, err.stderr);
    };
    let path = PathBuf::from(path);
    assert!(same_file(&path, &git_dir.join("index.lock")), "{path:?}");

    remove_lock(&path, std::slice::from_ref(&git_dir)).unwrap();
    assert!(!path.exists());
    add().unwrap();
    // Gone already: nothing to do.
    remove_lock(&path, &[git_dir]).unwrap();
}

#[test]
fn removing_only_lock_files_in_the_git_dir() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let git_dir = repo.join(".git");
    let git_dirs = std::slice::from_ref(&git_dir);
    let refused = |path: &Path| {
        let err = remove_lock(path, git_dirs).unwrap_err();
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::InvalidInput,
            "{path:?}: {err}"
        );
    };

    // Not a lock file.
    refused(&git_dir.join("config"));
    assert!(git_dir.join("config").exists());
    // Outside the git dir: in the sandbox, and in the working tree.
    for outside in [sb.path().join("outside.lock"), repo.join("tree.lock")] {
        std::fs::write(&outside, "").unwrap();
        refused(&outside);
        assert!(outside.exists());
    }
    // A directory.
    std::fs::create_dir(git_dir.join("dir.lock")).unwrap();
    refused(&git_dir.join("dir.lock"));
    assert!(git_dir.join("dir.lock").is_dir());
    // A symlink pointing outside: neither it nor its target goes.
    let target = sb.path().join("precious");
    std::fs::write(&target, "keep").unwrap();
    std::os::unix::fs::symlink(&target, git_dir.join("x.lock")).unwrap();
    refused(&git_dir.join("x.lock"));
    assert!(git_dir.join("x.lock").is_symlink());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
    // A symlinked folder inside the git dir leading outside.
    let elsewhere = sb.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("a.lock"), "").unwrap();
    std::os::unix::fs::symlink(&elsewhere, git_dir.join("link")).unwrap();
    refused(&git_dir.join("link").join("a.lock"));
    assert!(elsewhere.join("a.lock").exists());
    // `..` out of the git dir.
    refused(&git_dir.join("..").join("tree.lock"));

    // Missing: fine. Inside a subfolder (a ref lock): removed.
    remove_lock(&git_dir.join("missing.lock"), git_dirs).unwrap();
    let ref_lock = git_dir.join("refs").join("heads").join("main.lock");
    std::fs::write(&ref_lock, "").unwrap();
    remove_lock(&ref_lock, git_dirs).unwrap();
    assert!(!ref_lock.exists());
}

#[test]
fn setting_the_identity() {
    let sb = Sandbox::new();
    let Some(git) = git_without_identity(&sb) else {
        return;
    };
    let cancel = CancelToken::new();
    let repo = sb.init_repo("r");
    // Never guess: the commit fails deterministically, whatever the hostname.
    sb.run(&repo, &["config", "user.useConfigOnly", "true"]);
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    sb.run(&repo, &["add", "a.txt"]);
    let err = commit(&git, &repo, "first", CommitOptions::default(), &cancel).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::IdentityUnknown, "{}", err.stderr);

    set_identity(
        &git,
        &repo,
        "Ada Lovelace",
        "ada@example.invalid",
        false,
        &cancel,
    )
    .unwrap();
    let get = |scope, key| config_get(&git, &repo, scope, key);
    assert_eq!(get("--local", "user.name").as_deref(), Some("Ada Lovelace"));
    assert_eq!(
        get("--local", "user.email").as_deref(),
        Some("ada@example.invalid")
    );
    assert_eq!(get("--global", "user.name"), None);
    commit(&git, &repo, "first", CommitOptions::default(), &cancel).unwrap();
    // Author and committer both come from the configuration just written.
    assert_eq!(
        sb.run(&repo, &["log", "-1", "--format=%an <%ae> / %cn <%ce>"])
            .trim(),
        "Ada Lovelace <ada@example.invalid> / Ada Lovelace <ada@example.invalid>"
    );

    // Replacing works even over several values.
    sb.run(&repo, &["config", "--add", "user.name", "Second"]);
    set_identity(&git, &repo, "Ada", "ada@example.invalid", false, &cancel).unwrap();
    assert_eq!(get("--local", "user.name").as_deref(), Some("Ada"));

    // Globally: every repository of the user, none of them in particular.
    let other = sb.init_repo("other");
    sb.run(&other, &["config", "user.useConfigOnly", "true"]);
    set_identity(
        &git,
        &other,
        "Grace",
        "grace@example.invalid",
        true,
        &cancel,
    )
    .unwrap();
    let get = |scope, key| config_get(&git, &other, scope, key);
    assert_eq!(get("--global", "user.name").as_deref(), Some("Grace"));
    assert_eq!(
        get("--global", "user.email").as_deref(),
        Some("grace@example.invalid")
    );
    assert_eq!(get("--local", "user.name"), None);
    let written = std::fs::read_to_string(sb.path().join(".gitconfig")).unwrap();
    assert!(written.contains("grace@example.invalid"), "{written}");
    std::fs::write(other.join("b.txt"), "b\n").unwrap();
    sb.run(&other, &["add", "b.txt"]);
    commit(&git, &other, "first", CommitOptions::default(), &cancel).unwrap();
}

#[test]
fn identity_from_config_env_or_nowhere() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let cancel = CancelToken::new();

    // No config, but the sandbox sets GIT_AUTHOR_* for git: filled in, not a guess.
    let id = identity(sb.git(), &repo, &cancel);
    assert_eq!(id.name.as_deref(), Some("Test Author"));
    assert_eq!(id.email.as_deref(), Some("author@example.invalid"));
    assert!(!id.guessed);

    // From the environment: no config file to name, and nothing it overrides.
    assert_eq!((id.name_origin, id.email_origin), (None, None));
    assert_eq!((id.env_name, id.env_email), (None, None));

    // Configured: the configuration's values, and where each is set: the name in the
    // repository, the email in a file it includes. The sandbox's GIT_AUTHOR_* still win
    // in a commit, so they come beside them.
    sb.run(&repo, &["config", "user.name", "Ada Lovelace"]);
    std::fs::write(
        sb.path().join("work identity"),
        "[user]\n\temail = ada@example.invalid\n",
    )
    .unwrap();
    let included = sb.path().join("work identity");
    sb.run(
        &repo,
        &["config", "include.path", included.to_str().unwrap()],
    );
    let id = identity(sb.git(), &repo, &cancel);
    assert_eq!(id.name.as_deref(), Some("Ada Lovelace"));
    assert_eq!(id.email.as_deref(), Some("ada@example.invalid"));
    assert!(!id.guessed);
    assert_eq!(
        id.author(),
        (Some("Test Author"), Some("author@example.invalid"))
    );
    sb.commit_file(&repo, "a.txt", "a\n", "first");
    assert_eq!(
        sb.run(&repo, &["log", "-1", "--format=%an <%ae>"]).trim(),
        "Test Author <author@example.invalid>",
        "what git wrote"
    );
    let origin = |o: Option<ConfigOrigin>| o.map(|o| (o.scope, o.file));
    assert_eq!(
        origin(id.name_origin),
        Some(("local".to_owned(), ".git/config".to_owned()))
    );
    assert_eq!(
        origin(id.email_origin),
        Some(("local".to_owned(), included.to_str().unwrap().to_owned()))
    );

    let Some(bare) = git_without_identity(&sb) else {
        return;
    };
    let id = identity(&bare, &repo, &cancel);
    assert_eq!(id.name.as_deref(), Some("Ada Lovelace"));
    assert!(!id.guessed);
    // Without GIT_AUTHOR_*, the configuration is what a commit records; one variable
    // overrides only its own half.
    assert_eq!(
        id.author(),
        (Some("Ada Lovelace"), Some("ada@example.invalid"))
    );
    let mut named = bare.clone();
    named.set_env("GIT_AUTHOR_NAME", "Ada at Home");
    let id = identity(&named, &repo, &cancel);
    assert_eq!(
        (
            id.name.as_deref(),
            id.env_name.as_deref(),
            id.env_email.as_deref()
        ),
        (Some("Ada Lovelace"), Some("Ada at Home"), None)
    );
    assert_eq!(
        id.author(),
        (Some("Ada at Home"), Some("ada@example.invalid"))
    );

    // Nothing configured and guessing disabled: git can't tell.
    let fresh = sb.init_repo("fresh");
    sb.run(&fresh, &["config", "user.useConfigOnly", "true"]);
    assert_eq!(
        identity(&bare, &fresh, &cancel),
        Default::default(),
        "no identity"
    );
    // Only the name configured: the email stays unknown.
    sb.run(&fresh, &["config", "user.name", "Ada"]);
    let id = identity(&bare, &fresh, &cancel);
    assert_eq!(
        (id.name.as_deref(), id.email.as_deref(), id.guessed),
        (Some("Ada"), None, false)
    );
    // `EMAIL` is a choice of the user's, even though git reads it where it would guess.
    sb.run(&fresh, &["config", "user.useConfigOnly", "false"]);
    let mut with_email = bare.clone();
    with_email.set_env("EMAIL", "ada@mail.example.invalid");
    let id = identity(&with_email, &fresh, &cancel);
    assert_eq!(
        (id.name.as_deref(), id.email.as_deref(), id.guessed),
        (Some("Ada"), Some("ada@mail.example.invalid"), false)
    );
}

#[test]
fn identity_guessed_by_git() {
    // Only the email configured: git takes the name from the account (passwd's full name
    // or the login), which is a guess. Its email guess needs a hostname with a domain,
    // which depends on the machine, so the email is the configured part.
    let sb = Sandbox::new();
    let Some(git) = git_without_identity(&sb) else {
        return;
    };
    let repo = sb.init_repo("r");
    sb.run(&repo, &["config", "user.email", "ada@example.invalid"]);
    let cancel = CancelToken::new();
    let resolved = git
        .command(&repo)
        .args(["var", "GIT_AUTHOR_IDENT"])
        .run(&cancel);
    let Ok(resolved) = resolved else {
        eprintln!("skipped: git can't guess a name for this account");
        return;
    };
    let resolved = resolved.stdout_str().into_owned();
    let guessed_name = resolved.split(" <").next().unwrap().to_owned();

    let id = identity(&git, &repo, &cancel);
    assert_eq!(id.name.as_deref(), Some(guessed_name.as_str()));
    assert_eq!(id.email.as_deref(), Some("ada@example.invalid"));
    assert!(id.guessed, "{id:?}");

    // The same name given through the environment is no guess.
    let mut named = git.clone();
    named.set_env("GIT_AUTHOR_NAME", &guessed_name);
    let id = identity(&named, &repo, &cancel);
    assert_eq!(id.name.as_deref(), Some(guessed_name.as_str()));
    assert!(!id.guessed);
}

#[test]
fn a_folder_that_isnt_utf8_is_found_from_the_opened_one_and_trusted() {
    let sb = Sandbox::new();
    let parent = sb.path().canonicalize().unwrap();
    let root = parent.join(OsStr::from_bytes(b"caf\xe9 repo"));
    std::fs::create_dir(&root).unwrap();
    sb.run(&root, &["init", "-q"]);
    let sub = root.join("src");
    std::fs::create_dir(&sub).unwrap();
    let cancel = CancelToken::new();
    // Git's test switch: every repository counts as another user's.
    let status = |cwd: &std::path::Path| {
        sb.git()
            .command(cwd)
            .args(["status", "--porcelain"])
            .env("GIT_TEST_ASSUME_DIFFERENT_OWNER", "1")
            .read_only()
            .run(&cancel)
    };
    let err = status(&sub).unwrap_err();
    assert!(
        untrusted_path(&err.stderr).unwrap() != root,
        "git's text is lossy"
    );
    assert_eq!(
        untrusted_folder(&err.stderr, Some(&sub)),
        Some(root.clone())
    );
    // Opened elsewhere: the printed path, as before.
    let printed = untrusted_folder(&err.stderr, Some(&parent.join("other")));
    assert_eq!(printed, untrusted_path(&err.stderr));

    trust_directory(sb.git(), &root, &cancel).unwrap();
    status(&sub).unwrap();
}

#[test]
fn a_diff_in_a_refused_folder_says_git_refuses_it() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "1\n", "first");
    sb.commit_file(&repo, "a.txt", "2\n", "second");
    let head = ObjectId::from_hex(sb.run(&repo, &["rev-parse", "HEAD"]).trim().as_bytes()).unwrap();
    let parent =
        ObjectId::from_hex(sb.run(&repo, &["rev-parse", "HEAD~1"]).trim().as_bytes()).unwrap();
    let mut git = sb.git().clone();
    git.set_env("GIT_TEST_ASSUME_DIFFERENT_OWNER", "1");
    let source = DiffSource::Commit {
        parent: Some(parent),
        commit: head,
    };
    let path = BString::from("a.txt");
    let cancel = CancelToken::new();
    // Without the check this was `git diff --no-index`'s usage text.
    let err = file_diff(
        &git,
        &repo,
        &source,
        &path,
        None,
        DiffOptions::default(),
        &cancel,
    )
    .unwrap_err();
    let err = err.downcast::<gitweft_git::GitError>().unwrap();
    assert_eq!(err.kind, GitErrorKind::DubiousOwnership, "{}", err.stderr);
    // Trusted, the same call works.
    let diff = file_diff(
        sb.git(),
        &repo,
        &source,
        &path,
        None,
        DiffOptions::default(),
        &cancel,
    );
    assert!(!diff.unwrap().hunks.is_empty());
}

/// A profile goes into the repository's own config only, signing key included; one
/// without a key leaves the repository's key alone.
#[test]
fn an_identity_profile_is_set_in_the_repository_only() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let cancel = CancelToken::new();
    gitweft_git::repair::apply_identity(
        sb.git(),
        &repo,
        "Ada Work",
        "ada@work.example",
        Some("ABC123"),
        &cancel,
    )
    .unwrap();
    assert_eq!(
        config_get(sb.git(), &repo, "--local", "user.name").as_deref(),
        Some("Ada Work")
    );
    assert_eq!(
        config_get(sb.git(), &repo, "--local", "user.email").as_deref(),
        Some("ada@work.example")
    );
    assert_eq!(
        config_get(sb.git(), &repo, "--local", "user.signingkey").as_deref(),
        Some("ABC123")
    );
    assert_eq!(config_get(sb.git(), &repo, "--global", "user.email"), None);
    assert_eq!(
        gitweft_git::repair::signing_key(sb.git(), &repo, &cancel).as_deref(),
        Some("ABC123")
    );

    gitweft_git::repair::apply_identity(sb.git(), &repo, "Ada", "ada@home.example", None, &cancel)
        .unwrap();
    assert_eq!(
        config_get(sb.git(), &repo, "--local", "user.email").as_deref(),
        Some("ada@home.example")
    );
    assert_eq!(
        config_get(sb.git(), &repo, "--local", "user.signingkey").as_deref(),
        Some("ABC123"),
        "left as it was"
    );
}
