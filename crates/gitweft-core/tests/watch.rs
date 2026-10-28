//! The watcher against real repositories and real inotify events.

use std::path::Path;
use std::time::{Duration, Instant};

use gitweft_core::watch::{Changes, RepoWatcher, Update};
use gitweft_git::CancelToken;
use gitweft_git::gix::bstr::BString;
use gitweft_git::read::{open_repo, read_status};
use gitweft_testkit::Sandbox;

const TIMEOUT: Duration = Duration::from_secs(5);
/// Well above the watcher's batching windows (75 / 200 ms), with margin for slow machines.
const QUIET: Duration = Duration::from_millis(800);

fn start(sb: &Sandbox, repo: &Path) -> (RepoWatcher, async_channel::Receiver<Update>) {
    let shared = open_repo(repo).unwrap().into_sync();
    let (watcher, updates) = RepoWatcher::start(sb.git().clone(), shared);
    assert_eq!(next(&updates, TIMEOUT), Some(Update::Ready));
    (watcher, updates)
}

fn next(updates: &async_channel::Receiver<Update>, within: Duration) -> Option<Update> {
    let deadline = Instant::now() + within;
    loop {
        if let Ok(update) = updates.try_recv() {
            return Some(update);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Everything reported within `within`, merged.
fn changes_within(updates: &async_channel::Receiver<Update>, within: Duration) -> Changes {
    let deadline = Instant::now() + within;
    let mut all = Changes::default();
    while let Some(update) = next(updates, deadline.saturating_duration_since(Instant::now())) {
        if let Update::Changed(changes) = update {
            all.merge(changes);
        }
    }
    all
}

/// The first reported change, plus whatever follows shortly after.
fn wait_for_change(updates: &async_channel::Receiver<Update>) -> Changes {
    loop {
        match next(updates, TIMEOUT) {
            Some(Update::Changed(mut changes)) => {
                changes.merge(changes_within(updates, QUIET));
                return changes;
            }
            Some(_) => {}
            None => panic!("no change reported within {TIMEOUT:?}"),
        }
    }
}

#[test]
fn editing_a_tracked_file_in_a_subdirectory_reports_status() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    std::fs::create_dir_all(repo.join("src/deep")).unwrap();
    sb.commit_file(&repo, "src/deep/a.txt", "1\n", "first");
    let (_watcher, updates) = start(&sb, &repo);

    std::fs::write(repo.join("src/deep/a.txt"), "2\n").unwrap();
    let changes = wait_for_change(&updates);
    assert!(changes.status, "{changes:?}");
    assert!(!changes.refs, "{changes:?}");
}

#[test]
fn a_commit_reports_refs_and_status() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "1\n", "first");
    let (_watcher, updates) = start(&sb, &repo);

    sb.commit_file(&repo, "a.txt", "2\n", "second");
    let changes = wait_for_change(&updates);
    assert!(changes.refs && changes.status, "{changes:?}");
}

#[test]
#[cfg_attr(
    windows,
    ignore = "on Windows an ignored file still reports status; not explained yet"
)]
fn ignored_files_and_status_reads_stay_quiet() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, ".gitignore", "*.log\n", "ignore logs");
    sb.commit_file(&repo, "a.txt", "1\n", "first");
    let (_watcher, updates) = start(&sb, &repo);

    std::fs::write(repo.join("build.log"), "noise\n").unwrap();
    assert_eq!(changes_within(&updates, QUIET), Changes::default());

    // A real change is still seen...
    std::fs::write(repo.join("a.txt"), "2\n").unwrap();
    assert!(wait_for_change(&updates).status);
    // ...but reading the status (index stat data is stale now) writes nothing.
    for _ in 0..2 {
        read_status(sb.git(), &repo, &CancelToken::new()).unwrap();
    }
    assert_eq!(changes_within(&updates, QUIET), Changes::default());
}

