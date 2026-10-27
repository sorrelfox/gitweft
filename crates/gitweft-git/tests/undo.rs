use std::path::{Path, PathBuf};

use gitweft_git::branch::{create_branch, delete_branch};
use gitweft_git::merge::{MergeMode, merge};
use gitweft_git::tag::create_tag;
use gitweft_git::undo::{CHECKED_OUT, UndoEntry, Worktree, record, record_branch};
use gitweft_git::{CancelToken, GitError, GitErrorKind};
use gitweft_testkit::Sandbox;

fn rev(sb: &Sandbox, repo: &Path, name: &str) -> String {
    sb.run(repo, &["rev-parse", name]).trim().to_owned()
}

fn status(sb: &Sandbox, repo: &Path) -> String {
    sb.run(repo, &["status", "--porcelain"])
}

fn undo(sb: &Sandbox, repo: &Path, entry: &UndoEntry) -> Result<UndoEntry, GitError> {
    let trash = sb.path().join("trash");
    gitweft_git::undo::undo(sb.git(), repo, entry, &trash, &CancelToken::new())
}

/// A repository with two commits on `main`.
fn repo(sb: &Sandbox) -> PathBuf {
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "1\n", "first");
    sb.commit_file(&repo, "a.txt", "2\n", "second");
    repo
}

/// Commits `a.txt` with `content`, recording it like the app does.
fn recorded_commit(sb: &Sandbox, repo: &Path, content: &str) -> UndoEntry {
    let cancel = CancelToken::new();
    let ((), changes) = record(sb.git(), repo, &[CHECKED_OUT], &cancel, || {
        sb.commit_file(repo, "a.txt", content, "third");
        Ok(())
    })
    .unwrap();
    UndoEntry::new("commit".into(), changes, Worktree::Soft).unwrap()
}

#[test]
fn undoing_a_commit_keeps_its_changes_staged_and_redo_restores_it() {
    let sb = Sandbox::new();
    let repo = repo(&sb);
    let second = rev(&sb, &repo, "HEAD");
    let entry = recorded_commit(&sb, &repo, "3\n");
    let third = rev(&sb, &repo, "HEAD");
    assert_eq!(entry.changes.len(), 1);
    assert_eq!(entry.changes[0].name, "refs/heads/main");

    let redo = undo(&sb, &repo, &entry).unwrap();
    assert_eq!(rev(&sb, &repo, "HEAD"), second);
    assert_eq!(status(&sb, &repo), "M  a.txt\n");
    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "3\n");

    let again = undo(&sb, &repo, &redo).unwrap();
    assert_eq!(rev(&sb, &repo, "HEAD"), third);
    assert_eq!(status(&sb, &repo), "");
    assert_eq!(again, entry);
}

#[test]
fn undo_refuses_when_the_branch_moved_since() {
    let sb = Sandbox::new();
    let repo = repo(&sb);
    let entry = recorded_commit(&sb, &repo, "3\n");
    // Another commit, e.g. from a terminal: undoing the first would drop it.
    sb.commit_file(&repo, "a.txt", "4\n", "fourth");
    let fourth = rev(&sb, &repo, "HEAD");

    let err = undo(&sb, &repo, &entry).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::RefMoved);
    assert!(
        err.stderr.contains("main has moved since"),
        "{}",
        err.stderr
    );
    assert_eq!(rev(&sb, &repo, "HEAD"), fourth);
    assert_eq!(status(&sb, &repo), "");
}

