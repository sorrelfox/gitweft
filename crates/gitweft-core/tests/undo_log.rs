//! The shared undo log: writers merge instead of overwriting, and undoing a hard reset
//! puts back the untracked files it moved to the trash.

use std::path::Path;

use gitweft_core::oplog::{self, UndoHistory};
use gitweft_git::rewrite::{ResetMode, recorded_reset};
use gitweft_git::undo::{RefChange, UndoEntry, Worktree};
use gitweft_git::{CancelToken, ObjectId};
use gitweft_testkit::Sandbox;

fn entry(label: &str, byte: u8) -> UndoEntry {
    let id = ObjectId::from_bytes_or_panic(&[byte; 20]);
    let change = RefChange {
        name: format!("refs/tags/{label}"),
        before: None,
        after: Some(id),
    };
    UndoEntry::new(label.to_owned(), vec![change], Worktree::Follow).unwrap()
}

fn labels(stack: &[UndoEntry]) -> Vec<&str> {
    stack.iter().map(|entry| entry.label.as_str()).collect()
}

#[test]
fn two_writers_keep_each_others_steps() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("repos/r/oplog.jsonl");
    // Tab A and tab B each start from what they read, then write one step each.
    let a = oplog::update(&file, |h| h.push(entry("a", 1))).unwrap();
    let b = oplog::update(&file, |h| h.push(entry("b", 2))).unwrap();
    assert_eq!(labels(&a.undo), ["a"]);
    assert_eq!(labels(&b.undo), ["a", "b"]);

    // A undoes its own step (not the newest): only that one moves to redo.
    let after = oplog::update(&file, |h| {
        h.used(&entry("a", 1), false, Some(entry("a-back", 3)))
    })
    .unwrap();
    assert_eq!(labels(&after.undo), ["b"]);
    assert_eq!(labels(&after.redo), ["a-back"]);
    // B, not knowing, tries the same step: it's gone already, nothing else moves.
    let twice = oplog::update(&file, |h| h.used(&entry("a", 1), false, None)).unwrap();
    assert_eq!(twice, after);
    // A new step anywhere clears the redo stack.
    let pushed = oplog::update(&file, |h| h.push(entry("c", 4))).unwrap();
    assert_eq!(labels(&pushed.undo), ["b", "c"]);
    assert!(pushed.redo.is_empty());
    assert_eq!(oplog::load(&file), pushed);
}

#[test]
fn writers_in_threads_all_land() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("oplog.jsonl");
    std::thread::scope(|scope| {
        for n in 0..8u8 {
            let file = &file;
            scope.spawn(move || {
                for k in 0..5u8 {
                    oplog::update(file, |h| {
                        h.undo.push(entry(&format!("{n}-{k}"), n * 10 + k))
                    })
                    .unwrap();
                }
            });
        }
    });
    assert_eq!(oplog::load(&file).undo.len(), 40);
}

#[test]
fn a_read_only_update_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("oplog.jsonl");
    assert_eq!(
        oplog::update(&file, |_| {}).unwrap(),
        UndoHistory::default()
    );
    assert!(!file.exists());
}

fn read(repo: &Path, file: &str) -> String {
    std::fs::read_to_string(repo.join(file)).unwrap()
}

#[test]
fn undoing_a_hard_reset_brings_back_the_untracked_files_it_trashed() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "1\n", "first");
    sb.run(&repo, &["switch", "-q", "-c", "ahead"]);
    sb.commit_file(&repo, "t1.txt", "t1\n", "add t1");
    sb.commit_file(&repo, "dir/t2.txt", "t2\n", "add t2");
    sb.run(&repo, &["switch", "-q", "main"]);
    std::fs::write(repo.join("t1.txt"), "precious\n").unwrap();
    std::fs::create_dir(repo.join("dir")).unwrap();
    std::fs::write(repo.join("dir/t2.txt"), "also precious\n").unwrap();
    std::fs::write(repo.join("keep.txt"), "untouched\n").unwrap();
    let trash_root = sb.path().join("trash");
    let cancel = CancelToken::new();

    // Only untracked files in the way: no tracked changes to save.
    let (backup, entry) = recorded_reset(
        sb.git(),
        &repo,
        "ahead",
        ResetMode::Hard,
        &trash_root,
        "reset".into(),
        &cancel,
    )
    .unwrap();
    let entry = entry.unwrap();
    assert_eq!(entry.worktree, Worktree::Follow);
    assert_eq!(entry.trash, backup.trash_dir);
    assert_eq!(read(&repo, "t1.txt"), "t1\n");

    let done = oplog::apply(sb.git(), &repo, &entry, &trash_root, &cancel).unwrap();
    assert_eq!(done.left_in_trash, 0);
    assert_eq!(read(&repo, "t1.txt"), "precious\n");
    assert_eq!(read(&repo, "dir/t2.txt"), "also precious\n");
    assert_eq!(read(&repo, "keep.txt"), "untouched\n");
    assert_eq!(
        sb.run(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "main\n"
    );
    assert!(
        !backup.trash_dir.unwrap().exists(),
        "the emptied entry is removed"
    );
    // Redo must move them out of the way again (a plain `reset --keep` would refuse).
    assert_eq!(done.reverse.worktree, Worktree::Discard);

    let redone = oplog::apply(sb.git(), &repo, &done.reverse, &trash_root, &cancel).unwrap();
    assert_eq!(read(&repo, "t1.txt"), "t1\n");
    let again = redone.reverse.trash.clone().expect("a new trash entry");
    assert_eq!(read(&again.join("files"), "t1.txt"), "precious\n");

    // A file that can't go back (here: gone from the trash entry) is counted; the rest
    // still comes back.
    std::fs::remove_file(again.join("files/t1.txt")).unwrap();
    let partial = oplog::apply(sb.git(), &repo, &redone.reverse, &trash_root, &cancel).unwrap();
    assert_eq!(partial.left_in_trash, 1);
    assert_eq!(read(&repo, "dir/t2.txt"), "also precious\n");
    assert!(!repo.join("t1.txt").exists());
    assert!(again.exists(), "the entry keeps its record of t1.txt");
}
