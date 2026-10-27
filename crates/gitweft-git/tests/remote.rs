//! Fetch, pull and push against real `file://` remotes (no network).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gitweft_git::read::{Tracking, branch_tracking};
use gitweft_git::remote::{FetchTarget, ForceMode, PullMode, PushRequest, fetch, pull, push};
use gitweft_git::{CancelToken, GitErrorKind, StderrEvent};
use gitweft_testkit::Sandbox;

/// A bare remote with one commit (as a `file://` URL) and a clone of it, `work`.
fn remote_and_clone(sb: &Sandbox) -> (String, PathBuf) {
    let seed = sb.init_repo("seed");
    sb.commit_file(&seed, "a.txt", "1\n", "first");
    let bare = sb.path().join("remote.git");
    sb.run(
        sb.path(),
        &[
            "clone",
            "-q",
            "--bare",
            seed.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    let url = format!("file://{}", bare.display());
    let work = clone(sb, &url, "work");
    (url, work)
}

fn clone(sb: &Sandbox, url: &str, name: &str) -> PathBuf {
    sb.run(sb.path(), &["clone", "-q", url, name]);
    sb.path().join(name)
}

fn rev(sb: &Sandbox, repo: &Path, rev: &str) -> String {
    sb.run(repo, &["rev-parse", rev]).trim().to_owned()
}

fn no_events(_: StderrEvent) {}

fn push_main(force: ForceMode) -> PushRequest {
    PushRequest {
        remote: "origin".into(),
        branch: "main".into(),
        remote_branch: "main".into(),
        set_upstream: false,
        force,
    }
}

#[test]
fn fetch_brings_new_commits_and_reports_progress() {
    let sb = Sandbox::new();
    let (url, work) = remote_and_clone(&sb);
    let other = clone(&sb, &url, "other");
    let pushed = sb.commit_file(&other, "b.txt", "2\n", "second");
    sb.run(&other, &["push", "-q", "origin", "main"]);

    let mut progress = 0;
    fetch(
        sb.git(),
        &work,
        FetchTarget::All,
        true,
        &CancelToken::new(),
        |event| {
            if matches!(event, StderrEvent::Progress(_)) {
                progress += 1;
            }
        },
    )
    .unwrap();
    assert_eq!(rev(&sb, &work, "origin/main"), pushed);
    assert!(
        progress > 0,
        "--progress reports progress without a terminal"
    );
}

#[test]
fn pull_fast_forwards_and_divergence_needs_a_choice() {
    let sb = Sandbox::new();
    let (url, work) = remote_and_clone(&sb);
    let other = clone(&sb, &url, "other");
    let theirs = sb.commit_file(&other, "b.txt", "2\n", "theirs");
    sb.run(&other, &["push", "-q", "origin", "main"]);
    let cancel = CancelToken::new();

    pull(
        sb.git(),
        &work,
        PullMode::Configured,
        false,
        &cancel,
        no_events,
    )
    .unwrap();
    assert_eq!(rev(&sb, &work, "HEAD"), theirs);

    // Both sides move on: without pull.rebase, git wants to be told how to combine them.
    sb.commit_file(&work, "c.txt", "3\n", "ours");
    sb.commit_file(&other, "d.txt", "4\n", "theirs again");
    sb.run(&other, &["push", "-q", "origin", "main"]);
    for mode in [PullMode::Configured, PullMode::FastForwardOnly] {
        let err = pull(sb.git(), &work, mode, false, &cancel, no_events).unwrap_err();
        assert_eq!(err.kind, GitErrorKind::DivergentBranches, "{}", err.stderr);
    }

    pull(sb.git(), &work, PullMode::Rebase, false, &cancel, no_events).unwrap();
    assert_eq!(
        sb.run(&work, &["log", "--format=%s", "-3"]),
        "ours\ntheirs again\ntheirs\n"
    );
}

#[test]
fn fetch_prunes_deleted_branches_only_when_asked() {
    let sb = Sandbox::new();
    let (url, work) = remote_and_clone(&sb);
    let other = clone(&sb, &url, "other");
    sb.run(&other, &["push", "-q", "origin", "main:refs/heads/gone"]);
    let cancel = CancelToken::new();
    let fetch_origin = |prune: bool| {
        fetch(
            sb.git(),
            &work,
            FetchTarget::Remote("origin"),
            prune,
            &cancel,
            no_events,
        )
        .unwrap();
    };
    let gone = || {
        sb.run(
            &work,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/remotes/origin/gone",
            ],
        )
    };

    fetch_origin(false);
    assert_eq!(gone(), "refs/remotes/origin/gone\n");
    sb.run(&other, &["push", "-q", "origin", "--delete", "gone"]);
    fetch_origin(false);
    assert_eq!(gone(), "refs/remotes/origin/gone\n", "kept without prune");
    fetch_origin(true);
    assert_eq!(gone(), "");
    assert_eq!(rev(&sb, &work, "origin/main"), rev(&sb, &other, "main"));
}

#[test]
fn pull_merges_and_autostash_carries_local_changes_across() {
    let sb = Sandbox::new();
    let (url, work) = remote_and_clone(&sb);
    let lines = "1\n2\n3\n4\n5\n6\n7\n8\n";
    sb.commit_file(&work, "m.txt", lines, "lines");
    sb.run(&work, &["push", "-q", "origin", "main"]);
    let other = clone(&sb, &url, "other");
    let theirs = sb.commit_file(&other, "m.txt", &lines.replace("1\n", "one\n"), "theirs");
    sb.run(&other, &["push", "-q", "origin", "main"]);
    let ours = sb.commit_file(&work, "c.txt", "c\n", "ours");
    // A local change to the file the merge changes, elsewhere in it.
    std::fs::write(work.join("m.txt"), lines.replace("8\n", "eight\n")).unwrap();
    let cancel = CancelToken::new();

    let err = pull(sb.git(), &work, PullMode::Merge, false, &cancel, no_events).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::WouldOverwrite, "{}", err.stderr);
    assert_eq!(rev(&sb, &work, "HEAD"), ours);

    pull(sb.git(), &work, PullMode::Merge, true, &cancel, no_events).unwrap();
    // A merge commit of ours and theirs (not a rebase)…
    let parents = sb.run(&work, &["rev-list", "--parents", "-n", "1", "HEAD"]);
    let parents: Vec<&str> = parents.split_whitespace().skip(1).collect();
    assert_eq!(parents, [ours.as_str(), theirs.as_str()]);
    // …with the local change back on top, unstaged, and no stash left over.
    assert_eq!(
        std::fs::read_to_string(work.join("m.txt")).unwrap(),
        "one\n2\n3\n4\n5\n6\n7\neight\n"
    );
    assert_eq!(sb.run(&work, &["status", "--porcelain"]), " M m.txt\n");
    assert_eq!(sb.run(&work, &["stash", "list"]), "");
}

