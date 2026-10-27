//! External diff and merge tools through `git difftool` / `git mergetool`, with fake
//! tools configured in the sandbox repository.

use std::path::{Path, PathBuf};

use gitweft_git::gix::bstr::BStr;
use gitweft_git::tools::{
    DiffToolTarget, ToolError, ToolKind, configured_tool, run_difftool, run_mergetool,
};
use gitweft_git::{CancelToken, ObjectId};
use gitweft_testkit::Sandbox;

fn id(hex: &str) -> ObjectId {
    ObjectId::from_hex(hex.trim().as_bytes()).unwrap()
}

fn b(path: &str) -> &BStr {
    path.into()
}

fn config(sb: &Sandbox, repo: &Path, key: &str, value: &str) {
    sb.run(repo, &["config", key, value]);
}

/// Configures difftool `name` to append what it compared (`$LOCAL` then `$REMOTE`) to a
/// file in the sandbox, and returns that file.
fn fake_difftool(sb: &Sandbox, repo: &Path, name: &str) -> PathBuf {
    let out = sb.path().join(format!("{name}.out"));
    let cmd = format!(
        "printf '[' >> '{out}'; cat \"$LOCAL\" \"$REMOTE\" >> '{out}'; printf ']' >> '{out}'",
        out = out.display()
    );
    config(sb, repo, &format!("difftool.{name}.cmd"), &cmd);
    out
}

/// Everything the fake tool saw so far (`[local remote]` per run), or `None` when it
/// never ran.
fn seen(out: &Path) -> Option<String> {
    std::fs::read_to_string(out).ok()
}

/// Configures mergetool `fake`: takes "theirs" and reports success.
fn fake_mergetool(sb: &Sandbox, repo: &Path) {
    config(sb, repo, "mergetool.fake.cmd", "cp \"$REMOTE\" \"$MERGED\"");
    config(sb, repo, "mergetool.fake.trustExitCode", "true");
    config(sb, repo, "merge.tool", "fake");
}

/// `git ls-files -u` for the whole repository.
fn unmerged(sb: &Sandbox, repo: &Path) -> String {
    sb.run(repo, &["ls-files", "-u"])
}

/// `main` and `feature` both changed each of `files` on the same line; `main` is checked
/// out and `feature` merged into it, stopping on the conflicts.
fn conflicted(sb: &Sandbox, files: &[&str]) -> PathBuf {
    let repo = sb.init_repo("r");
    for file in files {
        sb.commit_file(&repo, file, "base\n", "base");
    }
    sb.run(&repo, &["switch", "-q", "-c", "feature"]);
    for file in files {
        sb.commit_file(&repo, file, "theirs\n", "feature");
    }
    sb.run(&repo, &["switch", "-q", "main"]);
    for file in files {
        sb.commit_file(&repo, file, "ours\n", "main");
    }
    let err = sb
        .git()
        .command(&repo)
        .args(["merge", "-q", "feature"])
        .run(&CancelToken::new())
        .unwrap_err();
    assert_eq!(err.exit_code, Some(1), "{}", err.stderr);
    assert_eq!(unmerged(sb, &repo).lines().count(), 3 * files.len());
    repo
}

