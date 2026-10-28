use std::path::Path;

use gitweft_git::gix::bstr::BString;
use gitweft_git::read::{ChangeKind, FileChange};
use gitweft_git::stash::{
    StashEntry, StashOptions, list_stashes, stash_apply, stash_apply_commit, stash_branch,
    stash_contents, stash_drop, stash_push,
};
use gitweft_git::{CancelToken, GitErrorKind, ObjectId};
use gitweft_testkit::Sandbox;

fn only_stash(sb: &Sandbox, repo: &Path) -> StashEntry {
    let mut stashes = list_stashes(sb.git(), repo, &CancelToken::new()).unwrap();
    assert_eq!(stashes.len(), 1, "{stashes:?}");
    stashes.remove(0)
}

fn paths(changes: &[FileChange]) -> Vec<&str> {
    let mut paths: Vec<&str> = changes
        .iter()
        .map(|c| std::str::from_utf8(&c.path).unwrap())
        .collect();
    paths.sort_unstable();
    paths
}

/// `git status --porcelain`, one line per entry.
fn status(sb: &Sandbox, repo: &Path) -> Vec<String> {
    sb.run(repo, &["status", "--porcelain"])
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn push_only_some_paths() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "1\n", "first");
    sb.commit_file(&repo, "b.txt", "1\n", "second");
    let cancel = CancelToken::new();

    std::fs::write(repo.join("a.txt"), "2\n").unwrap();
    std::fs::write(repo.join("b.txt"), "2\n").unwrap();
    sb.run(&repo, &["add", "a.txt"]);
    std::fs::write(repo.join("a.txt"), "3\n").unwrap();
    let options = StashOptions {
        paths: vec![BString::from("a.txt")],
        ..StashOptions::default()
    };
    assert!(stash_push(sb.git(), &repo, &options, &cancel).unwrap());

    assert_eq!(read(&repo, "a.txt").as_deref(), Some("1\n"));
    assert_eq!(read(&repo, "b.txt").as_deref(), Some("2\n"));
    assert_eq!(status(&sb, &repo), [" M b.txt"]);
    let stash = only_stash(&sb, &repo);
    let contents = stash_contents(sb.git(), &repo, &stash, &cancel).unwrap();
    assert_eq!(paths(&contents.tracked), ["a.txt"]);

    // Its staged and unstaged parts both come back.
    sb.run(&repo, &["checkout", "--", "b.txt"]);
    sb.run(&repo, &["stash", "pop", "--index"]);
    assert_eq!(status(&sb, &repo), ["MM a.txt"]);
    assert_eq!(read(&repo, "a.txt").as_deref(), Some("3\n"));
}

#[test]
fn push_an_untracked_path() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "1\n", "first");
    let cancel = CancelToken::new();

    std::fs::write(repo.join("a.txt"), "2\n").unwrap();
    std::fs::write(repo.join("new.txt"), "new\n").unwrap();
    std::fs::write(repo.join("other.txt"), "other\n").unwrap();
    // A name that would be a glob matching `other.txt`: taken literally. Windows has no
    // `*` in a file name, but takes a `[o]`.
    let glob = if cfg!(unix) { "*.txt" } else { "[o]ther.txt" };
    std::fs::write(repo.join(glob), "star\n").unwrap();
    let options = StashOptions {
        include_untracked: true,
        paths: vec![BString::from("new.txt"), BString::from(glob)],
        ..StashOptions::default()
    };
    assert!(stash_push(sb.git(), &repo, &options, &cancel).unwrap());

    assert_eq!(read(&repo, "new.txt"), None);
    assert_eq!(read(&repo, glob), None);
    assert_eq!(read(&repo, "other.txt").as_deref(), Some("other\n"));
    assert_eq!(read(&repo, "a.txt").as_deref(), Some("2\n"));
    let stash = only_stash(&sb, &repo);
    let contents = stash_contents(sb.git(), &repo, &stash, &cancel).unwrap();
    assert!(contents.tracked.is_empty(), "{contents:?}");
    assert_eq!(paths(&contents.untracked), [glob, "new.txt"]);
}

