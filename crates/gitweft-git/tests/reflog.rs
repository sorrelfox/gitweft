use std::collections::HashSet;
use std::path::Path;

use gitweft_git::read::{ReflogEntry, reflog, unreachable_from_refs};
use gitweft_git::{CancelToken, ObjectId};
use gitweft_testkit::Sandbox;

fn commit(sb: &Sandbox, repo: &Path, content: &str, message: &str) -> ObjectId {
    sb.commit_file(repo, "f.txt", content, message)
        .parse()
        .unwrap()
}

fn log(sb: &Sandbox, repo: &Path, refname: &str) -> Vec<ReflogEntry> {
    reflog(sb.git(), repo, refname, 100, &CancelToken::new()).unwrap()
}

fn lost(sb: &Sandbox, repo: &Path, entries: &[ReflogEntry]) -> HashSet<ObjectId> {
    let ids: Vec<ObjectId> = entries.iter().map(|e| e.id).collect();
    unreachable_from_refs(sb.git(), repo, &ids, &CancelToken::new()).unwrap()
}

#[test]
fn a_reset_leaves_lost_commits_in_the_reflog() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let a = commit(&sb, &repo, "a\n", "First");
    let b = commit(&sb, &repo, "b\n", "Second");
    let c = commit(&sb, &repo, "c\n", "Third");
    sb.run(&repo, &["reset", "-q", "--hard", &a.to_string()]);

    let entries = log(&sb, &repo, "HEAD");
    let ids: Vec<_> = entries.iter().map(|e| e.id).collect();
    assert_eq!(ids, [a, c, b, a]);
    let indexes: Vec<_> = entries.iter().map(|e| e.index).collect();
    assert_eq!(indexes, [0, 1, 2, 3]);
    assert!(entries[0].action.starts_with("reset: moving to"));
    assert_eq!(entries[1].action, "commit: Third");
    assert_eq!(entries[3].action, "commit (initial): First");
    assert_eq!(entries[1].summary, "Third");
    assert_eq!(entries[1].author, "Test Author");
    assert_eq!(entries[1].time, 1_767_225_600); // FIXED_DATE

    assert_eq!(lost(&sb, &repo, &entries), HashSet::from([b, c]));

    // A branch at the second commit brings it (not the third) back into reach.
    sb.run(&repo, &["branch", "keep", &b.to_string()]);
    assert_eq!(lost(&sb, &repo, &entries), HashSet::from([c]));
}

#[test]
fn branch_reflogs_and_amended_commits() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    commit(&sb, &repo, "a\n", "First");
    let before = commit(&sb, &repo, "b\n", "Typo");
    sb.run(&repo, &["commit", "-q", "--amend", "-m", "Fixed"]);

    let entries = log(&sb, &repo, "main");
    assert_eq!(entries.len(), 3);
    assert!(entries[0].action.starts_with("commit (amend):"));
    assert_eq!(entries[0].summary, "Fixed");
    assert_eq!(lost(&sb, &repo, &entries), HashSet::from([before]));

    assert!(lost(&sb, &repo, &[]).is_empty());
}