#[test]
fn configured_tool_follows_gits_order() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let cancel = CancelToken::new();
    let tool = |kind| configured_tool(sb.git(), &repo, kind, &cancel).unwrap();

    assert_eq!(tool(ToolKind::Diff), None);
    assert_eq!(tool(ToolKind::Merge), None);

    // A diff tool falls back to the merge tool, as in git.
    config(&sb, &repo, "merge.tool", "kdiff3");
    assert_eq!(tool(ToolKind::Diff).as_deref(), Some("kdiff3"));
    assert_eq!(tool(ToolKind::Merge).as_deref(), Some("kdiff3"));

    config(&sb, &repo, "diff.tool", "vimdiff");
    assert_eq!(tool(ToolKind::Diff).as_deref(), Some("vimdiff"));
    assert_eq!(tool(ToolKind::Merge).as_deref(), Some("kdiff3"));

    config(&sb, &repo, "merge.guitool", "bc");
    assert_eq!(tool(ToolKind::Diff).as_deref(), Some("bc"));
    assert_eq!(tool(ToolKind::Merge).as_deref(), Some("bc"));

    // Any case in the key name; the GUI diff tool wins over everything.
    config(&sb, &repo, "diff.guiTool", "meld");
    assert_eq!(tool(ToolKind::Diff).as_deref(), Some("meld"));
    assert_eq!(tool(ToolKind::Merge).as_deref(), Some("bc"));

    // An empty value is unset; the repository's value overrides the global one.
    config(&sb, &repo, "diff.guitool", "");
    config(&sb, &repo, "merge.guitool", "");
    assert_eq!(tool(ToolKind::Diff).as_deref(), Some("vimdiff"));
    sb.run(&repo, &["config", "--global", "merge.tool", "global-tool"]);
    assert_eq!(tool(ToolKind::Merge).as_deref(), Some("kdiff3"));
    sb.run(&repo, &["config", "--unset", "merge.tool"]);
    assert_eq!(tool(ToolKind::Merge).as_deref(), Some("global-tool"));
}

#[test]
fn difftool_on_a_modified_working_tree_file() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "committed\n", "first");
    let head = sb.commit_file(&repo, "other.txt", "other\n", "second");
    std::fs::write(repo.join("a.txt"), "current\n").unwrap();
    std::fs::write(repo.join("other.txt"), "changed too\n").unwrap();
    let out = fake_difftool(&sb, &repo, "fake");
    config(&sb, &repo, "diff.tool", "fake");

    let target = DiffToolTarget::WorkingTree {
        base: Some(id(&head)),
    };
    run_difftool(
        sb.git(),
        &repo,
        &target,
        b("a.txt"),
        None,
        None,
        &CancelToken::new(),
    )
    .unwrap();
    // Only the one file, committed against current.
    assert_eq!(seen(&out).as_deref(), Some("[committed\ncurrent\n]"));
}

/// A stand-in for `flatpak-spawn` (the "host" is this machine) that starts the command with
/// no environment but `PATH` and its `--env` variables, as the host's session would, and
/// logs each call's arguments, one line each. Returns the program and its log.
fn spawner(sb: &Sandbox, body: &str) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let program = sb.path().join("spawn-stub");
    let log = sb.path().join("spawn.log");
    let script = format!(
        "#!/bin/sh\n\
         [ -n \"$STUB_CLEAN\" ] || exec env -i STUB_CLEAN=1 PATH=\"$PATH\" \"$0\" \"$@\"\n\
         unset STUB_CLEAN\n\
         printf '%s\\n' \"$*\" >> '{log}'\n\
         [ \"$1\" = --host ] || exit 64\n\
         shift\n\
         while :; do case \"$1\" in\n\
           --watch-bus) shift ;;\n\
           --directory=*) cd \"${{1#--directory=}}\" || exit 65; shift ;;\n\
           --env=*) export \"${{1#--env=}}\"; shift ;;\n\
           *) break ;;\n\
         esac; done\n\
         {body}\n",
        log = log.display()
    );
    std::fs::write(&program, script).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    // Another test's thread forking while the script was open for writing keeps it open in
    // that child until it execs, and running it fails with "Text file busy" meanwhile.
    for _ in 0..200 {
        // The stand-in spawner itself, asked for nothing: never git.
        #[allow(clippy::disallowed_methods)]
        let ran = std::process::Command::new(&program)
            .stdin(std::process::Stdio::null())
            .status();
        match ran {
            Err(err) if err.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(std::time::Duration::from_millis(10))
            }
            _ => break,
        }
    }
    let _ = std::fs::remove_file(&log);
    (program, log)
}

