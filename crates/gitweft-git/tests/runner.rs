//! Runner tests against the real `git` binary, isolated from the user's configuration.

use std::thread;
use std::time::{Duration, Instant};

use gitweft_git::{CancelToken, GitErrorKind, StderrEvent};
use gitweft_testkit::Sandbox;

#[test]
fn init_commit_and_read_back() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("repo");
    sb.commit_file(&repo, "a.txt", "hello\n", "first");

    let out = sb
        .git()
        .command(&repo)
        .args(["log", "--format=%s"])
        .read_only()
        .run(&CancelToken::new())
        .unwrap();
    assert_eq!(out.stdout_str().trim(), "first");
}

#[test]
fn stdin_is_forwarded() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let out = sb
        .git()
        .command(&repo)
        .args(["hash-object", "--stdin"])
        .stdin("hello\n")
        .run(&CancelToken::new())
        .unwrap();
    // Well-known blob id of "hello\n".
    assert_eq!(
        out.stdout_str().trim(),
        "ce013625030ba8dba906f756967f9e9ca394464a"
    );
}

#[test]
fn failure_is_classified() {
    let sb = Sandbox::new();
    let err = sb
        .git()
        .command(sb.path())
        .args(["status"])
        .run(&CancelToken::new())
        .unwrap_err();
    assert_eq!(err.kind, GitErrorKind::NotARepo);
    assert_eq!(err.exit_code, Some(128));
    assert!(
        err.summary().starts_with("fatal: not a git repository"),
        "{}",
        err.summary()
    );
}

#[test]
fn clone_reports_progress() {
    let sb = Sandbox::new();
    let src = sb.init_repo("src");
    for i in 0..20 {
        std::fs::write(src.join(format!("f{i}.txt")), format!("{i}\n")).unwrap();
    }
    sb.run(&src, &["add", "."]);
    sb.run(&src, &["commit", "-qm", "files"]);

    let mut progress = Vec::new();
    // `--no-local` forces the pack protocol so git emits object progress.
    sb.git()
        .command(sb.path())
        .args(["clone", "--progress", "--no-local"])
        .arg(&src)
        .arg(sb.path().join("dst"))
        .run_with_events(&CancelToken::new(), |event| {
            if let StderrEvent::Progress(p) = event {
                progress.push(p);
            }
        })
        .unwrap();
    assert!(
        progress.iter().any(|p| p.percent == Some(100)),
        "expected a completed progress phase, got {progress:?}"
    );
}

#[test]
fn cancellation_kills_the_process_group() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");

    let cancel = CancelToken::new();
    let trigger = cancel.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        trigger.cancel();
    });

    let started = Instant::now();
    // A shell alias spawns `sleep` as a grandchild; it must die with the group.
    let err = sb
        .git()
        .command(&repo)
        .args(["-c", "alias.nap=!sleep 30", "nap"])
        .run(&cancel)
        .unwrap_err();
    assert_eq!(err.kind, GitErrorKind::Cancelled);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );
}

#[test]
fn cancellation_ends_even_when_a_child_ignores_sigterm() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");

    let cancel = CancelToken::new();
    let trigger = cancel.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        trigger.cancel();
    });

    // A pre-commit hook that ignores SIGTERM (and so does its `sleep`) and keeps the stderr
    // it shares with git open, while git itself goes on SIGTERM. An earlier version sent
    // the SIGKILL only to a git that outlived the grace period, so the run waited for the `sleep`.
    let hook = repo.join(".git/hooks/pre-commit");
    std::fs::write(&hook, "#!/bin/sh\ntrap '' TERM\nsleep 30\n").unwrap();
    gitweft_platform::fs::set_executable(&hook, true).unwrap();
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    sb.run(&repo, &["add", "a.txt"]);
    let started = Instant::now();
    let err = sb
        .git()
        .command(&repo)
        .args(["commit", "-qm", "blocked"])
        .run(&cancel)
        .unwrap_err();
    assert_eq!(err.kind, GitErrorKind::Cancelled);
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "took {:?}",
        started.elapsed()
    );
}
