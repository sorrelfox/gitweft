use std::path::{Path, PathBuf};

use gitweft_git::gix::bstr::BString;
use gitweft_git::worktrees::{
    WorktreeInfo, WorktreeTarget, add_worktree, list_worktrees, lock_worktree, prune_worktrees,
    remove_worktree, unlock_worktree, worktree_has_changes,
};
use gitweft_git::{CancelToken, GitError, ObjectId};
use gitweft_testkit::Sandbox;

/// A repository `r` with two commits and a branch `feat` at the first; returns the repo,
/// the sandbox folder (canonical, as git reports paths) and the two commit ids.
fn setup(sb: &Sandbox) -> (PathBuf, PathBuf, ObjectId, ObjectId) {
    let repo = sb.init_repo("r");
    let first = sb.commit_file(&repo, "a.txt", "1\n", "first");
    sb.run(&repo, &["branch", "feat"]);
    let second = sb.commit_file(&repo, "a.txt", "2\n", "second");
    let root = std::fs::canonicalize(sb.path()).unwrap();
    let id = |hex: &str| ObjectId::from_hex(hex.as_bytes()).unwrap();
    (root.join("r"), root, id(&first), id(&second))
}

fn list(sb: &Sandbox, repo: &Path) -> Vec<WorktreeInfo> {
    list_worktrees(sb.git(), repo, &CancelToken::new()).unwrap()
}

fn find(sb: &Sandbox, repo: &Path, path: &Path) -> WorktreeInfo {
    list(sb, repo)
        .into_iter()
        .find(|w| w.path == path)
        .unwrap_or_else(|| panic!("{} is not listed", path.display()))
}

fn add(sb: &Sandbox, repo: &Path, path: &Path, target: WorktreeTarget) {
    add_worktree(sb.git(), repo, path, &target, &CancelToken::new()).unwrap();
}

fn detached(sb: &Sandbox, repo: &Path, path: &Path) {
    add(sb, repo, path, WorktreeTarget::Detached("HEAD".into()));
}

fn message(err: &GitError) -> String {
    err.summary()
}

#[test]
fn main_worktree_is_listed_first() {
    let sb = Sandbox::new();
    let (repo, root, _, second) = setup(&sb);
    detached(&sb, &repo, &root.join("w"));

    let worktrees = list(&sb, &repo);
    assert_eq!(worktrees.len(), 2, "{worktrees:?}");
    assert_eq!(
        worktrees[0],
        WorktreeInfo {
            path: repo.clone(),
            head: Some(second),
            branch: Some("refs/heads/main".into()),
            bare: false,
            is_main: true,
            locked: None,
            prunable: None,
        }
    );
    assert!(!worktrees[1].is_main);
    // Listed the same from inside a linked worktree.
    assert_eq!(list(&sb, &root.join("w")), worktrees);
}

#[test]
fn add_a_new_branch_an_existing_branch_and_detached() {
    let sb = Sandbox::new();
    let (repo, root, first, second) = setup(&sb);

    let new_at_head = root.join("new at head");
    add(
        &sb,
        &repo,
        &new_at_head,
        WorktreeTarget::NewBranch {
            name: "topic".into(),
            start: None,
        },
    );
    let new_at_first = root.join("from-first");
    add(
        &sb,
        &repo,
        &new_at_first,
        WorktreeTarget::NewBranch {
            name: "old".into(),
            start: Some(first.to_string()),
        },
    );
    let existing = root.join("feat");
    add(&sb, &repo, &existing, WorktreeTarget::Branch("feat".into()));
    // A relative path is relative to the repository, and may start with `-`.
    add(
        &sb,
        &repo,
        Path::new("../-detached"),
        WorktreeTarget::Detached("feat".into()),
    );

    let got: Vec<_> = list(&sb, &repo)
        .into_iter()
        .map(|w| (w.path, w.head, w.branch, w.is_main))
        .collect();
    let branch = |name: &str| Some(BString::from(format!("refs/heads/{name}")));
    assert_eq!(
        got,
        [
            (repo.clone(), Some(second), branch("main"), true),
            (root.join("-detached"), Some(first), None, false),
            (existing, Some(first), branch("feat"), false),
            (new_at_first, Some(first), branch("old"), false),
            (new_at_head.clone(), Some(second), branch("topic"), false),
        ]
    );
    assert_eq!(
        std::fs::read_to_string(new_at_head.join("a.txt")).unwrap(),
        "2\n"
    );
}

/// A worktree on a branch whose name isn't UTF-8 lists it byte for byte.
#[cfg(unix)]
#[test]
fn a_worktree_branch_that_isnt_utf8_keeps_its_bytes() {
    use std::os::unix::ffi::OsStrExt as _;
    let sb = Sandbox::new();
    let (repo, root, _, _) = setup(&sb);
    let path = root.join("latin");
    sb.git()
        .command(&repo)
        .args(["worktree", "add", "-b"])
        .arg(std::ffi::OsStr::from_bytes(b"caf\xe9"))
        .arg(&path)
        .run(&CancelToken::new())
        .unwrap();
    let listed = list(&sb, &repo);
    let worktree = listed.iter().find(|w| w.path == path).unwrap();
    assert_eq!(
        worktree.branch,
        Some(BString::from(b"refs/heads/caf\xe9".as_slice()))
    );
}