#[test]
fn tools_run_on_the_host_through_the_spawner() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let head = sb.commit_file(&repo, "a.txt", "committed\n", "first");
    std::fs::write(repo.join("a.txt"), "current\n").unwrap();
    let out = fake_difftool(&sb, &repo, "fake");
    let (spawn, log) = spawner(&sb, "exec \"$@\"");
    let mut git = sb.git().clone();
    git.set_env("GIT_ASKPASS", "/app/bin/gitweft");
    let host = git.on_host(&spawn).unwrap();
    assert_eq!(host.env_var("GIT_ASKPASS"), None);
    assert!(host.env_var("GIT_CONFIG_GLOBAL").is_some());

    let target = DiffToolTarget::WorkingTree {
        base: Some(id(&head)),
    };
    run_difftool(
        &host,
        &repo,
        &target,
        b("a.txt"),
        None,
        Some("fake"),
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(seen(&out).as_deref(), Some("[committed\ncurrent\n]"));
    // Every git ran through the spawner, in the repository, with the runner's variables.
    let calls = std::fs::read_to_string(&log).unwrap();
    let difftool = calls
        .lines()
        .find(|line| line.contains(" difftool "))
        .expect("difftool went through the spawner");
    let folder = format!("--host --watch-bus --directory={} ", repo.display());
    assert!(difftool.starts_with(&folder), "{difftool}");
    for part in [
        "--env=LANGUAGE=",
        "--env=GIT_TERMINAL_PROMPT=0",
        "--env=GIT_LITERAL_PATHSPECS=1",
        "--env=GIT_OPTIONAL_LOCKS=0",
        " git -c color.ui=false",
    ] {
        assert!(difftool.contains(part), "{part} missing: {difftool}");
    }
    assert!(!calls.contains("GIT_ASKPASS"), "{calls}");
}

#[test]
fn a_host_without_git_is_refused() {
    let sb = Sandbox::new();
    let (spawn, _) = spawner(&sb, "echo \"$1: command not found\" >&2; exit 127");
    let err = sb.git().on_host(&spawn).unwrap_err();
    assert_eq!(err.kind, gitweft_git::GitErrorKind::GitMissing);
    assert!(err.summary().contains("not found"), "{}", err.summary());
}

/// `tools_git` goes to the host only in a Flatpak, which it knows by the marker file at
/// the sandbox's root (#50). The askpass variables tell the two apart: the host's git
/// doesn't get them.
#[test]
fn the_tools_git_is_the_hosts_only_in_a_flatpak() {
    use gitweft_git::tools::{ToolError, tools_git_with};
    let sb = Sandbox::new();
    let marker = sb.path().join("flatpak-info");
    let (spawn, _) = spawner(&sb, "exec \"$@\"");
    let mut git = sb.git().clone();
    git.set_env("GIT_ASKPASS", "/app/bin/gitweft");

    let here = tools_git_with(&git, &marker, &spawn).unwrap();
    assert!(
        here.env_var("GIT_ASKPASS").is_some(),
        "no marker: git itself"
    );

    std::fs::write(&marker, "[Application]\n").unwrap();
    let host = tools_git_with(&git, &marker, &spawn).unwrap();
    assert_eq!(
        host.env_var("GIT_ASKPASS"),
        None,
        "the marker: the host's git"
    );

    let (no_git, _) = spawner(&sb, "echo \"$1: command not found\" >&2; exit 127");
    let refused = tools_git_with(&git, &marker, &no_git);
    assert!(matches!(refused, Err(ToolError::NoHostGit)), "{refused:?}");
}

#[test]
fn difftool_with_an_unborn_head() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    std::fs::write(repo.join("new.txt"), "new\n").unwrap();
    sb.run(&repo, &["add", "new.txt"]);
    std::fs::write(repo.join("new.txt"), "newer\n").unwrap();
    let out = fake_difftool(&sb, &repo, "fake");
    config(&sb, &repo, "diff.tool", "fake");

    let target = DiffToolTarget::WorkingTree { base: None };
    run_difftool(
        sb.git(),
        &repo,
        &target,
        b("new.txt"),
        None,
        None,
        &CancelToken::new(),
    )
    .unwrap();
    // Nothing against the working-tree file.
    assert_eq!(seen(&out).as_deref(), Some("[newer\n]"));
}