#[test]
fn undoing_a_merge_moves_the_files_back_but_keeps_local_changes() {
    let sb = Sandbox::new();
    let repo = repo(&sb);
    let cancel = CancelToken::new();
    let before = rev(&sb, &repo, "HEAD");
    sb.run(&repo, &["switch", "-q", "-c", "topic"]);
    sb.commit_file(&repo, "b.txt", "topic\n", "topic work");
    sb.run(&repo, &["switch", "-q", "main"]);
    std::fs::write(repo.join("a.txt"), "local edit\n").unwrap();

    let (_, changes) = record(sb.git(), &repo, &[CHECKED_OUT], &cancel, || {
        merge(sb.git(), &repo, "topic", MergeMode::NoFastForward, &cancel)
    })
    .unwrap();
    let entry = UndoEntry::new("merge".into(), changes, Worktree::Follow).unwrap();
    assert!(repo.join("b.txt").exists());

    undo(&sb, &repo, &entry).unwrap();
    assert_eq!(rev(&sb, &repo, "HEAD"), before);
    assert!(!repo.join("b.txt").exists());
    assert_eq!(
        std::fs::read_to_string(repo.join("a.txt")).unwrap(),
        "local edit\n"
    );
}

#[test]
fn deleted_branches_and_created_tags_come_back_and_go_again() {
    let sb = Sandbox::new();
    let repo = repo(&sb);
    let cancel = CancelToken::new();
    create_branch(sb.git(), &repo, "wip", Some("HEAD~1"), false, &cancel).unwrap();
    let wip = rev(&sb, &repo, "wip");

    let ((), changes) = record(sb.git(), &repo, &["refs/heads/wip"], &cancel, || {
        delete_branch(sb.git(), &repo, "wip", true, &cancel)
    })
    .unwrap();
    let deleted = UndoEntry::new("delete".into(), changes, Worktree::Follow).unwrap();
    let ((), changes) = record(sb.git(), &repo, &["refs/tags/v1"], &cancel, || {
        create_tag(sb.git(), &repo, "v1", "HEAD", Some("Release"), &cancel)
    })
    .unwrap();
    let tagged = UndoEntry::new("tag".into(), changes, Worktree::Follow).unwrap();
    let tag_object = rev(&sb, &repo, "refs/tags/v1");

    let redo_delete = undo(&sb, &repo, &deleted).unwrap();
    assert_eq!(rev(&sb, &repo, "wip"), wip);
    let redo_tag = undo(&sb, &repo, &tagged).unwrap();
    assert_eq!(sb.run(&repo, &["tag"]), "");

    // Redo: the annotated tag comes back as the same tag object.
    undo(&sb, &repo, &redo_tag).unwrap();
    assert_eq!(rev(&sb, &repo, "refs/tags/v1"), tag_object);
    undo(&sb, &repo, &redo_delete).unwrap();
    assert!(!sb.run(&repo, &["branch"]).contains("wip"));
    // The branch is gone now, so the delete can't be redone twice.
    let err = undo(&sb, &repo, &redo_delete).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::RefMoved);
}

#[test]
fn nothing_is_recorded_when_nothing_moved_and_the_checked_out_branch_is_never_deleted() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let cancel = CancelToken::new();
    let (_, changes) = record(sb.git(), &repo, &[CHECKED_OUT], &cancel, || Ok(())).unwrap();
    assert!(changes.is_empty());

    // The first commit on an unborn branch: undoing would delete the checked-out branch.
    let ((), changes) = record(sb.git(), &repo, &[CHECKED_OUT], &cancel, || {
        sb.commit_file(&repo, "a.txt", "1\n", "first");
        Ok(())
    })
    .unwrap();
    assert_eq!(changes[0].before, None);
    let entry = UndoEntry::new("commit".into(), changes, Worktree::Soft).unwrap();
    let err = undo(&sb, &repo, &entry).unwrap_err();
    assert!(err.stderr.contains("checked out"), "{}", err.stderr);
    assert_eq!(status(&sb, &repo), "");
}

#[test]
fn a_detached_head_commit_is_undone_on_head_itself() {
    let sb = Sandbox::new();
    let repo = repo(&sb);
    sb.run(&repo, &["switch", "-q", "--detach", "HEAD~1"]);
    let start = rev(&sb, &repo, "HEAD");
    let entry = recorded_commit(&sb, &repo, "detached\n");
    assert_eq!(entry.changes[0].name, "HEAD");

    let redo = undo(&sb, &repo, &entry).unwrap();
    assert_eq!(rev(&sb, &repo, "HEAD"), start);
    assert_eq!(status(&sb, &repo), "M  a.txt\n");

    // Once HEAD is on a branch again, "HEAD" means that branch: refuse.
    sb.run(&repo, &["stash", "-q"]);
    sb.run(&repo, &["switch", "-q", "main"]);
    let err = undo(&sb, &repo, &redo).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::RefMoved);
}