#[test]
fn a_branch_checked_out_elsewhere_is_refused() {
    let sb = Sandbox::new();
    let (repo, root, _, _) = setup(&sb);
    let cancel = CancelToken::new();
    add(
        &sb,
        &repo,
        &root.join("w1"),
        WorktreeTarget::Branch("feat".into()),
    );

    for (path, name) in [("w2", "feat"), ("w3", "main")] {
        let err = add_worktree(
            sb.git(),
            &repo,
            &root.join(path),
            &WorktreeTarget::Branch(name.into()),
            &cancel,
        )
        .unwrap_err();
        // Git 2.39 says "is already checked out at".
        let text = message(&err);
        assert!(
            text.contains(&format!("'{name}' is already used by worktree at"))
                || text.contains(&format!("'{name}' is already checked out at")),
            "{err:?}"
        );
        assert!(!root.join(path).exists());
    }
    // A new branch with an existing name.
    let err = add_worktree(
        sb.git(),
        &repo,
        &root.join("w4"),
        &WorktreeTarget::NewBranch {
            name: "feat".into(),
            start: None,
        },
        &cancel,
    )
    .unwrap_err();
    assert_eq!(message(&err), "fatal: a branch named 'feat' already exists");
    assert_eq!(list(&sb, &repo).len(), 2);
}

#[test]
fn names_starting_with_a_dash_are_refused() {
    let sb = Sandbox::new();
    let (repo, root, _, _) = setup(&sb);
    let cancel = CancelToken::new();
    for target in [
        WorktreeTarget::Branch("-f".into()),
        WorktreeTarget::Detached("--orphan".into()),
        WorktreeTarget::NewBranch {
            name: "-x".into(),
            start: None,
        },
        WorktreeTarget::NewBranch {
            name: "x".into(),
            start: Some("-HEAD".into()),
        },
    ] {
        let err = add_worktree(sb.git(), &repo, &root.join("w"), &target, &cancel).unwrap_err();
        assert!(message(&err).contains("is not a valid"), "{err:?}");
        // Refused before running git at all.
        assert_eq!(err.exit_code, None);
    }
    assert!(!root.join("w").exists());
    assert_eq!(list(&sb, &repo).len(), 1);
}

#[test]
fn lock_and_unlock() {
    let sb = Sandbox::new();
    let (repo, root, _, _) = setup(&sb);
    let cancel = CancelToken::new();
    let (a, b) = (root.join("a"), root.join("b"));
    detached(&sb, &repo, &a);
    detached(&sb, &repo, &b);

    lock_worktree(sb.git(), &repo, &a, Some("on a usb stick"), &cancel).unwrap();
    lock_worktree(sb.git(), &repo, &b, None, &cancel).unwrap();
    assert_eq!(
        find(&sb, &repo, &a).locked.as_deref(),
        Some("on a usb stick")
    );
    assert_eq!(find(&sb, &repo, &b).locked.as_deref(), Some(""));

    let err = lock_worktree(sb.git(), &repo, &b, Some("again"), &cancel).unwrap_err();
    assert!(message(&err).contains("is already locked"), "{err:?}");

    // Removing a locked worktree fails, even with force.
    for force in [false, true] {
        let err = remove_worktree(sb.git(), &repo, &a, force, &cancel).unwrap_err();
        assert_eq!(
            message(&err),
            "fatal: cannot remove a locked working tree, lock reason: on a usb stick"
        );
    }
    let err = remove_worktree(sb.git(), &repo, &b, false, &cancel).unwrap_err();
    // Without a reason, git's line ends in `;` (the hint follows on the next line).
    assert_eq!(message(&err), "fatal: cannot remove a locked working tree;");
    assert!(a.exists() && b.exists());

    unlock_worktree(sb.git(), &repo, &a, &cancel).unwrap();
    assert_eq!(find(&sb, &repo, &a).locked, None);
    assert_eq!(find(&sb, &repo, &b).locked.as_deref(), Some(""));
    let err = unlock_worktree(sb.git(), &repo, &a, &cancel).unwrap_err();
    assert!(message(&err).ends_with("is not locked"), "{err:?}");

    remove_worktree(sb.git(), &repo, &a, false, &cancel).unwrap();
    assert!(!a.exists());
}

#[test]
fn remove_a_clean_worktree() {
    let sb = Sandbox::new();
    let (repo, root, _, _) = setup(&sb);
    let cancel = CancelToken::new();
    let path = root.join("clean");
    add(&sb, &repo, &path, WorktreeTarget::Branch("feat".into()));
    assert!(!worktree_has_changes(sb.git(), &path, &cancel).unwrap());

    remove_worktree(sb.git(), &repo, &path, false, &cancel).unwrap();
    assert!(!path.exists());
    assert_eq!(list(&sb, &repo).len(), 1);
    // Its branch stays.
    sb.run(&repo, &["rev-parse", "--verify", "refs/heads/feat"]);

    // The main worktree can't be removed.
    let err = remove_worktree(sb.git(), &repo, &repo, true, &cancel).unwrap_err();
    assert!(message(&err).ends_with("is a main working tree"), "{err:?}");
}