#[test]
fn difftool_on_commits() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let root = id(&sb.commit_file(&repo, "a.txt", "one\n", "root"));
    let second = id(&sb.commit_file(&repo, "a.txt", "two\n", "second"));
    std::fs::write(repo.join("a.txt"), "dirty\n").unwrap();
    let out = fake_difftool(&sb, &repo, "fake");
    config(&sb, &repo, "diff.tool", "fake");
    let cancel = CancelToken::new();

    let target = DiffToolTarget::Commit {
        parent: None,
        commit: root,
    };
    run_difftool(sb.git(), &repo, &target, b("a.txt"), None, None, &cancel).unwrap();
    assert_eq!(seen(&out).as_deref(), Some("[one\n]"));

    // The commit's content, not the working tree's.
    let target = DiffToolTarget::Commit {
        parent: Some(root),
        commit: second,
    };
    run_difftool(sb.git(), &repo, &target, b("a.txt"), None, None, &cancel).unwrap();
    assert_eq!(seen(&out).as_deref(), Some("[one\n][one\ntwo\n]"));
}

/// A root commit's parent and an unborn branch's base are the empty tree of the
/// repository's hash: SHA-1's empty tree doesn't exist in a SHA-256 repository.
#[test]
fn difftool_in_a_sha256_repository() {
    let sb = Sandbox::new();
    let cancel = CancelToken::new();

    let repo = sb.init_sha256_repo("r");
    let root = id(&sb.commit_file(&repo, "a.txt", "one\n", "root"));
    let out = fake_difftool(&sb, &repo, "fake");
    config(&sb, &repo, "diff.tool", "fake");
    let target = DiffToolTarget::Commit {
        parent: None,
        commit: root,
    };
    run_difftool(sb.git(), &repo, &target, b("a.txt"), None, None, &cancel).unwrap();
    assert_eq!(seen(&out).as_deref(), Some("[one\n]"));

    let unborn = sb.init_sha256_repo("unborn");
    std::fs::write(unborn.join("new.txt"), "new\n").unwrap();
    sb.run(&unborn, &["add", "new.txt"]);
    std::fs::write(unborn.join("new.txt"), "newer\n").unwrap();
    let out = fake_difftool(&sb, &unborn, "fake2");
    config(&sb, &unborn, "diff.tool", "fake2");
    let target = DiffToolTarget::WorkingTree { base: None };
    run_difftool(
        sb.git(),
        &unborn,
        &target,
        b("new.txt"),
        None,
        None,
        &cancel,
    )
    .unwrap();
    assert_eq!(seen(&out).as_deref(), Some("[newer\n]"));
}

#[test]
fn difftool_on_a_renamed_file() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let content = "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\n";
    let first = id(&sb.commit_file(&repo, "old.txt", content, "first"));
    sb.run(&repo, &["mv", "old.txt", "new.txt"]);
    std::fs::write(repo.join("new.txt"), format!("{content}line 7\n")).unwrap();
    sb.run(&repo, &["add", "new.txt"]);
    sb.run(&repo, &["commit", "-q", "-m", "rename"]);
    let renamed = id(&sb.run(&repo, &["rev-parse", "HEAD"]));
    let cmd = format!(
        "printf '%s\\n' \"$MERGED\" \"$LOCAL\" \"$REMOTE\" >> '{out}'",
        out = sb.path().join("names.out").display()
    );
    config(&sb, &repo, "difftool.names.cmd", &cmd);
    let out = fake_difftool(&sb, &repo, "fake");
    config(&sb, &repo, "diff.tool", "fake");
    let cancel = CancelToken::new();

    let target = DiffToolTarget::Commit {
        parent: Some(first),
        commit: renamed,
    };
    run_difftool(
        sb.git(),
        &repo,
        &target,
        b("new.txt"),
        Some(b("old.txt")),
        None,
        &cancel,
    )
    .unwrap();
    // One pair: the old file against the new one, not a deletion and an addition.
    assert_eq!(seen(&out).unwrap(), format!("[{content}{content}line 7\n]"));

    // git names the pair by its old path ($MERGED); the temporary files carry each
    // side's own name.
    run_difftool(
        sb.git(),
        &repo,
        &target,
        b("new.txt"),
        Some(b("old.txt")),
        Some("names"),
        &cancel,
    )
    .unwrap();
    let names = seen(&sb.path().join("names.out")).unwrap();
    let names: Vec<&str> = names.lines().collect();
    assert_eq!(names.len(), 3, "{names:?}");
    assert_eq!(names[0], "old.txt");
    assert!(names[1].ends_with("old.txt"), "{names:?}");
    assert!(names[2].ends_with("new.txt"), "{names:?}");
}