#[test]
fn a_deleted_branch_comes_back_with_its_upstream_and_goes_again_without_it() {
    let sb = Sandbox::new();
    let repo = repo(&sb);
    let cancel = CancelToken::new();
    create_branch(sb.git(), &repo, "a.b", None, false, &cancel).unwrap();
    // Set by hand, as `push -u` would, plus one of a neighbour whose name shares a prefix.
    sb.run(&repo, &["config", "branch.a.b.remote", "origin"]);
    sb.run(&repo, &["config", "branch.a.b.merge", "refs/heads/a.b"]);
    sb.run(&repo, &["config", "branch.a.b.description", "two\nlines"]);
    sb.run(&repo, &["config", "branch.a.b.c.remote", "other"]);
    let config = |key: &str| {
        sb.git()
            .command(&repo)
            .args(["config", "--get", key])
            .allow_exit_codes(&[1])
            .read_only()
            .run(&cancel)
            .unwrap()
            .stdout_str()
            .into_owned()
    };

    let ((), changes, kept) = record_branch(sb.git(), &repo, "refs/heads/a.b", &cancel, || {
        delete_branch(sb.git(), &repo, "a.b", true, &cancel)
    })
    .unwrap();
    let kept = kept.unwrap();
    assert_eq!(
        kept.entries,
        [
            ("branch.a.b.remote".to_owned(), "origin".to_owned()),
            ("branch.a.b.merge".to_owned(), "refs/heads/a.b".to_owned()),
            ("branch.a.b.description".to_owned(), "two\nlines".to_owned()),
        ]
    );
    assert_eq!(config("branch.a.b.remote"), "");
    let mut deleted = UndoEntry::new("delete".into(), changes, Worktree::Follow).unwrap();
    deleted.branch_config.push(kept);

    let redo = undo(&sb, &repo, &deleted).unwrap();
    assert_eq!(config("branch.a.b.remote"), "origin\n");
    assert_eq!(config("branch.a.b.merge"), "refs/heads/a.b\n");
    assert_eq!(config("branch.a.b.description"), "two\nlines\n");
    assert_eq!(config("branch.a.b.c.remote"), "other\n");
    assert_eq!(redo.branch_config, deleted.branch_config);

    // Redo deletes it again, config and all, as `git branch -d` did; the neighbour stays.
    let again = undo(&sb, &repo, &redo).unwrap();
    assert!(!sb.run(&repo, &["branch"]).contains("a.b\n"));
    assert_eq!(config("branch.a.b.remote"), "");
    assert_eq!(config("branch.a.b.c.remote"), "other\n");
    undo(&sb, &repo, &again).unwrap();
    assert_eq!(config("branch.a.b.remote"), "origin\n");
}

