use std::path::{Path, PathBuf};

use gitweft_git::merge::{Operation, OperationAction, operation_state, run_operation_action};
use gitweft_git::read::count_merges;
use gitweft_git::rewrite::{
    ResetMode, cherry_pick, cherry_pick_no_commit, commit_fixup, rebase_onto, recorded_reset,
    revert,
};
use gitweft_git::undo::{CHECKED_OUT, UndoEntry, Worktree, record};
use gitweft_git::{CancelToken, GitError, GitErrorKind, ObjectId};
use gitweft_testkit::Sandbox;

fn rev(sb: &Sandbox, repo: &Path, name: &str) -> String {
    sb.run(repo, &["rev-parse", name]).trim().to_owned()
}

fn id(sb: &Sandbox, repo: &Path, name: &str) -> ObjectId {
    ObjectId::from_hex(rev(sb, repo, name).as_bytes()).unwrap()
}

fn status(sb: &Sandbox, repo: &Path) -> String {
    sb.run(repo, &["status", "--porcelain", "--untracked-files=no"])
}

fn read(repo: &Path, file: &str) -> String {
    std::fs::read_to_string(repo.join(file)).unwrap()
}

fn trash(sb: &Sandbox) -> PathBuf {
    sb.path().join("trash")
}

fn undo(sb: &Sandbox, repo: &Path, entry: &UndoEntry) -> Result<UndoEntry, GitError> {
    gitweft_git::undo::undo(sb.git(), repo, entry, &trash(sb), &CancelToken::new())
}

fn reset(sb: &Sandbox, repo: &Path, target: &str, mode: ResetMode) -> Option<UndoEntry> {
    let cancel = CancelToken::new();
    let (_, entry) = recorded_reset(
        sb.git(),
        repo,
        target,
        mode,
        &trash(sb),
        "reset".into(),
        &cancel,
    )
    .unwrap();
    entry
}

#[test]
fn a_fixup_commit_names_its_target() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let cancel = CancelToken::new();
    sb.commit_file(&repo, "a.txt", "1\n", "one\n\nwith a body");
    let target = id(&sb, &repo, "HEAD");
    sb.commit_file(&repo, "b.txt", "b\n", "two");

    // Nothing staged: a clean tree, then a change that isn't staged.
    let err = commit_fixup(sb.git(), &repo, target, &cancel).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::NothingToCommit);
    std::fs::write(repo.join("a.txt"), "fixed\n").unwrap();
    let err = commit_fixup(sb.git(), &repo, target, &cancel).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::NothingToCommit);
    assert_eq!(rev(&sb, &repo, "HEAD~1"), target.to_string());

    sb.run(&repo, &["add", "a.txt"]);
    commit_fixup(sb.git(), &repo, target, &cancel).unwrap();
    assert_eq!(
        sb.run(&repo, &["log", "-1", "--format=%B"]).trim(),
        "fixup! one"
    );
    assert_eq!(status(&sb, &repo), "");
}

/// `main` with `a.txt` = 1, 2, 3 over three commits, and `topic` from the first commit
/// adding `t1.txt` and then `t2.txt`.
fn repo(sb: &Sandbox) -> PathBuf {
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "1\n", "one");
    sb.run(&repo, &["switch", "-q", "-c", "topic"]);
    sb.commit_file(&repo, "t1.txt", "t1\n", "topic one");
    sb.commit_file(&repo, "t2.txt", "t2\n", "topic two");
    sb.run(&repo, &["switch", "-q", "main"]);
    sb.commit_file(&repo, "a.txt", "2\n", "two");
    sb.commit_file(&repo, "a.txt", "3\n", "three");
    repo
}

#[test]
fn cherry_picked_commits_are_undone_and_redone() {
    let sb = Sandbox::new();
    let repo = repo(&sb);
    let cancel = CancelToken::new();
    let start = rev(&sb, &repo, "HEAD");
    let picks = [id(&sb, &repo, "topic~1"), id(&sb, &repo, "topic")];
    let (outcome, changes) = record(sb.git(), &repo, &[CHECKED_OUT], &cancel, || {
        cherry_pick(sb.git(), &repo, &picks, None, &cancel)
    })
    .unwrap();
    assert_eq!(outcome.skipped, 0);
    assert_eq!(
        sb.run(&repo, &["log", "--format=%s", "-3"]),
        "topic two\ntopic one\nthree\n"
    );
    let picked = rev(&sb, &repo, "HEAD");
    let entry = UndoEntry::new("cherry-pick".into(), changes, Worktree::Follow).unwrap();

    let redo = undo(&sb, &repo, &entry).unwrap();
    assert_eq!(rev(&sb, &repo, "HEAD"), start);
    assert!(!repo.join("t1.txt").exists());
    undo(&sb, &repo, &redo).unwrap();
    assert_eq!(rev(&sb, &repo, "HEAD"), picked);
    assert_eq!(read(&repo, "t2.txt"), "t2\n");
}

