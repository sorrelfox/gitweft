use gitweft_git::CancelToken;
use gitweft_git::read::{ChangeKind, EntryKind, read_status};
use gitweft_testkit::Sandbox;

#[test]
fn reports_staged_unstaged_untracked_and_renames() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let body = "line\n".repeat(20);
    std::fs::write(repo.join("tracked.txt"), "1\n").unwrap();
    std::fs::write(repo.join("both.txt"), "1\n").unwrap();
    std::fs::write(repo.join("gone.txt"), "x\n").unwrap();
    std::fs::write(repo.join("old name.txt"), &body).unwrap();
    sb.run(&repo, &["add", "."]);
    sb.run(&repo, &["commit", "-q", "-m", "base"]);

    std::fs::write(repo.join("tracked.txt"), "2\n").unwrap(); // unstaged
    std::fs::write(repo.join("both.txt"), "2\n").unwrap();
    sb.run(&repo, &["add", "both.txt"]);
    std::fs::write(repo.join("both.txt"), "3\n").unwrap(); // staged + unstaged
    std::fs::remove_file(repo.join("gone.txt")).unwrap(); // unstaged delete
    sb.run(&repo, &["mv", "old name.txt", "new name.txt"]); // staged rename
    std::fs::write(repo.join("added.txt"), "new\n").unwrap();
    sb.run(&repo, &["add", "added.txt"]); // staged add
    std::fs::create_dir(repo.join("dir")).unwrap();
    std::fs::write(repo.join("dir/untracked.txt"), "?\n").unwrap();

    let status = read_status(sb.git(), &repo, &CancelToken::new()).unwrap();
    let find = |path: &str| {
        status
            .entries
            .iter()
            .find(|e| e.path == path)
            .unwrap_or_else(|| panic!("{path} missing from {status:?}"))
    };

    assert_eq!(status.branch.head.as_deref(), Some("main"));
    assert!(status.branch.oid.is_some());
    assert_eq!(find("tracked.txt").worktree, Some(ChangeKind::Modified));
    assert_eq!(find("tracked.txt").index, None);
    let both = find("both.txt");
    assert_eq!(
        (both.index, both.worktree),
        (Some(ChangeKind::Modified), Some(ChangeKind::Modified))
    );
    assert_eq!(find("gone.txt").worktree, Some(ChangeKind::Deleted));
    let renamed = find("new name.txt");
    assert_eq!(renamed.index, Some(ChangeKind::Renamed));
    assert_eq!(renamed.orig_path.as_ref().unwrap(), "old name.txt");
    assert_eq!(find("added.txt").index, Some(ChangeKind::Added));
    assert_eq!(find("dir/untracked.txt").kind, EntryKind::Untracked);

    let staged: Vec<_> = status.staged().map(|e| e.display_path()).collect();
    assert_eq!(staged.len(), 3, "{staged:?}"); // both, renamed, added
}

#[test]
fn conflicts_and_ahead_behind() {
    let sb = Sandbox::new();
    let origin = sb.init_repo("origin");
    sb.commit_file(&origin, "f", "base\n", "base");
    sb.run(sb.path(), &["clone", "-q", "origin", "work"]);
    let work = sb.path().join("work");

    sb.commit_file(&origin, "f", "theirs\n", "theirs");
    sb.run(&work, &["fetch", "-q"]);
    sb.commit_file(&work, "f", "ours\n", "ours");

    let status = read_status(sb.git(), &work, &CancelToken::new()).unwrap();
    assert_eq!(status.branch.upstream.as_deref(), Some("origin/main"));
    assert_eq!((status.branch.ahead, status.branch.behind), (1, 1));
    assert!(status.is_clean());

    // Merge conflict on `f`.
    let _ = sb
        .git()
        .command(&work)
        .args(["merge", "-q", "origin/main"])
        .run(&CancelToken::new());
    let status = read_status(sb.git(), &work, &CancelToken::new()).unwrap();
    let conflict = status.conflicts().next().expect("a conflict");
    assert_eq!(conflict.path, "f");
    assert_eq!(&conflict.code, b"UU");
}

#[test]
fn unborn_repository() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("empty");
    std::fs::write(repo.join("a"), "1\n").unwrap();
    let status = read_status(sb.git(), &repo, &CancelToken::new()).unwrap();
    assert_eq!(status.branch.oid, None);
    assert_eq!(status.branch.head.as_deref(), Some("main"));
    assert_eq!(status.unstaged().count(), 1);
}
