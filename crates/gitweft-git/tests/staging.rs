use gitweft_git::CancelToken;
use gitweft_git::gix::bstr::BString;
use gitweft_git::ops::{backup_worktree_file, discard_paths, stage_paths, unstage_paths};
use gitweft_git::read::{ChangeKind, EntryKind, Status, read_status};
use gitweft_testkit::Sandbox;

fn paths(list: &[&str]) -> Vec<BString> {
    list.iter().map(|p| BString::from(*p)).collect()
}

fn status(sb: &Sandbox, repo: &std::path::Path) -> Status {
    read_status(sb.git(), repo, &CancelToken::new()).unwrap()
}

fn entry<'a>(status: &'a Status, path: &str) -> &'a gitweft_git::read::StatusEntry {
    status.entries.iter().find(|e| e.path == path).unwrap()
}

#[test]
fn stage_and_unstage_modified_deleted_untracked_and_glob_named_files() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    std::fs::write(repo.join("mod.txt"), "1\n").unwrap();
    std::fs::write(repo.join("del.txt"), "x\n").unwrap();
    sb.run(&repo, &["add", "."]);
    sb.run(&repo, &["commit", "-qm", "base"]);

    std::fs::write(repo.join("mod.txt"), "2\n").unwrap();
    std::fs::remove_file(repo.join("del.txt")).unwrap();
    std::fs::write(repo.join("new file.txt"), "n\n").unwrap();
    std::fs::write(repo.join("*.txt"), "literal star\n").unwrap();
    std::fs::write(repo.join("untouched.txt"), "u\n").unwrap();

    let git = sb.git();
    let cancel = CancelToken::new();
    stage_paths(
        git,
        &repo,
        &paths(&["mod.txt", "del.txt", "new file.txt", "*.txt"]),
        &cancel,
    )
    .unwrap();
    let s = status(&sb, &repo);
    assert_eq!(entry(&s, "mod.txt").index, Some(ChangeKind::Modified));
    assert_eq!(entry(&s, "del.txt").index, Some(ChangeKind::Deleted));
    assert_eq!(entry(&s, "new file.txt").index, Some(ChangeKind::Added));
    assert_eq!(entry(&s, "*.txt").index, Some(ChangeKind::Added));
    // `*.txt` was taken literally, not as a glob.
    assert_eq!(entry(&s, "untouched.txt").kind, EntryKind::Untracked);

    unstage_paths(
        git,
        &repo,
        &paths(&["mod.txt", "del.txt", "new file.txt"]),
        false,
        &cancel,
    )
    .unwrap();
    let s = status(&sb, &repo);
    assert_eq!(entry(&s, "mod.txt").index, None);
    assert_eq!(entry(&s, "mod.txt").worktree, Some(ChangeKind::Modified));
    assert_eq!(entry(&s, "del.txt").worktree, Some(ChangeKind::Deleted));
    assert_eq!(entry(&s, "new file.txt").kind, EntryKind::Untracked);
    assert_eq!(s.staged().count(), 1, "only `*.txt` stays staged");
}

#[test]
fn unstage_a_rename_and_on_an_unborn_branch() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    std::fs::write(repo.join("old.txt"), "line\n".repeat(20)).unwrap();
    sb.run(&repo, &["add", "."]);
    sb.run(&repo, &["commit", "-qm", "base"]);
    sb.run(&repo, &["mv", "old.txt", "new.txt"]);

    unstage_paths(
        sb.git(),
        &repo,
        &paths(&["new.txt", "old.txt"]),
        false,
        &CancelToken::new(),
    )
    .unwrap();
    let s = status(&sb, &repo);
    assert_eq!(s.staged().count(), 0);
    assert_eq!(entry(&s, "old.txt").worktree, Some(ChangeKind::Deleted));

    let unborn = sb.init_repo("unborn");
    std::fs::write(unborn.join("a"), "1\n").unwrap();
    sb.run(&unborn, &["add", "a"]);
    unstage_paths(sb.git(), &unborn, &paths(&["a"]), true, &CancelToken::new()).unwrap();
    assert_eq!(entry(&status(&sb, &unborn), "a").kind, EntryKind::Untracked);
}

#[test]
fn discard_backs_up_before_throwing_away() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    std::fs::write(repo.join("keep.txt"), "committed\n").unwrap();
    sb.run(&repo, &["add", "."]);
    sb.run(&repo, &["commit", "-qm", "base"]);

    std::fs::write(repo.join("keep.txt"), "precious edit\n").unwrap();
    std::fs::create_dir(repo.join("dir")).unwrap();
    std::fs::write(repo.join("dir/scratch.txt"), "untracked work\n").unwrap();
    let trash = sb.path().join("trash");

    let backup = discard_paths(
        sb.git(),
        &repo,
        &paths(&["keep.txt"]),
        &paths(&["dir/scratch.txt"]),
        &trash,
        &CancelToken::new(),
    )
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(repo.join("keep.txt")).unwrap(),
        "committed\n"
    );
    assert!(!repo.join("dir/scratch.txt").exists());
    assert!(status(&sb, &repo).is_clean());

    // The discarded edit is still in the object database ...
    let (path, blob) = &backup.blobs[0];
    assert_eq!(path, "keep.txt");
    let content = sb.run(&repo, &["cat-file", "-p", &blob.to_string()]);
    assert_eq!(content, "precious edit\n");
    // ... and the untracked file sits in the trash entry's `files/`.
    let moved = backup.trash_dir.unwrap().join("files/dir/scratch.txt");
    assert_eq!(std::fs::read_to_string(moved).unwrap(), "untracked work\n");
}

#[test]
fn backup_worktree_file_saves_the_current_content() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    std::fs::write(repo.join("f.txt"), "work in progress\n").unwrap();
    let id = backup_worktree_file(
        sb.git(),
        &repo,
        &BString::from("f.txt"),
        &CancelToken::new(),
    )
    .unwrap()
    .expect("file exists");
    assert_eq!(
        sb.run(&repo, &["cat-file", "-p", &id.to_string()]),
        "work in progress\n"
    );
    let missing =
        backup_worktree_file(sb.git(), &repo, &BString::from("nope"), &CancelToken::new()).unwrap();
    assert!(missing.is_none());
}

/// A symlink's content is its target, as git stores a link (not the file it points at),
/// and a dangling one has content too.
#[cfg(unix)]
#[test]
fn backup_worktree_file_saves_a_symlinks_target() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    std::fs::write(repo.join("f.txt"), "the file's own text\n").unwrap();
    std::os::unix::fs::symlink("f.txt", repo.join("link")).unwrap();
    std::os::unix::fs::symlink("nowhere", repo.join("dangling")).unwrap();
    for (name, target) in [("link", "f.txt"), ("dangling", "nowhere")] {
        let id = backup_worktree_file(sb.git(), &repo, &BString::from(name), &CancelToken::new())
            .unwrap()
            .expect("the link exists");
        assert_eq!(sb.run(&repo, &["cat-file", "-p", &id.to_string()]), target);
        // The id git gives the link when it's added.
        sb.run(&repo, &["add", name]);
        let staged = sb.run(&repo, &["rev-parse", &format!(":{name}")]);
        assert_eq!(id.to_string(), staged.trim());
    }
}
