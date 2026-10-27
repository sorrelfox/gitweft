use std::path::Path;

use gitweft_git::read::{DanglingCommit, dangling_commits};
use gitweft_git::{CancelToken, GitError, GitErrorKind, ObjectId};
use gitweft_testkit::Sandbox;

fn commit(sb: &Sandbox, repo: &Path, content: &str, message: &str) -> ObjectId {
    sb.commit_file(repo, "f.txt", content, message)
        .parse()
        .unwrap()
}

fn rev(sb: &Sandbox, repo: &Path, spec: &str) -> ObjectId {
    sb.run(repo, &["rev-parse", spec]).trim().parse().unwrap()
}

fn dangling(sb: &Sandbox, repo: &Path, limit: usize) -> (Vec<DanglingCommit>, usize) {
    dangling_commits(sb.git(), repo, limit, &CancelToken::new()).unwrap()
}

fn ids(sb: &Sandbox, repo: &Path) -> Vec<ObjectId> {
    dangling(sb, repo, 100).0.iter().map(|c| c.id).collect()
}

/// Forgets every reflog entry, so only refs keep commits reachable.
fn expire_reflogs(sb: &Sandbox, repo: &Path) {
    sb.run(
        repo,
        &[
            "reflog",
            "expire",
            "--expire=now",
            "--expire-unreachable=now",
            "--all",
        ],
    );
}

#[test]
fn a_reset_commit_dangles_once_no_reflog_has_it() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let a = commit(&sb, &repo, "a\n", "First");
    commit(&sb, &repo, "b\n", "Second");
    let c = commit(&sb, &repo, "c\n", "Third");
    sb.run(&repo, &["reset", "-q", "--hard", &a.to_string()]);

    // HEAD's reflog still has it: the reflog page shows it, this list doesn't.
    assert_eq!(dangling(&sb, &repo, 100), (vec![], 0));

    expire_reflogs(&sb, &repo);
    let (list, total) = dangling(&sb, &repo, 100);
    // Only the tip: the second commit is reached through the third.
    assert_eq!(total, 1);
    assert_eq!(list.len(), 1);
    let lost = &list[0];
    assert_eq!(lost.id, c);
    assert_eq!(lost.summary, "Third");
    assert_eq!(lost.author.name, "Test Author");
    assert_eq!(lost.author.email, "author@example.invalid");
    assert_eq!(lost.author.time, 1_767_225_600); // FIXED_DATE
    assert_eq!(lost.time, 1_767_225_600);
    assert!(!lost.stash);
}

#[test]
fn an_amended_commit_dangles_once_no_reflog_has_it() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    commit(&sb, &repo, "a\n", "First");
    let before = commit(&sb, &repo, "b\n", "Typo");
    sb.run(&repo, &["commit", "-q", "--amend", "-m", "Fixed"]);

    assert!(ids(&sb, &repo).is_empty());
    expire_reflogs(&sb, &repo);
    assert_eq!(ids(&sb, &repo), [before]);
}

#[test]
fn dropped_stashes_are_marked() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    commit(&sb, &repo, "a\n", "First");

    // A plain stash, one with a message, and one with untracked files: dropping
    // removes their reflog entry, so they dangle at once.
    std::fs::write(repo.join("f.txt"), "wip\n").unwrap();
    sb.run(&repo, &["stash", "-q"]);
    let plain = rev(&sb, &repo, "stash");
    std::fs::write(repo.join("f.txt"), "named\n").unwrap();
    sb.run(&repo, &["stash", "push", "-q", "-m", "try this"]);
    let named = rev(&sb, &repo, "stash");
    std::fs::write(repo.join("new.txt"), "untracked\n").unwrap();
    sb.run(&repo, &["stash", "push", "-q", "--include-untracked"]);
    let with_untracked = rev(&sb, &repo, "stash");
    sb.run(&repo, &["stash", "drop", "-q", "stash@{1}"]);
    sb.run(&repo, &["stash", "clear"]);

    // A merge that only reads like a stash: its second parent is no index commit.
    let head = rev(&sb, &repo, "HEAD");
    let side = sb
        .run(&repo, &["commit-tree", "HEAD^{tree}", "-m", "side"])
        .trim()
        .to_owned();
    let fake: ObjectId = sb
        .run(
            &repo,
            &[
                "commit-tree",
                "HEAD^{tree}",
                "-p",
                &head.to_string(),
                "-p",
                &side,
                "-m",
                "On main: looks like a stash",
            ],
        )
        .trim()
        .parse()
        .unwrap();

    let (list, total) = dangling(&sb, &repo, 100);
    assert_eq!(total, 4, "{list:#?}");
    let stash_of = |id: ObjectId| list.iter().find(|c| c.id == id).map(|c| c.stash);
    assert_eq!(stash_of(plain), Some(true));
    assert_eq!(stash_of(named), Some(true));
    assert_eq!(stash_of(with_untracked), Some(true));
    assert_eq!(stash_of(fake), Some(false));
    let named = list.iter().find(|c| c.id == named).unwrap();
    assert_eq!(named.summary, "On main: try this");
}

#[test]
fn the_limit_keeps_the_newest_and_the_total_counts_all() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    commit(&sb, &repo, "a\n", "First");
    // Commits on no ref and in no reflog, committed on different days.
    let mut made = Vec::new();
    for (day, message) in [("02", "February"), ("04", "April"), ("03", "March")] {
        let output = sb
            .git()
            .command(&repo)
            .env("GIT_COMMITTER_DATE", format!("2026-{day}-01T00:00:00Z"))
            .args(["commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", message])
            .run(&CancelToken::new())
            .unwrap();
        made.push(output.stdout_str().trim().parse::<ObjectId>().unwrap());
    }

    let (list, total) = dangling(&sb, &repo, 2);
    assert_eq!(total, 3);
    let summaries: Vec<&str> = list.iter().map(|c| c.summary.as_str()).collect();
    assert_eq!(summaries, ["April", "March"]);
    assert_eq!(list[0].id, made[1]);
    assert!(list[0].time > list[1].time);

    assert_eq!(dangling(&sb, &repo, 0), (vec![], 3));
    assert_eq!(dangling(&sb, &repo, 10).0.len(), 3);
}

#[test]
fn repositories_without_any() {
    let sb = Sandbox::new();
    let empty = sb.init_repo("empty");
    assert_eq!(dangling(&sb, &empty, 10), (vec![], 0));

    let repo = sb.init_repo("r");
    commit(&sb, &repo, "a\n", "First");
    commit(&sb, &repo, "b\n", "Second");
    expire_reflogs(&sb, &repo);
    assert_eq!(dangling(&sb, &repo, 10), (vec![], 0));
}

#[test]
fn fsck_never_writes_lost_found() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let a = commit(&sb, &repo, "a\n", "First");
    commit(&sb, &repo, "b\n", "Second");
    sb.run(&repo, &["reset", "-q", "--hard", &a.to_string()]);
    expire_reflogs(&sb, &repo);
    assert_eq!(ids(&sb, &repo).len(), 1);
    assert!(!repo.join(".git/lost-found").exists());
}

#[test]
fn a_cancelled_search_says_so() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    commit(&sb, &repo, "a\n", "First");
    let cancel = CancelToken::new();
    cancel.cancel();
    let err = dangling_commits(sb.git(), &repo, 10, &cancel).unwrap_err();
    let err = err.downcast_ref::<GitError>().expect("a git error");
    assert_eq!(err.kind, GitErrorKind::Cancelled);
}