#[test]
fn a_cherry_pick_without_committing_stages_the_changes() {
    let sb = Sandbox::new();
    let repo = repo(&sb);
    let cancel = CancelToken::new();
    let start = rev(&sb, &repo, "HEAD");
    let picks = [id(&sb, &repo, "topic~1"), id(&sb, &repo, "topic")];
    cherry_pick_no_commit(sb.git(), &repo, &picks, None, &cancel).unwrap();
    assert_eq!(rev(&sb, &repo, "HEAD"), start);
    assert_eq!(status(&sb, &repo), "A  t1.txt\nA  t2.txt\n");
    let gix_repo = gitweft_git::gix::open(&repo).unwrap();
    assert_eq!(operation_state(&gix_repo), None);
}

#[test]
fn a_conflicting_pick_without_committing_leaves_only_the_conflict() {
    let sb = Sandbox::new();
    let repo = repo(&sb);
    let cancel = CancelToken::new();
    sb.run(&repo, &["switch", "-q", "topic"]);
    sb.commit_file(&repo, "a.txt", "topic\n", "topic edits a");
    let conflicting = [id(&sb, &repo, "HEAD")];
    sb.run(&repo, &["switch", "-q", "main"]);
    let start = rev(&sb, &repo, "HEAD");

    let err = cherry_pick_no_commit(sb.git(), &repo, &conflicting, None, &cancel).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::Conflict);
    assert_eq!(rev(&sb, &repo, "HEAD"), start);
    // One commit: no operation to continue, just the conflicted file.
    let gix_repo = gitweft_git::gix::open(&repo).unwrap();
    assert_eq!(operation_state(&gix_repo), None);
    assert_eq!(status(&sb, &repo), "UU a.txt\n");
}

#[test]
fn commits_already_on_the_branch_are_skipped() {
    let sb = Sandbox::new();
    let repo = repo(&sb);
    let cancel = CancelToken::new();
    // main gets topic's first change on its own.
    sb.commit_file(&repo, "t1.txt", "t1\n", "same as topic one");
    let start = rev(&sb, &repo, "HEAD");

    let only = [id(&sb, &repo, "topic~1")];
    let outcome = cherry_pick(sb.git(), &repo, &only, None, &cancel).unwrap();
    assert_eq!(outcome.skipped, 1);
    assert_eq!(rev(&sb, &repo, "HEAD"), start);
    let gix_repo = gitweft_git::gix::open(&repo).unwrap();
    assert_eq!(operation_state(&gix_repo), None);

    // In a sequence, the rest still goes on.
    let both = [id(&sb, &repo, "topic~1"), id(&sb, &repo, "topic")];
    let outcome = cherry_pick(sb.git(), &repo, &both, None, &cancel).unwrap();
    assert_eq!(outcome.skipped, 1);
    assert_eq!(sb.run(&repo, &["log", "--format=%s", "-1"]), "topic two\n");
    let gix_repo = gitweft_git::gix::open(&repo).unwrap();
    assert_eq!(operation_state(&gix_repo), None);
}

#[test]
fn a_conflicting_cherry_pick_stops_in_progress_and_records_nothing() {
    let sb = Sandbox::new();
    let repo = repo(&sb);
    let cancel = CancelToken::new();
    sb.run(&repo, &["switch", "-q", "topic"]);
    sb.commit_file(&repo, "a.txt", "topic\n", "topic edits a");
    let conflicting = [id(&sb, &repo, "HEAD")];
    sb.run(&repo, &["switch", "-q", "main"]);
    let start = rev(&sb, &repo, "HEAD");

    let err = record(sb.git(), &repo, &[CHECKED_OUT], &cancel, || {
        cherry_pick(sb.git(), &repo, &conflicting, None, &cancel)
    })
    .unwrap_err();
    assert_eq!(err.kind, GitErrorKind::Conflict);
    let gix_repo = gitweft_git::gix::open(&repo).unwrap();
    assert_eq!(operation_state(&gix_repo), Some(Operation::CherryPick));

    run_operation_action(
        sb.git(),
        &repo,
        Operation::CherryPick,
        OperationAction::Abort,
        &cancel,
    )
    .unwrap();
    assert_eq!(rev(&sb, &repo, "HEAD"), start);
    assert_eq!(status(&sb, &repo), "");
}