#[test]
fn branch_from_a_stash() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let base = sb.commit_file(&repo, "a.txt", "1\n", "first");
    let cancel = CancelToken::new();

    std::fs::write(repo.join("a.txt"), "2\n").unwrap();
    std::fs::write(repo.join("staged.txt"), "staged\n").unwrap();
    sb.run(&repo, &["add", "staged.txt"]);
    stash_push(sb.git(), &repo, &StashOptions::default(), &cancel).unwrap();
    sb.commit_file(&repo, "a.txt", "moved on\n", "second");
    let stash = only_stash(&sb, &repo);

    // An existing branch name fails and keeps the stash.
    let err = stash_branch(sb.git(), &repo, &stash, "main", &cancel).unwrap_err();
    assert!(err.stderr.contains("already exists"), "{}", err.stderr);
    assert_eq!(only_stash(&sb, &repo), stash);
    let err = stash_branch(sb.git(), &repo, &stash, "-f", &cancel).unwrap_err();
    assert!(
        err.stderr.contains("not a valid branch name"),
        "{}",
        err.stderr
    );
    assert_eq!(only_stash(&sb, &repo), stash);

    stash_branch(sb.git(), &repo, &stash, "wip", &cancel).unwrap();
    assert_eq!(sb.run(&repo, &["branch", "--show-current"]).trim(), "wip");
    assert_eq!(sb.run(&repo, &["rev-parse", "HEAD"]).trim(), base.trim());
    assert_eq!(read(&repo, "a.txt").as_deref(), Some("2\n"));
    assert_eq!(status(&sb, &repo), [" M a.txt", "A  staged.txt"]);
    assert!(list_stashes(sb.git(), &repo, &cancel).unwrap().is_empty());
}

#[test]
fn branch_from_a_stash_refuses_a_moved_list() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "1\n", "first");
    let cancel = CancelToken::new();
    std::fs::write(repo.join("a.txt"), "2\n").unwrap();
    stash_push(sb.git(), &repo, &StashOptions::default(), &cancel).unwrap();
    let stash = only_stash(&sb, &repo);
    std::fs::write(repo.join("a.txt"), "3\n").unwrap();
    stash_push(sb.git(), &repo, &StashOptions::default(), &cancel).unwrap();

    let err = stash_branch(sb.git(), &repo, &stash, "wip", &cancel).unwrap_err();
    assert!(err.stderr.contains("stash list changed"), "{}", err.stderr);
    assert_eq!(sb.run(&repo, &["branch", "--list", "wip"]), "");
}

/// What git does when the working tree has changes of its own.
#[test]
fn branch_from_a_stash_with_local_changes() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "1\n", "first");
    sb.commit_file(&repo, "b.txt", "1\n", "second");
    let cancel = CancelToken::new();
    std::fs::write(repo.join("a.txt"), "stashed\n").unwrap();
    stash_push(sb.git(), &repo, &StashOptions::default(), &cancel).unwrap();
    let stash = only_stash(&sb, &repo);

    // A local change to a file the stash doesn't touch: carried onto the new branch,
    // and the stash applies next to it.
    std::fs::write(repo.join("b.txt"), "local\n").unwrap();
    stash_branch(sb.git(), &repo, &stash, "wip", &cancel).unwrap();
    assert_eq!(sb.run(&repo, &["branch", "--show-current"]).trim(), "wip");
    assert_eq!(status(&sb, &repo), [" M a.txt", " M b.txt"]);
    assert!(list_stashes(sb.git(), &repo, &cancel).unwrap().is_empty());

    // A local change to a file the stash changes too: the branch is created and
    // checked out, but applying fails, and the stash stays.
    sb.run(&repo, &["checkout", "-q", "--", "b.txt"]);
    stash_push(sb.git(), &repo, &StashOptions::default(), &cancel).unwrap();
    let stash = only_stash(&sb, &repo);
    std::fs::write(repo.join("a.txt"), "local\n").unwrap();
    let err = stash_branch(sb.git(), &repo, &stash, "wip2", &cancel).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::WouldOverwrite, "{}", err.stderr);
    assert!(
        err.stderr.contains("overwritten by merge"),
        "{}",
        err.stderr
    );
    assert_eq!(sb.run(&repo, &["branch", "--show-current"]).trim(), "wip2");
    assert_eq!(read(&repo, "a.txt").as_deref(), Some("local\n"));
    assert_eq!(only_stash(&sb, &repo), stash);

    // A local change the checkout itself would overwrite (the stash was made on an older
    // commit): git aborts before creating the branch.
    sb.run(&repo, &["checkout", "-q", "--", "a.txt"]);
    sb.run(&repo, &["checkout", "-q", "main"]);
    sb.commit_file(&repo, "b.txt", "2\n", "third");
    std::fs::write(repo.join("b.txt"), "local\n").unwrap();
    let err = stash_branch(sb.git(), &repo, &stash, "wip3", &cancel).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::WouldOverwrite, "{}", err.stderr);
    assert!(
        err.stderr.contains("overwritten by checkout"),
        "{}",
        err.stderr
    );
    assert_eq!(sb.run(&repo, &["branch", "--show-current"]).trim(), "main");
    assert_eq!(sb.run(&repo, &["branch", "--list", "wip3"]), "");
    assert_eq!(read(&repo, "b.txt").as_deref(), Some("local\n"));
    assert_eq!(only_stash(&sb, &repo), stash);
}