#[test]
fn a_tool_given_overrides_the_configured_one() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let head = id(&sb.commit_file(&repo, "a.txt", "1\n", "first"));
    std::fs::write(repo.join("a.txt"), "2\n").unwrap();
    let configured = fake_difftool(&sb, &repo, "fake");
    let given = fake_difftool(&sb, &repo, "fake2");
    config(&sb, &repo, "diff.guitool", "fake");
    let target = DiffToolTarget::WorkingTree { base: Some(head) };

    run_difftool(
        sb.git(),
        &repo,
        &target,
        b("a.txt"),
        None,
        Some("fake2"),
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(seen(&configured), None);
    assert_eq!(seen(&given).as_deref(), Some("[1\n2\n]"));

    // A name git doesn't know is an error, not a guess.
    let err = run_difftool(
        sb.git(),
        &repo,
        &target,
        b("a.txt"),
        None,
        Some("no-such-tool"),
        &CancelToken::new(),
    )
    .unwrap_err();
    let ToolError::Git(err) = err else {
        panic!("{err:?}")
    };
    assert!(err.stderr.contains("no-such-tool"), "{}", err.stderr);
}

#[test]
fn nothing_configured() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let head = id(&sb.commit_file(&repo, "a.txt", "1\n", "first"));
    std::fs::write(repo.join("a.txt"), "2\n").unwrap();
    // Tools exist but none is chosen.
    let out = fake_difftool(&sb, &repo, "fake");
    config(
        &sb,
        &repo,
        "mergetool.fake.cmd",
        "cp \"$REMOTE\" \"$MERGED\"",
    );
    let cancel = CancelToken::new();

    let target = DiffToolTarget::WorkingTree { base: Some(head) };
    let err = run_difftool(sb.git(), &repo, &target, b("a.txt"), None, None, &cancel).unwrap_err();
    assert!(
        matches!(err, ToolError::NotConfigured(ToolKind::Diff)),
        "{err:?}"
    );
    assert!(err.to_string().contains("diff.tool"), "{err}");
    assert_eq!(seen(&out), None);

    let err = run_mergetool(sb.git(), &repo, b("a.txt"), None, &cancel).unwrap_err();
    assert!(
        matches!(err, ToolError::NotConfigured(ToolKind::Merge)),
        "{err:?}"
    );
    assert!(err.to_string().contains("merge.tool"), "{err}");
}

#[test]
fn mergetool_resolves_a_conflict() {
    let sb = Sandbox::new();
    let repo = conflicted(&sb, &["a.txt", "b.txt"]);
    fake_mergetool(&sb, &repo);

    run_mergetool(sb.git(), &repo, b("a.txt"), None, &CancelToken::new()).unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.join("a.txt")).unwrap(),
        "theirs\n"
    );
    // a.txt is staged; only b.txt is still conflicted.
    let left = unmerged(&sb, &repo);
    assert!(!left.contains("a.txt"), "{left}");
    assert!(left.contains("b.txt"), "{left}");
    // git's default would leave the conflicted version behind as a.txt.orig: not unless
    // the user asks for it.
    assert!(!repo.join("a.txt.orig").exists());

    config(&sb, &repo, "mergetool.keepBackup", "true");
    run_mergetool(sb.git(), &repo, b("b.txt"), None, &CancelToken::new()).unwrap();
    assert_eq!(unmerged(&sb, &repo), "");
    let backup = std::fs::read_to_string(repo.join("b.txt.orig")).unwrap();
    assert!(backup.contains("<<<<<<<"), "{backup}");

    // A path with nothing to merge: git says so and succeeds.
    run_mergetool(sb.git(), &repo, b("a.txt"), None, &CancelToken::new()).unwrap();
}