#[test]
fn merge_commits_are_picked_and_reverted_against_a_mainline() {
    let sb = Sandbox::new();
    let repo = repo(&sb);
    let cancel = CancelToken::new();
    sb.run(&repo, &["switch", "-q", "-c", "merged", "topic~2"]);
    sb.run(
        &repo,
        &["merge", "-q", "--no-ff", "-m", "merge topic", "topic"],
    );
    let merge = [id(&sb, &repo, "HEAD")];
    sb.run(&repo, &["switch", "-q", "main"]);

    let err = cherry_pick(sb.git(), &repo, &merge, None, &cancel).unwrap_err();
    assert!(err.stderr.contains("is a merge"), "{}", err.stderr);
    cherry_pick(sb.git(), &repo, &merge, Some(1), &cancel).unwrap();
    assert_eq!(read(&repo, "t1.txt"), "t1\n");
    assert_eq!(read(&repo, "t2.txt"), "t2\n");

    revert(sb.git(), &repo, &[id(&sb, &repo, "HEAD")], None, &cancel).unwrap();
    assert!(!repo.join("t2.txt").exists());
}

#[test]
fn a_revert_is_a_new_commit_that_undo_removes() {
    let sb = Sandbox::new();
    let repo = repo(&sb);
    let cancel = CancelToken::new();
    let start = rev(&sb, &repo, "HEAD");
    let second = [id(&sb, &repo, "HEAD~1")];
    // Reverting "two" conflicts with "three", which changed the same line.
    let err = revert(sb.git(), &repo, &second, None, &cancel).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::Conflict);
    run_operation_action(
        sb.git(),
        &repo,
        Operation::Revert,
        OperationAction::Abort,
        &cancel,
    )
    .unwrap();

    let last = [id(&sb, &repo, "HEAD")];
    let (outcome, changes) = record(sb.git(), &repo, &[CHECKED_OUT], &cancel, || {
        revert(sb.git(), &repo, &last, None, &cancel)
    })
    .unwrap();
    assert_eq!(outcome.skipped, 0);
    assert_eq!(read(&repo, "a.txt"), "2\n");
    assert_eq!(
        sb.run(&repo, &["log", "--format=%s", "-1"]),
        "Revert \"three\"\n"
    );
    let entry = UndoEntry::new("revert".into(), changes, Worktree::Follow).unwrap();
    undo(&sb, &repo, &entry).unwrap();
    assert_eq!(rev(&sb, &repo, "HEAD"), start);
    assert_eq!(read(&repo, "a.txt"), "3\n");
}

#[test]
fn a_soft_reset_leaves_the_changes_staged_and_undo_moves_the_branch_back() {
    let sb = Sandbox::new();
    let repo = repo(&sb);
    let start = rev(&sb, &repo, "HEAD");
    let entry = reset(&sb, &repo, "HEAD~2", ResetMode::Soft).unwrap();
    assert_eq!(status(&sb, &repo), "M  a.txt\n");

    let redo = undo(&sb, &repo, &entry).unwrap();
    assert_eq!(rev(&sb, &repo, "HEAD"), start);
    assert_eq!(status(&sb, &repo), "");
    undo(&sb, &repo, &redo).unwrap();
    assert_eq!(status(&sb, &repo), "M  a.txt\n");
}