#[test]
fn contents_of_a_stash() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let base = sb.commit_file(&repo, "a.txt", "1\n", "first");
    let cancel = CancelToken::new();

    std::fs::write(repo.join("a.txt"), "2\n").unwrap();
    std::fs::write(repo.join("staged.txt"), "staged\n").unwrap();
    sb.run(&repo, &["add", "staged.txt"]);
    std::fs::write(repo.join("untracked.txt"), "untracked\n").unwrap();
    let options = StashOptions {
        include_untracked: true,
        ..StashOptions::default()
    };
    stash_push(sb.git(), &repo, &options, &cancel).unwrap();
    let stash = only_stash(&sb, &repo);
    let contents = stash_contents(sb.git(), &repo, &stash, &cancel).unwrap();

    assert_eq!(contents.base.to_string(), base.trim());
    let kinds = |changes: &[FileChange]| -> Vec<(String, ChangeKind)> {
        let mut kinds: Vec<_> = changes
            .iter()
            .map(|c| (c.path.to_string(), c.kind))
            .collect();
        kinds.sort_by(|a, b| a.0.cmp(&b.0));
        kinds
    };
    assert_eq!(
        kinds(&contents.tracked),
        [
            ("a.txt".to_owned(), ChangeKind::Modified),
            ("staged.txt".to_owned(), ChangeKind::Added),
        ]
    );
    assert_eq!(
        kinds(&contents.untracked),
        [("untracked.txt".to_owned(), ChangeKind::Added)]
    );
    let third = sb.run(&repo, &["rev-parse", &format!("{}^3", stash.id)]);
    assert_eq!(
        contents.untracked_commit.map(|id| id.to_string()),
        Some(third.trim().to_owned())
    );
}

/// The untracked files' commit has no parent, so it's compared with the empty tree, which
/// in a SHA-256 repository isn't SHA-1's.
#[test]
fn contents_of_a_stash_in_a_sha256_repository() {
    let sb = Sandbox::new();
    let repo = sb.init_sha256_repo("r");
    let base = sb.commit_file(&repo, "a.txt", "1\n", "first");
    assert_eq!(base.trim().len(), 64);
    let cancel = CancelToken::new();

    std::fs::write(repo.join("a.txt"), "2\n").unwrap();
    std::fs::write(repo.join("untracked.txt"), "untracked\n").unwrap();
    let options = StashOptions {
        include_untracked: true,
        ..StashOptions::default()
    };
    // Ubuntu 24.04's git (2.43.0-1ubuntu7.3, in the CI container) can't stash in a SHA-256
    // repository at all, even from a terminal; git 2.43.0 built from source can.
    if let Err(err) = stash_push(sb.git(), &repo, &options, &cancel) {
        if err.stderr.trim() == "Cannot save the current status" {
            eprintln!("note: this git can't stash in a SHA-256 repository; skipping this test");
            return;
        }
        panic!("{err:?}");
    }
    let stash = only_stash(&sb, &repo);
    let contents = stash_contents(sb.git(), &repo, &stash, &cancel).unwrap();

    assert_eq!(contents.base.to_string(), base.trim());
    assert_eq!(paths(&contents.tracked), ["a.txt"]);
    assert_eq!(paths(&contents.untracked), ["untracked.txt"]);
    assert_eq!(contents.untracked[0].kind, ChangeKind::Added);
}