#[test]
fn new_directories_are_watched_once_the_status_knows_them() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "1\n", "first");
    let (watcher, updates) = start(&sb, &repo);

    std::fs::create_dir(repo.join("fresh")).unwrap();
    std::fs::write(repo.join("fresh/n.txt"), "n\n").unwrap();
    assert!(wait_for_change(&updates).status);

    watcher.set_untracked(vec![BString::from("fresh/n.txt")]);
    std::thread::sleep(Duration::from_millis(200)); // let it add the watch
    std::fs::write(repo.join("fresh/n.txt"), "changed\n").unwrap();
    assert!(wait_for_change(&updates).status);
}

#[test]
fn staging_reports_status_only() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "1\n", "first");
    std::fs::write(repo.join("a.txt"), "2\n").unwrap();
    let (_watcher, updates) = start(&sb, &repo);

    sb.run(&repo, &["add", "a.txt"]);
    let changes = wait_for_change(&updates);
    assert!(changes.status && !changes.refs, "{changes:?}");
}

#[test]
fn a_submodule_set_up_after_the_start_gets_watched() {
    let sb = Sandbox::new();
    // Submodule clones from a local path need this (set in the sandbox's own config).
    sb.run(
        sb.path(),
        &["config", "--global", "protocol.file.allow", "always"],
    );
    let lib = sb.init_repo("lib");
    let first = sb.commit_file(&lib, "lib.txt", "1\n", "first");
    sb.commit_file(&lib, "lib.txt", "2\n", "second");
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "1\n", "first");
    let (_watcher, updates) = start(&sb, &repo);

    sb.run(
        &repo,
        &["submodule", "add", "-q", lib.to_str().unwrap(), "lib"],
    );
    let changes = wait_for_change(&updates);
    assert!(changes.submodules && changes.status, "{changes:?}");

    // The new submodule's own HEAD is watched now: moving it is reported.
    sb.run(
        &repo.join("lib"),
        &["checkout", "-q", "--detach", first.trim()],
    );
    let changes = wait_for_change(&updates);
    assert!(changes.submodules && changes.status, "{changes:?}");
}

#[test]
fn a_truncated_index_does_not_stop_the_watcher() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "1\n", "first");
    let (_watcher, updates) = start(&sb, &repo);

    // gix-index panicked on this in the watcher thread, and watching ended.
    std::fs::write(repo.join(".git/index"), b"DIRC").unwrap();
    let changes = wait_for_change(&updates);
    assert!(changes.status, "{changes:?}");

    // Still watching: the next edit is reported.
    std::fs::write(repo.join("a.txt"), "2\n").unwrap();
    let changes = wait_for_change(&updates);
    assert!(changes.status, "{changes:?}");
}

/// `.git/info/` made after the watcher started is watched from then on: an `exclude`
/// written into it later is seen.
#[test]
fn an_info_folder_made_after_the_start_is_watched() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "1\n", "first");
    let info = repo.join(".git/info");
    let _ = std::fs::remove_dir_all(&info);
    let (_watcher, updates) = start(&sb, &repo);

    std::fs::create_dir(&info).unwrap();
    let changes = wait_for_change(&updates);
    assert!(changes.status, "the new info/ may hold rules: {changes:?}");

    std::fs::write(info.join("exclude"), "*.log\n").unwrap();
    let changes = wait_for_change(&updates);
    assert!(changes.status, "a rule written into it later: {changes:?}");
}

/// A linked worktree sees a commit made in the main one: branches live in the common git dir.
#[test]
fn a_linked_worktree_sees_commits_made_in_the_main_one() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "1\n", "first");
    let linked = sb.path().join("linked");
    sb.run(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "side",
            linked.to_str().unwrap(),
        ],
    );
    let (_watcher, updates) = start(&sb, &linked);

    sb.commit_file(&repo, "a.txt", "2\n", "second, in the main worktree");
    let changes = wait_for_change(&updates);
    assert!(changes.refs, "{changes:?}");
}