#[test]
fn undoing_a_mixed_reset_restores_the_index_exactly() {
    let sb = Sandbox::new();
    let repo = repo(&sb);
    let start = rev(&sb, &repo, "HEAD");
    // Staged and unstaged versions of one file, plus a staged new file.
    std::fs::write(repo.join("a.txt"), "staged\n").unwrap();
    std::fs::write(repo.join("new.txt"), "new\n").unwrap();
    sb.run(&repo, &["add", "a.txt", "new.txt"]);
    std::fs::write(repo.join("a.txt"), "unstaged\n").unwrap();
    let before = status(&sb, &repo);
    assert_eq!(before, "MM a.txt\nA  new.txt\n");

    let entry = reset(&sb, &repo, "HEAD~1", ResetMode::Mixed).unwrap();
    assert!(matches!(entry.worktree, Worktree::RestoreIndex(_)));
    assert_eq!(read(&repo, "a.txt"), "unstaged\n");
    let after = status(&sb, &repo);
    assert_eq!(after, " M a.txt\n");

    let redo = undo(&sb, &repo, &entry).unwrap();
    assert_eq!(rev(&sb, &repo, "HEAD"), start);
    assert_eq!(status(&sb, &repo), before);
    assert_eq!(sb.run(&repo, &["show", ":a.txt"]), "staged\n");
    let again = undo(&sb, &repo, &redo).unwrap();
    assert_eq!(status(&sb, &repo), after);
    undo(&sb, &repo, &again).unwrap();
    assert_eq!(status(&sb, &repo), before);

    // Something staged meanwhile would be lost: refused, nothing changes.
    let entry = reset(&sb, &repo, "HEAD", ResetMode::Mixed).unwrap();
    sb.run(&repo, &["add", "a.txt"]);
    let err = undo(&sb, &repo, &entry).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::WouldOverwrite);
    assert_eq!(status(&sb, &repo), "M  a.txt\n");
}

#[test]
fn resets_that_change_nothing_record_nothing() {
    let sb = Sandbox::new();
    let repo = repo(&sb);
    assert_eq!(reset(&sb, &repo, "HEAD", ResetMode::Soft), None);
    assert_eq!(reset(&sb, &repo, "HEAD", ResetMode::Mixed), None);
    assert_eq!(reset(&sb, &repo, "HEAD", ResetMode::Hard), None);
    // Unstaging everything is a change.
    std::fs::write(repo.join("a.txt"), "x\n").unwrap();
    sb.run(&repo, &["add", "a.txt"]);
    let entry = reset(&sb, &repo, "HEAD", ResetMode::Mixed).unwrap();
    assert_eq!(status(&sb, &repo), " M a.txt\n");
    undo(&sb, &repo, &entry).unwrap();
    assert_eq!(status(&sb, &repo), "M  a.txt\n");
}

#[test]
fn a_hard_reset_saves_what_it_throws_away_and_undo_brings_it_back() {
    let sb = Sandbox::new();
    let repo = repo(&sb);
    let start = rev(&sb, &repo, "HEAD");
    std::fs::write(repo.join("a.txt"), "staged\n").unwrap();
    std::fs::write(repo.join("new.txt"), "new\n").unwrap();
    sb.run(&repo, &["add", "a.txt", "new.txt"]);
    std::fs::write(repo.join("a.txt"), "unstaged\n").unwrap();
    // Untracked things where topic's files go: a file, and a folder where a file goes.
    std::fs::write(repo.join("t1.txt"), "precious\n").unwrap();
    std::fs::create_dir(repo.join("t2.txt")).unwrap();
    std::fs::write(repo.join("t2.txt/inner"), "also precious\n").unwrap();
    std::fs::write(repo.join("elsewhere.txt"), "untouched\n").unwrap();
    let before = status(&sb, &repo);

    let cancel = CancelToken::new();
    let (backup, entry) = recorded_reset(
        sb.git(),
        &repo,
        "topic",
        ResetMode::Hard,
        &trash(&sb),
        "reset".into(),
        &cancel,
    )
    .unwrap();
    let entry = entry.unwrap();
    assert_eq!(rev(&sb, &repo, "HEAD"), rev(&sb, &repo, "topic"));
    assert_eq!(status(&sb, &repo), "");
    assert_eq!(read(&repo, "t1.txt"), "t1\n");
    assert_eq!(read(&repo, "elsewhere.txt"), "untouched\n");
    let trash_dir = backup.trash_dir.unwrap();
    let saved = trash_dir.join("files");
    assert_eq!(read(&saved, "t1.txt"), "precious\n");
    assert_eq!(read(&saved, "t2.txt/inner"), "also precious\n");
    assert_eq!(
        entry.worktree,
        Worktree::RestoreChanges(backup.changes.unwrap())
    );
    // The entry names the trash entry, for the caller to restore after the undo.
    assert_eq!(entry.trash.as_ref(), Some(&trash_dir));

    let redo = undo(&sb, &repo, &entry).unwrap();
    assert_eq!(redo.worktree, Worktree::Discard);
    assert_eq!(redo.trash, None);
    // The tracked files the reset brought are gone again; the trashed ones wait.
    assert!(!repo.join("t1.txt").exists());
    assert_eq!(rev(&sb, &repo, "HEAD"), start);
    assert_eq!(status(&sb, &repo), before);
    assert_eq!(read(&repo, "a.txt"), "unstaged\n");
    assert_eq!(sb.run(&repo, &["show", ":a.txt"]), "staged\n");

    // Redo throws the changes away again, saving them anew.
    let again = undo(&sb, &repo, &redo).unwrap();
    assert!(matches!(again.worktree, Worktree::RestoreChanges(_)));
    assert_eq!(
        again.trash, None,
        "nothing untracked was in the way this time"
    );
    assert_eq!(status(&sb, &repo), "");
    undo(&sb, &repo, &again).unwrap();
    assert_eq!(status(&sb, &repo), before);
}