#[test]
fn a_failed_merge_leaves_the_conflict() {
    let sb = Sandbox::new();
    let repo = conflicted(&sb, &["a.txt"]);
    let cancel = CancelToken::new();
    let before = std::fs::read_to_string(repo.join("a.txt")).unwrap();

    // A trusted exit code that says it failed, even though the file was written.
    config(
        &sb,
        &repo,
        "mergetool.failing.cmd",
        "cp \"$REMOTE\" \"$MERGED\"; exit 1",
    );
    config(&sb, &repo, "mergetool.failing.trustExitCode", "true");
    let err = run_mergetool(sb.git(), &repo, b("a.txt"), Some("failing"), &cancel).unwrap_err();
    assert!(matches!(err, ToolError::Git(_)), "{err:?}");
    assert!(unmerged(&sb, &repo).contains("a.txt"));
    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), before);

    // Not trusted and the file untouched: git asks "Was the merge successful?" on stdin,
    // which is empty, so this fails at once instead of waiting for an answer.
    config(&sb, &repo, "mergetool.idle.cmd", "true");
    let err = run_mergetool(sb.git(), &repo, b("a.txt"), Some("idle"), &cancel).unwrap_err();
    let ToolError::Git(err) = err else {
        panic!("{err:?}")
    };
    assert_eq!(err.exit_code, Some(1), "{}", err.stderr);
    assert!(unmerged(&sb, &repo).contains("a.txt"));
}

/// A tool git knows but can't find the program of (vimdiff without vim, #84) is refused
/// before git starts: git would write its temporary files first and leave them. One given
/// its own program (`mergetool.<tool>.path`) is started as before.
#[test]
fn a_merge_tool_that_isnt_installed_is_refused_first() {
    let sb = Sandbox::new();
    let repo = conflicted(&sb, &["a.txt"]);
    let cancel = CancelToken::new();

    // WinMerge's program, WinMergeU.exe, isn't on a Linux PATH.
    let err = run_mergetool(sb.git(), &repo, b("a.txt"), Some("winmerge"), &cancel).unwrap_err();
    assert!(
        matches!(&err, ToolError::MergeToolMissing(tool) if tool == "winmerge"),
        "{err:?}"
    );
    assert!(err.to_string().contains("mergetool.winmerge.path"), "{err}");
    let left: Vec<String> = std::fs::read_dir(&repo)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains('_'))
        .collect();
    assert!(left.is_empty(), "{left:?}");
    assert!(unmerged(&sb, &repo).contains("a.txt"));

    // `false` as its program: started, and its merge fails.
    config(&sb, &repo, "mergetool.winmerge.path", "false");
    let err = run_mergetool(sb.git(), &repo, b("a.txt"), Some("winmerge"), &cancel).unwrap_err();
    assert!(matches!(err, ToolError::Git(_)), "{err:?}");
    assert!(unmerged(&sb, &repo).contains("a.txt"));
}

#[test]
fn a_question_instead_of_a_tool_fails() {
    // Modify/delete: git asks "(m)odified or (d)eleted" on stdin instead of starting a
    // tool; with stdin empty that is an error, not a hang.
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "base\n", "base");
    sb.run(&repo, &["switch", "-q", "-c", "feature"]);
    sb.run(&repo, &["rm", "-q", "a.txt"]);
    sb.run(&repo, &["commit", "-q", "-m", "delete"]);
    sb.run(&repo, &["switch", "-q", "main"]);
    sb.commit_file(&repo, "a.txt", "ours\n", "main");
    sb.git()
        .command(&repo)
        .args(["merge", "-q", "feature"])
        .run(&CancelToken::new())
        .unwrap_err();
    fake_mergetool(&sb, &repo);

    let err = run_mergetool(sb.git(), &repo, b("a.txt"), None, &CancelToken::new()).unwrap_err();
    assert!(matches!(err, ToolError::Git(_)), "{err:?}");
    assert!(unmerged(&sb, &repo).contains("a.txt"));
}