#[test]
fn push_sets_the_upstream_and_tracking_follows() {
    let sb = Sandbox::new();
    let (_, work) = remote_and_clone(&sb);
    sb.run(&work, &["switch", "-q", "-c", "feature"]);
    sb.commit_file(&work, "f.txt", "f\n", "feature work");
    let feature = || -> Tracking {
        branch_tracking(sb.git(), &work, &CancelToken::new())
            .unwrap()
            .into_iter()
            .find(|t| t.branch == "feature")
            .unwrap()
    };
    assert_eq!(feature().upstream, None);

    let request = PushRequest {
        remote: "origin".into(),
        branch: "feature".into(),
        remote_branch: "feature".into(),
        set_upstream: true,
        force: ForceMode::None,
    };
    push(sb.git(), &work, &request, &CancelToken::new(), no_events).unwrap();
    let pushed = feature();
    assert_eq!(pushed.upstream.as_deref(), Some("origin/feature"));
    assert_eq!((pushed.ahead, pushed.behind, pushed.gone), (0, 0, false));

    sb.commit_file(&work, "g.txt", "g\n", "more");
    assert_eq!(feature().ahead, 1);

    // Deleted on the remote: the upstream is gone.
    sb.run(&work, &["push", "-q", "origin", "--delete", "feature"]);
    assert!(feature().gone, "{:?}", feature());
}