#[test]
fn undoing_a_hard_reset_waits_for_a_clean_tree() {
    let sb = Sandbox::new();
    let repo = repo(&sb);
    std::fs::write(repo.join("a.txt"), "mine\n").unwrap();
    let entry = reset(&sb, &repo, "HEAD~1", ResetMode::Hard).unwrap();
    let reset_to = rev(&sb, &repo, "HEAD");
    std::fs::write(repo.join("a.txt"), "edited since\n").unwrap();

    let err = undo(&sb, &repo, &entry).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::WouldOverwrite);
    assert_eq!(rev(&sb, &repo, "HEAD"), reset_to);
    assert_eq!(read(&repo, "a.txt"), "edited since\n");

    sb.run(&repo, &["checkout", "--", "a.txt"]);
    undo(&sb, &repo, &entry).unwrap();
    assert_eq!(read(&repo, "a.txt"), "mine\n");
}

#[test]
fn mixed_and_hard_resets_refuse_during_conflicts() {
    let sb = Sandbox::new();
    let repo = repo(&sb);
    let cancel = CancelToken::new();
    sb.run(&repo, &["switch", "-q", "topic"]);
    sb.commit_file(&repo, "a.txt", "topic\n", "topic edits a");
    let conflicting = [id(&sb, &repo, "HEAD")];
    sb.run(&repo, &["switch", "-q", "main"]);
    cherry_pick(sb.git(), &repo, &conflicting, None, &cancel).unwrap_err();
    let head = rev(&sb, &repo, "HEAD");

    for mode in [ResetMode::Mixed, ResetMode::Hard] {
        let err = recorded_reset(
            sb.git(),
            &repo,
            "HEAD~1",
            mode,
            &trash(&sb),
            "reset".into(),
            &cancel,
        )
        .unwrap_err();
        assert_eq!(err.kind, GitErrorKind::UnresolvedConflicts);
        assert_eq!(rev(&sb, &repo, "HEAD"), head);
    }
}

/// A branch rebased onto one whose name isn't UTF-8, named by its bytes.
#[test]
fn a_branch_is_rebased_onto_one_whose_name_isnt_utf8() {
    use std::os::unix::ffi::OsStrExt as _;
    let sb = Sandbox::new();
    let repo = repo(&sb);
    let cancel = CancelToken::new();
    sb.git()
        .command(&repo)
        .args(["branch", "-m", "main"])
        .arg(std::ffi::OsStr::from_bytes(b"caf\xe9"))
        .run(&cancel)
        .unwrap();
    sb.run(&repo, &["switch", "-q", "topic"]);
    let onto = std::ffi::OsStr::from_bytes(b"refs/heads/caf\xe9");
    rebase_onto(sb.git(), &repo, onto, false, &cancel).unwrap();
    assert_eq!(
        sb.run(&repo, &["log", "--format=%s", "-4"]),
        "topic two\ntopic one\nthree\ntwo\n"
    );
}