#[test]
fn difftool_exit_codes() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let head = id(&sb.commit_file(&repo, "a.txt", "1\n", "first"));
    std::fs::write(repo.join("a.txt"), "2\n").unwrap();
    config(&sb, &repo, "difftool.failing.cmd", "exit 3");
    let target = DiffToolTarget::WorkingTree { base: Some(head) };
    let cancel = CancelToken::new();
    let run = || {
        run_difftool(
            sb.git(),
            &repo,
            &target,
            b("a.txt"),
            None,
            Some("failing"),
            &cancel,
        )
    };

    // Ignored by default, an error when trusted.
    run().unwrap();
    config(&sb, &repo, "difftool.trustExitCode", "true");
    assert!(matches!(run(), Err(ToolError::Git(_))));

    // A command that can't be found is always an error.
    config(&sb, &repo, "difftool.trustExitCode", "false");
    config(
        &sb,
        &repo,
        "difftool.failing.cmd",
        "gitweft-no-such-program",
    );
    assert!(matches!(run(), Err(ToolError::Git(_))));
}

#[test]
fn paths_are_literal() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let cancel = CancelToken::new();

    // Difftool: `*.txt` is that one file, not a glob matching the others.
    for (file, content) in [("*.txt", "star"), ("a b.txt", "space"), ("c.txt", "c")] {
        sb.commit_file(&repo, file, &format!("{content}\n"), file);
    }
    let head = id(&sb.run(&repo, &["rev-parse", "HEAD"]));
    for (file, content) in [("*.txt", "STAR"), ("a b.txt", "SPACE"), ("c.txt", "C")] {
        std::fs::write(repo.join(file), format!("{content}\n")).unwrap();
    }
    let out = fake_difftool(&sb, &repo, "fake");
    config(&sb, &repo, "diff.tool", "fake");
    let target = DiffToolTarget::WorkingTree { base: Some(head) };
    run_difftool(sb.git(), &repo, &target, b("*.txt"), None, None, &cancel).unwrap();
    assert_eq!(seen(&out).as_deref(), Some("[star\nSTAR\n]"));
    run_difftool(sb.git(), &repo, &target, b("a b.txt"), None, None, &cancel).unwrap();
    assert_eq!(
        seen(&out).as_deref(),
        Some("[star\nSTAR\n][space\nSPACE\n]")
    );

    // Mergetool: one conflicted file at a time, whatever its name. `:!a.txt` would be
    // pathspec magic ("everything but a.txt"). (A `*` name is only safe while no other
    // conflicted file matches it as a shell glob: see `run_mergetool`.)
    let sb = Sandbox::new();
    let repo = conflicted(&sb, &[":!a.txt", "a.txt", "a b.txt", "*.md", "c.txt"]);
    fake_mergetool(&sb, &repo);
    let names = |left: &str| -> Vec<String> {
        let mut names: Vec<String> = left
            .lines()
            .map(|l| l.split_once('\t').unwrap().1.to_owned())
            .collect();
        names.dedup();
        names
    };
    run_mergetool(sb.git(), &repo, b(":!a.txt"), None, &cancel).unwrap();
    assert_eq!(
        names(&unmerged(&sb, &repo)),
        ["*.md", "a b.txt", "a.txt", "c.txt"]
    );
    run_mergetool(sb.git(), &repo, b("*.md"), None, &cancel).unwrap();
    assert_eq!(names(&unmerged(&sb, &repo)), ["a b.txt", "a.txt", "c.txt"]);
    run_mergetool(sb.git(), &repo, b("a b.txt"), None, &cancel).unwrap();
    assert_eq!(names(&unmerged(&sb, &repo)), ["a.txt", "c.txt"]);
    for file in [":!a.txt", "*.md", "a b.txt"] {
        assert_eq!(
            std::fs::read_to_string(repo.join(file)).unwrap(),
            "theirs\n"
        );
    }
}