#[test]
fn a_modified_worktree_needs_force() {
    let sb = Sandbox::new();
    let (repo, root, _, _) = setup(&sb);
    let cancel = CancelToken::new();
    let path = root.join("dirty");
    detached(&sb, &repo, &path);
    std::fs::write(path.join("a.txt"), "changed\n").unwrap();
    assert!(worktree_has_changes(sb.git(), &path, &cancel).unwrap());

    let err = remove_worktree(sb.git(), &repo, &path, false, &cancel).unwrap_err();
    assert!(
        message(&err).ends_with("contains modified or untracked files, use --force to delete it"),
        "{err:?}"
    );
    assert!(path.join("a.txt").exists());

    remove_worktree(sb.git(), &repo, &path, true, &cancel).unwrap();
    assert!(!path.exists());
    assert_eq!(list(&sb, &repo).len(), 1);
}

#[test]
fn an_untracked_file_needs_force() {
    let sb = Sandbox::new();
    let (repo, root, _, _) = setup(&sb);
    let cancel = CancelToken::new();
    let path = root.join("untracked");
    detached(&sb, &repo, &path);
    std::fs::write(path.join("new.txt"), "new\n").unwrap();
    // Even when the user hides untracked files from `git status`.
    sb.run(&repo, &["config", "status.showUntrackedFiles", "no"]);
    assert!(worktree_has_changes(sb.git(), &path, &cancel).unwrap());

    let err = remove_worktree(sb.git(), &repo, &path, false, &cancel).unwrap_err();
    assert!(
        message(&err).contains("contains modified or untracked files"),
        "{err:?}"
    );
    assert!(path.join("new.txt").exists());

    remove_worktree(sb.git(), &repo, &path, true, &cancel).unwrap();
    assert!(!path.exists());
}

#[test]
fn ignored_files_are_not_changes() {
    let sb = Sandbox::new();
    let (repo, root, _, _) = setup(&sb);
    let cancel = CancelToken::new();
    sb.commit_file(&repo, ".gitignore", "*.log\n", "ignore logs");
    let path = root.join("w");
    detached(&sb, &repo, &path);
    std::fs::write(path.join("build.log"), "x\n").unwrap();
    assert!(!worktree_has_changes(sb.git(), &path, &cancel).unwrap());
}

#[test]
fn a_worktree_moves_unless_locked_and_its_lock_keeps_the_reason() {
    use gitweft_git::worktrees::{lock_worktree, move_worktree, unlock_worktree};
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "a\n", "first");
    let (wt, moved) = (sb.path().join("wt"), sb.path().join("moved"));
    detached(&sb, &repo, &wt);
    let cancel = CancelToken::new();

    lock_worktree(sb.git(), &repo, &wt, Some("on a usb drive"), &cancel).unwrap();
    assert_eq!(
        find(&sb, &repo, &wt).locked.as_deref(),
        Some("on a usb drive")
    );
    let err = move_worktree(sb.git(), &repo, &wt, &moved, &cancel).unwrap_err();
    assert!(message(&err).contains("locked"), "{}", message(&err));
    assert!(wt.exists() && !moved.exists());

    unlock_worktree(sb.git(), &repo, &wt, &cancel).unwrap();
    move_worktree(sb.git(), &repo, &wt, &moved, &cancel).unwrap();
    assert!(!wt.exists() && moved.join("a.txt").exists());
    let paths: Vec<PathBuf> = list(&sb, &repo).into_iter().map(|info| info.path).collect();
    assert!(paths.contains(&moved), "{paths:?}");
}

#[test]
fn a_deleted_folder_is_prunable() {
    let sb = Sandbox::new();
    let (repo, root, _, _) = setup(&sb);
    let cancel = CancelToken::new();
    let (gone, kept, locked) = (root.join("gone"), root.join("kept"), root.join("locked"));
    detached(&sb, &repo, &gone);
    detached(&sb, &repo, &kept);
    detached(&sb, &repo, &locked);
    lock_worktree(sb.git(), &repo, &locked, Some("offline"), &cancel).unwrap();
    std::fs::remove_dir_all(&gone).unwrap();
    std::fs::remove_dir_all(&locked).unwrap();

    assert_eq!(
        find(&sb, &repo, &gone).prunable.as_deref(),
        Some("gitdir file points to non-existent location")
    );
    assert_eq!(find(&sb, &repo, &kept).prunable, None);
    // A locked worktree is never prunable.
    assert_eq!(find(&sb, &repo, &locked).prunable, None);

    prune_worktrees(sb.git(), &repo, &cancel).unwrap();
    let paths: Vec<_> = list(&sb, &repo).into_iter().map(|w| w.path).collect();
    assert_eq!(paths, [repo.clone(), kept, locked]);
}