#[test]
fn a_failing_pre_push_hook_is_a_hook_failure_in_its_own_words() {
    use std::os::unix::fs::PermissionsExt as _;
    let sb = Sandbox::new();
    let (_, work) = remote_and_clone(&sb);
    let hook = work.join(".git/hooks/pre-push");
    std::fs::write(
        &hook,
        "#!/bin/sh\necho 'no pushing on Fridays' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    sb.commit_file(&work, "b.txt", "2\n", "second");

    // git adds only "failed to push some refs": the runner finds the hook, and the
    // summary is the hook's line, not git's.
    let err = push(
        sb.git(),
        &work,
        &push_main(ForceMode::None),
        &CancelToken::new(),
        no_events,
    )
    .unwrap_err();
    assert_eq!(err.kind, GitErrorKind::HookFailed, "{}", err.stderr);
    assert_eq!(err.summary(), "no pushing on Fridays");
}

#[test]
fn a_branch_whose_name_isnt_utf8_is_pushed_by_its_bytes() {
    use gitweft_git::undo::ref_name_text;
    use std::os::unix::ffi::OsStrExt as _;
    let sb = Sandbox::new();
    let (_, work) = remote_and_clone(&sb);
    let cancel = CancelToken::new();
    sb.git()
        .command(&work)
        .args(["switch", "-q", "-c"])
        .arg(std::ffi::OsStr::from_bytes(b"caf\xe9"))
        .run(&cancel)
        .unwrap();
    sb.commit_file(&work, "c.txt", "c\n", "cafe work");

    // The names as the app keeps a name that isn't UTF-8; push gives git the bytes.
    let name = ref_name_text(b"caf\xe9");
    let request = PushRequest {
        remote: "origin".into(),
        branch: name.clone(),
        remote_branch: name,
        set_upstream: true,
        force: ForceMode::None,
    };
    push(sb.git(), &work, &request, &cancel, no_events).unwrap();
    let listed = sb
        .git()
        .command(&work)
        .args(["ls-remote", "origin"])
        .read_only()
        .run(&cancel)
        .unwrap()
        .stdout;
    let wanted = b"\trefs/heads/caf\xe9\n";
    assert!(
        listed.windows(wanted.len()).any(|line| line == wanted),
        "{}",
        String::from_utf8_lossy(&listed)
    );
}

#[test]
fn push_keeps_a_local_upstream_and_follows_a_renamed_one() {
    let sb = Sandbox::new();
    let (_, work) = remote_and_clone(&sb);
    let cancel = CancelToken::new();
    let remotes = ["origin".to_owned()];
    let target = |branch: &str| {
        let tracking = branch_tracking(sb.git(), &work, &cancel).unwrap();
        gitweft_git::remote::push_target(branch, &tracking, &remotes).unwrap()
    };

    // Its upstream is the local main (`branch.feature.remote = .`). An earlier version
    // pushed to origin with --set-upstream and replaced it.
    sb.run(&work, &["switch", "-q", "-c", "feature"]);
    sb.commit_file(&work, "f.txt", "f\n", "feature work");
    sb.run(
        &work,
        &["branch", "-q", "--set-upstream-to=main", "feature"],
    );
    let request = target("feature");
    assert_eq!(
        (request.remote.as_str(), request.remote_branch.as_str()),
        ("origin", "feature")
    );
    assert!(!request.set_upstream);
    push(sb.git(), &work, &request, &cancel, no_events).unwrap();
    assert_eq!(
        sb.run(&work, &["config", "branch.feature.remote"]),
        ".\n",
        "the local upstream is kept"
    );
    assert!(
        !sb.run(&work, &["ls-remote", "origin", "refs/heads/feature"])
            .is_empty()
    );

    // Tracks origin's `other` under another name: the push goes to `other`.
    sb.run(&work, &["switch", "-q", "-c", "renamed"]);
    sb.run(&work, &["push", "-q", "origin", "renamed:other"]);
    sb.run(
        &work,
        &["branch", "-q", "--set-upstream-to=origin/other", "renamed"],
    );
    sb.commit_file(&work, "r.txt", "r\n", "renamed work");
    let request = target("renamed");
    assert_eq!(request.remote_branch, "other");
    push(sb.git(), &work, &request, &cancel, no_events).unwrap();
    assert_eq!(
        sb.run(&work, &["rev-parse", "origin/other"]),
        sb.run(&work, &["rev-parse", "renamed"])
    );
}

#[test]
fn rewritten_history_needs_a_lease_push_which_protects_other_peoples_work() {
    let sb = Sandbox::new();
    let (url, work) = remote_and_clone(&sb);
    let cancel = CancelToken::new();
    sb.commit_file(&work, "b.txt", "2\n", "typo");
    push(
        sb.git(),
        &work,
        &push_main(ForceMode::None),
        &cancel,
        no_events,
    )
    .unwrap();

    // Amending a pushed commit: a plain push is rejected, a lease push goes through.
    sb.run(&work, &["commit", "-q", "--amend", "-m", "fixed"]);
    let err = push(
        sb.git(),
        &work,
        &push_main(ForceMode::None),
        &cancel,
        no_events,
    )
    .unwrap_err();
    assert_eq!(err.kind, GitErrorKind::NonFastForward, "{}", err.stderr);
    push(
        sb.git(),
        &work,
        &push_main(ForceMode::WithLease),
        &cancel,
        no_events,
    )
    .unwrap();
    let remote_main = sb.run(&work, &["ls-remote", "origin", "refs/heads/main"]);
    assert!(
        remote_main.starts_with(&rev(&sb, &work, "HEAD")),
        "{remote_main}"
    );

    // Someone else pushes; we fetch but don't integrate their commit. The lease push
    // must not throw it away.
    let other = clone(&sb, &url, "other");
    sb.commit_file(&other, "c.txt", "3\n", "their work");
    sb.run(&other, &["push", "-q", "origin", "main"]);
    sb.run(&work, &["commit", "-q", "--amend", "-m", "fixed again"]);
    fetch(
        sb.git(),
        &work,
        FetchTarget::Remote("origin"),
        false,
        &cancel,
        no_events,
    )
    .unwrap();
    let err = push(
        sb.git(),
        &work,
        &push_main(ForceMode::WithLease),
        &cancel,
        no_events,
    )
    .unwrap_err();
    assert_eq!(err.kind, GitErrorKind::NonFastForward, "{}", err.stderr);
}

#[test]
fn cancel_kills_a_hanging_transport() {
    use std::os::unix::fs::PermissionsExt;
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let hang = sb.path().join("hang.sh");
    std::fs::write(&hang, "#!/bin/sh\nsleep 30\n").unwrap();
    std::fs::set_permissions(&hang, std::fs::Permissions::from_mode(0o755)).unwrap();
    sb.run(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "ssh://git.example.invalid/repo.git",
        ],
    );
    let mut git = sb.git().clone();
    git.set_env("GIT_SSH_COMMAND", hang);

    let cancel = CancelToken::new();
    let canceller = {
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            cancel.cancel();
        })
    };
    let started = Instant::now();
    let err = fetch(
        &git,
        &repo,
        FetchTarget::Remote("origin"),
        false,
        &cancel,
        no_events,
    )
    .unwrap_err();
    canceller.join().unwrap();
    assert_eq!(err.kind, GitErrorKind::Cancelled, "{}", err.stderr);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "took {:?}",
        started.elapsed()
    );
}