#[test]
fn a_config_value_that_isnt_utf8_comes_back_byte_for_byte() {
    use gitweft_git::branch::{config_value_bytes, config_value_text};
    use std::os::unix::ffi::OsStrExt as _;
    let sb = Sandbox::new();
    let repo = repo(&sb);
    let cancel = CancelToken::new();
    // A description in Latin-1, as an old repository may have.
    let latin1: &[u8] = b"caf\xe9";
    sb.run(&repo, &["branch", "latin"]);
    sb.git()
        .command(&repo)
        .args(["config", "branch.latin.description"])
        .arg(std::ffi::OsStr::from_bytes(latin1))
        .run(&cancel)
        .unwrap();
    let description = || {
        sb.git()
            .command(&repo)
            .args(["config", "--get", "branch.latin.description"])
            .allow_exit_codes(&[1])
            .read_only()
            .run(&cancel)
            .unwrap()
            .stdout
    };

    let ((), changes, kept) = record_branch(sb.git(), &repo, "refs/heads/latin", &cancel, || {
        delete_branch(sb.git(), &repo, "latin", true, &cancel)
    })
    .unwrap();
    let kept = kept.unwrap();
    // Kept, not left out (#16): encoded, since the undo log holds text.
    assert_eq!(kept.entries.len(), 1, "{:?}", kept.entries);
    assert_eq!(config_value_bytes(&kept.entries[0].1), latin1);
    assert!(description().is_empty());

    let mut deleted = UndoEntry::new("delete".into(), changes, Worktree::Follow).unwrap();
    deleted.branch_config.push(kept);
    undo(&sb, &repo, &deleted).unwrap();
    assert_eq!(description(), b"caf\xe9\n");

    // The encoding leaves UTF-8 as it is.
    assert_eq!(config_value_text("café".as_bytes()), "café");
    assert_eq!(config_value_bytes("café"), "café".as_bytes());
}

#[test]
fn deleting_a_branch_whose_name_isnt_utf8_is_undone() {
    use gitweft_git::undo::ref_name_text;
    use std::os::unix::ffi::OsStrExt as _;
    let sb = Sandbox::new();
    let repo = repo(&sb);
    let cancel = CancelToken::new();
    let name = std::ffi::OsStr::from_bytes(b"caf\xe9");
    sb.git()
        .command(&repo)
        .arg("branch")
        .arg(name)
        .run(&cancel)
        .unwrap();
    let tip = rev(&sb, &repo, "main");
    let at = || {
        sb.git()
            .command(&repo)
            .args(["rev-parse", "--quiet", "--verify"])
            .arg(std::ffi::OsStr::from_bytes(b"refs/heads/caf\xe9"))
            .allow_exit_codes(&[1])
            .read_only()
            .run(&cancel)
            .unwrap()
            .stdout_str()
            .trim()
            .to_owned()
    };

    // The name as the undo log keeps it (#18); before, the text "caf\u{fffd}" named no ref.
    let full = ref_name_text(b"refs/heads/caf\xe9");
    let ((), changes, _) = record_branch(sb.git(), &repo, &full, &cancel, || {
        delete_branch(sb.git(), &repo, name, true, &cancel)
    })
    .unwrap();
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(at(), "");

    let deleted = UndoEntry::new("delete".into(), changes, Worktree::Follow).unwrap();
    undo(&sb, &repo, &deleted).unwrap();
    assert_eq!(at(), tip);
}

#[test]
fn undoing_a_branch_created_with_an_upstream_removes_its_config() {
    let sb = Sandbox::new();
    let repo = repo(&sb);
    let cancel = CancelToken::new();
    let ((), changes, kept) = record_branch(sb.git(), &repo, "refs/heads/track", &cancel, || {
        create_branch(sb.git(), &repo, "track", Some("main"), false, &cancel)?;
        sb.run(&repo, &["branch", "--set-upstream-to=main", "track"]);
        Ok(())
    })
    .unwrap();
    let mut created = UndoEntry::new("create".into(), changes, Worktree::Follow).unwrap();
    created.branch_config.extend(kept);
    assert_eq!(created.branch_config[0].entries.len(), 2);

    let redo = undo(&sb, &repo, &created).unwrap();
    assert_eq!(
        sb.run(&repo, &["config", "--local", "--list"])
            .matches("branch.track")
            .count(),
        0
    );
    undo(&sb, &repo, &redo).unwrap();
    assert_eq!(
        sb.run(&repo, &["config", "branch.track.merge"]),
        "refs/heads/main\n"
    );

    // A branch without config keeps none.
    let (_, _, none) = record_branch(sb.git(), &repo, "refs/heads/plain", &cancel, || {
        create_branch(sb.git(), &repo, "plain", None, false, &cancel)
    })
    .unwrap();
    assert!(none.is_none());
}