#[test]
fn contents_of_a_stash_without_untracked_files() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "1\n", "first");
    let cancel = CancelToken::new();
    std::fs::write(repo.join("a.txt"), "2\n").unwrap();
    std::fs::write(repo.join("untracked.txt"), "untracked\n").unwrap();
    stash_push(sb.git(), &repo, &StashOptions::default(), &cancel).unwrap();
    let stash = only_stash(&sb, &repo);

    let contents = stash_contents(sb.git(), &repo, &stash, &cancel).unwrap();
    assert_eq!(paths(&contents.tracked), ["a.txt"]);
    assert_eq!(contents.untracked_commit, None);
    assert!(contents.untracked.is_empty());
}

fn read(repo: &Path, file: &str) -> Option<String> {
    std::fs::read_to_string(repo.join(file)).ok()
}

#[test]
fn push_list_apply_pop_and_drop() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "1\n", "first");
    let cancel = CancelToken::new();

    assert!(
        !stash_push(sb.git(), &repo, &StashOptions::default(), &cancel).unwrap(),
        "nothing to stash is not an error"
    );

    std::fs::write(repo.join("a.txt"), "2\n").unwrap();
    std::fs::write(repo.join("new.txt"), "untracked\n").unwrap();
    let options = StashOptions {
        message: Some("half-done login form".into()),
        include_untracked: true,
        keep_index: false,
        paths: Vec::new(),
    };
    assert!(stash_push(sb.git(), &repo, &options, &cancel).unwrap());
    assert_eq!(read(&repo, "a.txt").as_deref(), Some("1\n"));
    assert_eq!(
        read(&repo, "new.txt"),
        None,
        "untracked files are stashed too"
    );

    std::fs::write(repo.join("a.txt"), "3\n").unwrap();
    stash_push(sb.git(), &repo, &StashOptions::default(), &cancel).unwrap();
    let stashes = list_stashes(sb.git(), &repo, &cancel).unwrap();
    assert_eq!(stashes.len(), 2);
    assert_eq!(stashes[0].index, 0);
    assert!(
        stashes[0].message.starts_with("WIP on main"),
        "{:?}",
        stashes[0]
    );
    assert_eq!(stashes[1].message, "On main: half-done login form");

    // Apply keeps the stash; drop removes it.
    stash_apply(sb.git(), &repo, &stashes[0], false, &cancel).unwrap();
    assert_eq!(read(&repo, "a.txt").as_deref(), Some("3\n"));
    stash_drop(sb.git(), &repo, &stashes[0], &cancel).unwrap();
    sb.run(&repo, &["checkout", "--", "a.txt"]);

    // The remaining stash moved to position 0: the old entry is refused, a fresh one works.
    let err = stash_apply(sb.git(), &repo, &stashes[1], true, &cancel).unwrap_err();
    assert!(err.stderr.contains("stash list changed"), "{}", err.stderr);
    let stashes = list_stashes(sb.git(), &repo, &cancel).unwrap();
    stash_apply(sb.git(), &repo, &stashes[0], true, &cancel).unwrap();
    assert_eq!(read(&repo, "a.txt").as_deref(), Some("2\n"));
    assert_eq!(read(&repo, "new.txt").as_deref(), Some("untracked\n"));
    assert!(list_stashes(sb.git(), &repo, &cancel).unwrap().is_empty());
}