/// A branch rebased onto another: its own commits on top, still checked out; the recorded
/// rebase is undone and redone, and a branch already on top stays where it is.
#[test]
fn a_branch_is_rebased_onto_another_and_the_rebase_undone() {
    let sb = Sandbox::new();
    let repo = repo(&sb);
    let cancel = CancelToken::new();
    sb.run(&repo, &["switch", "-q", "topic"]);
    let before = rev(&sb, &repo, "topic");
    let ((), changes) = record(sb.git(), &repo, &[CHECKED_OUT], &cancel, || {
        rebase_onto(sb.git(), &repo, "refs/heads/main", false, &cancel)
    })
    .unwrap();
    assert_eq!(
        sb.run(&repo, &["log", "--format=%s", "-4"]),
        "topic two\ntopic one\nthree\ntwo\n"
    );
    assert_eq!(
        sb.run(&repo, &["symbolic-ref", "--short", "HEAD"]),
        "topic\n"
    );
    assert_eq!(read(&repo, "a.txt"), "3\n");
    let rebased = rev(&sb, &repo, "topic");
    let entry = UndoEntry::new("rebase".into(), changes, Worktree::Follow).unwrap();

    let redo = undo(&sb, &repo, &entry).unwrap();
    assert_eq!(rev(&sb, &repo, "topic"), before);
    assert_eq!(read(&repo, "a.txt"), "1\n");
    assert_eq!(status(&sb, &repo), "");
    undo(&sb, &repo, &redo).unwrap();
    assert_eq!(rev(&sb, &repo, "topic"), rebased);

    rebase_onto(sb.git(), &repo, "refs/heads/main", false, &cancel).unwrap();
    assert_eq!(rev(&sb, &repo, "topic"), rebased);
}

#[test]
fn a_conflicting_rebase_stops_for_the_banner() {
    let sb = Sandbox::new();
    let repo = repo(&sb);
    let cancel = CancelToken::new();
    sb.run(&repo, &["switch", "-q", "topic"]);
    sb.commit_file(&repo, "a.txt", "topic's\n", "topic changes a");
    let err = rebase_onto(sb.git(), &repo, "refs/heads/main", false, &cancel).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::Conflict);
    let gix_repo = gitweft_git::gix::open(&repo).unwrap();
    assert_eq!(operation_state(&gix_repo), Some(Operation::Rebase));
}

/// A branch with a merge in it, rebased onto main: `--rebase-merges` recreates the merge,
/// a plain rebase flattens it into a line of the branch's commits.
#[test]
fn a_rebase_keeps_the_branchs_merges_only_when_asked() {
    let sb = Sandbox::new();
    let repo = repo(&sb);
    let cancel = CancelToken::new();
    sb.run(&repo, &["switch", "-q", "-c", "side", "topic"]);
    sb.commit_file(&repo, "s.txt", "s\n", "side one");
    sb.run(&repo, &["switch", "-q", "topic"]);
    sb.commit_file(&repo, "t3.txt", "t3\n", "topic three");
    sb.run(
        &repo,
        &["merge", "-q", "--no-ff", "-m", "merge side", "side"],
    );
    let before = rev(&sb, &repo, "topic");
    let merges = || {
        count_merges(
            sb.git(),
            &repo,
            "refs/heads/main",
            "refs/heads/topic",
            &cancel,
        )
        .unwrap()
    };
    let subjects = || {
        let log = sb.run(&repo, &["log", "--format=%s", "refs/heads/main..topic"]);
        let mut subjects: Vec<String> = log.lines().map(str::to_owned).collect();
        subjects.sort();
        subjects
    };
    assert_eq!(merges(), 1);

    rebase_onto(sb.git(), &repo, "refs/heads/main", true, &cancel).unwrap();
    sb.run(&repo, &["merge-base", "--is-ancestor", "main", "topic"]);
    assert_eq!(merges(), 1);
    let parents = sb.run(&repo, &["show", "-s", "--format=%P", "topic"]);
    assert_eq!(parents.split_whitespace().count(), 2, "{parents}");
    assert_eq!(
        subjects(),
        [
            "merge side",
            "side one",
            "topic one",
            "topic three",
            "topic two"
        ]
    );

    sb.run(&repo, &["reset", "-q", "--hard", &before]);
    rebase_onto(sb.git(), &repo, "refs/heads/main", false, &cancel).unwrap();
    sb.run(&repo, &["merge-base", "--is-ancestor", "main", "topic"]);
    assert_eq!(merges(), 0);
    assert_eq!(
        subjects(),
        ["side one", "topic one", "topic three", "topic two"]
    );
}
