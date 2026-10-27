use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use gitweft_git::CancelToken;
use gitweft_git::activity::{ACTIVITY_CAPACITY, ActivityEntry, ActivityLog, ActivityOutcome};
use gitweft_testkit::Sandbox;

#[test]
fn every_command_is_recorded_with_its_outcome() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let log = ActivityLog::new();
    let mut git = sb.git().clone();
    git.set_activity_log(log.clone());
    let changes = log.changes();
    assert!(changes.try_recv().is_err());

    git.command(&repo)
        .args(["status", "--short"])
        .read_only()
        .run(&CancelToken::new())
        .unwrap();
    git.command(&repo)
        .args(["rev-parse", "--verify", "no-such-revision"])
        .run(&CancelToken::new())
        .unwrap_err();
    let cancel = CancelToken::new();
    cancel.cancel();
    let _ = git.command(&repo).args(["status"]).run(&cancel);

    let entries = log.entries();
    assert_eq!(entries.len(), 3);
    // Newest first.
    assert_eq!(entries[0].outcome, ActivityOutcome::Cancelled);
    assert!(
        matches!(&entries[1].outcome, ActivityOutcome::Failed { summary, .. } if summary.contains("fatal")),
        "{:?}",
        entries[1].outcome
    );
    assert_eq!(entries[2].command, "git status --short");
    assert!(entries[2].read_only);
    assert_eq!(entries[2].outcome, ActivityOutcome::Succeeded);
    assert_eq!(entries[2].cwd, repo);
    // One wake-up for the whole burst.
    assert!(changes.try_recv().is_ok());
    assert!(changes.try_recv().is_err());

    // A handle without a log records nothing (and costs nothing).
    sb.git()
        .command(&repo)
        .args(["status"])
        .run(&CancelToken::new())
        .unwrap();
    assert_eq!(log.entries().len(), 3);
}

#[test]
fn only_the_newest_entries_are_kept() {
    let log = ActivityLog::new();
    for i in 0..ACTIVITY_CAPACITY + 10 {
        log.record(ActivityEntry {
            command: format!("git {i}"),
            cwd: PathBuf::from("/"),
            started: SystemTime::UNIX_EPOCH,
            duration: Duration::ZERO,
            read_only: true,
            outcome: ActivityOutcome::Succeeded,
        });
    }
    let entries = log.entries();
    assert_eq!(entries.len(), ACTIVITY_CAPACITY);
    assert_eq!(entries[0].command, format!("git {}", ACTIVITY_CAPACITY + 9));
    assert_eq!(entries.last().unwrap().command, "git 10");
}
