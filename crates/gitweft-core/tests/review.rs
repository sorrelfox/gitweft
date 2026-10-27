use std::path::Path;

use gitweft_core::review::{ReviewSession, ReviewedRange, SinceReview, since_review};
use gitweft_git::read::{ChangeKind, RangeChange};
use gitweft_git::{CancelToken, ObjectId};
use gitweft_testkit::Sandbox;

fn id(sb: &Sandbox, repo: &Path, rev: &str) -> ObjectId {
    sb.run(repo, &["rev-parse", rev]).trim().parse().unwrap()
}

fn numbers(except: Option<(u32, &str)>) -> String {
    (1..=30)
        .map(|n| match except {
            Some((line, text)) if line == n => format!("{text}\n"),
            _ => format!("{n}\n"),
        })
        .collect()
}

fn since(sb: &Sandbox, repo: &Path, reviewed: &ReviewedRange) -> SinceReview {
    let base = id(sb, repo, "main");
    let base = sb.run(repo, &["merge-base", &base.to_string(), "topic"]);
    let base: ObjectId = base.trim().parse().unwrap();
    let head = id(sb, repo, "topic");
    since_review(
        sb.git(),
        repo,
        reviewed,
        base,
        head,
        None,
        &CancelToken::new(),
    )
    .unwrap()
}

/// `topic` reviewed at "add a", "add b" and "add c"; then it gains "add d"; then it is
/// rebased onto a newer `main` with "add b" changed and "add c" dropped.
#[test]
fn a_branch_is_unchanged_then_advanced_then_rewritten_since_review() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "base.txt", "base\n", "base");
    sb.run(&repo, &["switch", "-q", "-c", "topic"]);
    sb.commit_file(&repo, "a.txt", "a\n", "add a");
    sb.commit_file(&repo, "b.txt", &numbers(None), "add b");
    sb.commit_file(&repo, "c.txt", "c\n", "add c");

    let mut session = ReviewSession::new("refs/heads/topic".into(), "refs/heads/main".into());
    session.mark_reviewed(id(&sb, &repo, "main"), id(&sb, &repo, "topic"));
    let reviewed = session.reviewed.unwrap();
    assert_eq!(reviewed.head, id(&sb, &repo, "topic"));
    assert_eq!(since(&sb, &repo, &reviewed), SinceReview::Unchanged);

    sb.commit_file(&repo, "d.txt", "d\n", "add d");
    let SinceReview::Advanced {
        from,
        commits,
        files,
        blobs,
    } = since(&sb, &repo, &reviewed)
    else {
        panic!("expected Advanced");
    };
    assert_eq!((from, commits), (reviewed.head, 1));
    let files: Vec<_> = files
        .iter()
        .map(|f| (f.kind, std::str::from_utf8(&f.path).unwrap()))
        .collect();
    assert_eq!(files, [(ChangeKind::Added, "d.txt")]);
    assert_eq!(blobs, [id(&sb, &repo, "topic:d.txt")]);

    sb.run(&repo, &["switch", "-q", "main"]);
    sb.commit_file(&repo, "m.txt", "m\n", "main moves");
    sb.run(&repo, &["switch", "-q", "topic"]);
    sb.run(&repo, &["rebase", "-q", "main"]);
    let rebased_d = id(&sb, &repo, "HEAD");
    sb.run(&repo, &["reset", "-q", "--hard", "HEAD~2"]);
    std::fs::write(repo.join("b.txt"), numbers(Some((17, "seventeen")))).unwrap();
    sb.run(&repo, &["commit", "-q", "--amend", "-am", "add b"]);
    sb.run(&repo, &["cherry-pick", &rebased_d.to_string()]);

    let SinceReview::Rewritten { pairs } = since(&sb, &repo, &reviewed) else {
        panic!("expected Rewritten");
    };
    let kinds: Vec<(RangeChange, &str)> = pairs
        .iter()
        .map(|pair| (pair.change, pair.summary.as_str()))
        .collect();
    assert_eq!(
        kinds,
        [
            (RangeChange::Same, "add a"),
            (RangeChange::Changed, "add b"),
            (RangeChange::Removed, "add c"),
            (RangeChange::Added, "add d"),
        ]
    );
    assert_eq!(
        pairs[3].new.map(|(_, id)| id),
        Some(id(&sb, &repo, "topic"))
    );
}

#[test]
fn a_reviewed_head_git_no_longer_has_is_an_error() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "base.txt", "base\n", "base");
    let head = id(&sb, &repo, "main");
    let gone = ObjectId::from_hex(b"0123456789abcdef0123456789abcdef01234567").unwrap();
    let reviewed = ReviewedRange {
        base: gone,
        head: gone,
    };
    let err = since_review(
        sb.git(),
        &repo,
        &reviewed,
        head,
        head,
        None,
        &CancelToken::new(),
    )
    .unwrap_err();
    assert!(!err.is_empty());
    eprintln!("{err}");
}