#[test]
fn keep_index_leaves_staged_changes_in_place() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "1\n", "first");
    sb.commit_file(&repo, "b.txt", "1\n", "second");
    let cancel = CancelToken::new();
    std::fs::write(repo.join("a.txt"), "staged\n").unwrap();
    sb.run(&repo, &["add", "a.txt"]);
    std::fs::write(repo.join("b.txt"), "unstaged\n").unwrap();

    let options = StashOptions {
        keep_index: true,
        ..StashOptions::default()
    };
    assert!(stash_push(sb.git(), &repo, &options, &cancel).unwrap());
    // The staged change stays, in the index and the working tree; the rest is stashed.
    assert_eq!(status(&sb, &repo), ["M  a.txt"]);
    assert_eq!(read(&repo, "a.txt").as_deref(), Some("staged\n"));
    assert_eq!(read(&repo, "b.txt").as_deref(), Some("1\n"));
    // The stash holds both.
    let stash = only_stash(&sb, &repo);
    let contents = stash_contents(sb.git(), &repo, &stash, &cancel).unwrap();
    assert_eq!(paths(&contents.tracked), ["a.txt", "b.txt"]);
}

#[test]
fn entries_name_their_position_and_time() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "1\n", "first");
    let cancel = CancelToken::new();
    for content in ["2\n", "3\n"] {
        std::fs::write(repo.join("a.txt"), content).unwrap();
        stash_push(sb.git(), &repo, &StashOptions::default(), &cancel).unwrap();
    }
    let stashes = list_stashes(sb.git(), &repo, &cancel).unwrap();
    assert_eq!(stashes.len(), 2);
    for (index, entry) in stashes.iter().enumerate() {
        assert_eq!(entry.revision(), format!("stash@{{{index}}}"));
        assert_eq!(
            sb.run(&repo, &["rev-parse", &entry.revision()]).trim(),
            entry.id.to_string()
        );
        // The sandbox's fixed commit date, 2026-01-01T00:00:00Z.
        assert_eq!(entry.time, 1_767_225_600);
    }
}

#[test]
fn a_conflicting_pop_keeps_the_stash() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "1\n", "first");
    let cancel = CancelToken::new();
    std::fs::write(repo.join("a.txt"), "stashed\n").unwrap();
    stash_push(sb.git(), &repo, &StashOptions::default(), &cancel).unwrap();
    sb.commit_file(&repo, "a.txt", "committed meanwhile\n", "second");

    let stashes = list_stashes(sb.git(), &repo, &cancel).unwrap();
    let err = stash_apply(sb.git(), &repo, &stashes[0], true, &cancel).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::Conflict, "{}", err.stderr);
    assert_eq!(list_stashes(sb.git(), &repo, &cancel).unwrap().len(), 1);
}

#[test]
fn an_apply_over_local_changes_names_the_file() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "notes.txt", "1\n", "first");
    let cancel = CancelToken::new();
    std::fs::write(repo.join("notes.txt"), "stashed\n").unwrap();
    stash_push(sb.git(), &repo, &StashOptions::default(), &cancel).unwrap();
    std::fs::write(repo.join("notes.txt"), "changed since\n").unwrap();

    let err = stash_apply(sb.git(), &repo, &only_stash(&sb, &repo), false, &cancel).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::WouldOverwrite, "{}", err.stderr);
    // git names the file on the line after its message; the summary carries it.
    assert!(
        err.summary().ends_with("by merge: notes.txt"),
        "{}",
        err.summary()
    );
    assert_eq!(read(&repo, "notes.txt").as_deref(), Some("changed since\n"));
}

#[test]
fn a_dropped_stash_applies_by_its_commit() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "1\n", "first");
    let cancel = CancelToken::new();
    std::fs::write(repo.join("a.txt"), "2\n").unwrap();
    assert!(stash_push(sb.git(), &repo, &StashOptions::default(), &cancel).unwrap());
    let stash = only_stash(&sb, &repo);
    stash_drop(sb.git(), &repo, &stash, &cancel).unwrap();
    assert_eq!(read(&repo, "a.txt").as_deref(), Some("1\n"));

    stash_apply_commit(sb.git(), &repo, stash.id, &cancel).unwrap();
    assert_eq!(read(&repo, "a.txt").as_deref(), Some("2\n"));

    // A commit that isn't a stash is refused.
    sb.run(&repo, &["checkout", "--", "a.txt"]);
    let head: ObjectId = sb
        .run(&repo, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    assert!(stash_apply_commit(sb.git(), &repo, head, &cancel).is_err());
    assert_eq!(read(&repo, "a.txt").as_deref(), Some("1\n"));
}
