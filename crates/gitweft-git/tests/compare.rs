use std::path::Path;

use gitweft_git::read::{
    ChangeKind, Comparison, changed_files, compare, count_commits, count_merges, is_ancestor,
    merge_base,
};
use gitweft_git::{CancelToken, ObjectId};
use gitweft_testkit::Sandbox;

fn id(sb: &Sandbox, repo: &Path, rev: &str) -> ObjectId {
    sb.run(repo, &["rev-parse", rev]).trim().parse().unwrap()
}

fn run_compare(sb: &Sandbox, repo: &Path, base: &str, head: &str, limit: usize) -> Comparison {
    compare(sb.git(), repo, base, head, limit, &CancelToken::new()).unwrap()
}

#[test]
fn commits_on_each_side_and_files_since_the_merge_base() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let base = sb.commit_file(&repo, "f.txt", "base\n", "Base");
    sb.run(&repo, &["switch", "-q", "-c", "topic"]);
    let t1 = sb.commit_file(&repo, "t.txt", "topic\n", "Topic one");
    let t2 = sb.commit_file(&repo, "t.txt", "topic 2\n", "Topic two");
    sb.run(&repo, &["switch", "-q", "main"]);
    let m1 = sb.commit_file(&repo, "f.txt", "main\n", "Main moves on");

    let c = run_compare(&sb, &repo, "main", "topic", 100);
    assert_eq!(c.base, id(&sb, &repo, "main"));
    assert_eq!(c.head, id(&sb, &repo, "topic"));
    assert_eq!(c.merge_base.map(|m| m.to_string()), Some(base));
    let ahead: Vec<_> = c.ahead.iter().map(|x| x.id.to_string()).collect();
    assert_eq!(ahead, [t2, t1]);
    let behind: Vec<_> = c.behind.iter().map(|x| x.id.to_string()).collect();
    assert_eq!(behind, [m1]);
    assert_eq!((c.ahead_count, c.behind_count), (2, 1));
    assert_eq!(c.ahead[0].summary, "Topic two");
    assert_eq!(c.ahead[0].author.name, "Test Author");
    // Only what the topic did since it forked, not main's later change to f.txt.
    let files: Vec<_> = c
        .files
        .iter()
        .map(|f| (f.kind, std::str::from_utf8(&f.path).unwrap()))
        .collect();
    assert_eq!(files, [(ChangeKind::Added, "t.txt")]);
    // Each file comes with its content in the compared ref.
    assert_eq!(c.blobs, [id(&sb, &repo, "topic:t.txt")]);

    // The lists are capped; the counts stay exact.
    let capped = run_compare(&sb, &repo, "main", "topic", 1);
    assert_eq!(capped.ahead.len(), 1);
    assert_eq!(capped.ahead_count, 2);
}

#[test]
fn unrelated_histories_compare_the_trees() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "a\n", "Main root");
    sb.run(&repo, &["switch", "-q", "--orphan", "other"]);
    sb.commit_file(&repo, "b.txt", "b\n", "Other root");

    let c = run_compare(&sb, &repo, "main", "other", 100);
    assert_eq!(c.merge_base, None);
    assert_eq!((c.ahead_count, c.behind_count), (1, 1));
    let mut files: Vec<_> = c
        .files
        .iter()
        .map(|f| (f.kind, std::str::from_utf8(&f.path).unwrap()))
        .collect();
    files.sort_by_key(|(_, path)| *path);
    assert_eq!(
        files,
        [(ChangeKind::Deleted, "a.txt"), (ChangeKind::Added, "b.txt")]
    );
    // A deleted file is known by the content it removed.
    let deleted = c.files.iter().position(|f| f.path == "a.txt").unwrap();
    assert_eq!(c.blobs[deleted], id(&sb, &repo, "main:a.txt"));
}

#[test]
fn unknown_or_option_like_revisions_are_errors() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "a\n", "Root");
    let cancel = CancelToken::new();
    assert!(compare(sb.git(), &repo, "main", "nope", 10, &cancel).is_err());
    assert!(compare(sb.git(), &repo, "--output=x", "main", 10, &cancel).is_err());
    assert!(!repo.join("x").exists());
}

#[test]
fn ancestry_file_changes_and_commit_counts_of_a_range() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "f.txt", "base\n", "Base");
    sb.run(&repo, &["switch", "-q", "-c", "topic"]);
    sb.commit_file(&repo, "t.txt", "topic\n", "Topic one");
    sb.commit_file(&repo, "t.txt", "topic 2\n", "Topic two");
    sb.commit_file(&repo, "u.txt", "u\n", "Topic three");
    sb.run(&repo, &["switch", "-q", "main"]);
    sb.commit_file(&repo, "f.txt", "main\n", "Main moves on");
    let (git, cancel) = (sb.git(), CancelToken::new());
    let [base, t1, topic, main] = ["main~1", "topic~2", "topic", "main"].map(|r| id(&sb, &repo, r));

    assert!(is_ancestor(git, &repo, &base, &topic, &cancel).unwrap());
    assert!(is_ancestor(git, &repo, &topic, &topic, &cancel).unwrap());
    assert!(!is_ancestor(git, &repo, &topic, &base, &cancel).unwrap());
    // Diverged: neither contains the other.
    assert!(!is_ancestor(git, &repo, &main, &topic, &cancel).unwrap());
    assert!(!is_ancestor(git, &repo, &topic, &main, &cancel).unwrap());
    // A commit git doesn't have is an error, not "no".
    let missing = ObjectId::from_hex(b"0123456789abcdef0123456789abcdef01234567").unwrap();
    assert!(is_ancestor(git, &repo, &missing, &topic, &cancel).is_err());

    let (files, blobs) = changed_files(git, &repo, &t1, &topic, &cancel).unwrap();
    let files: Vec<_> = files
        .iter()
        .map(|f| (f.kind, std::str::from_utf8(&f.path).unwrap()))
        .collect();
    assert_eq!(
        files,
        [
            (ChangeKind::Modified, "t.txt"),
            (ChangeKind::Added, "u.txt")
        ]
    );
    assert_eq!(
        blobs,
        [id(&sb, &repo, "topic:t.txt"), id(&sb, &repo, "topic:u.txt")]
    );

    assert_eq!(count_commits(git, &repo, &t1, &topic, &cancel).unwrap(), 2);
    assert_eq!(
        count_commits(git, &repo, &base, &topic, &cancel).unwrap(),
        3
    );
    assert_eq!(
        count_commits(git, &repo, &topic, &main, &cancel).unwrap(),
        1
    );
    assert_eq!(
        count_commits(git, &repo, &topic, &topic, &cancel).unwrap(),
        0
    );
}

#[test]
fn merge_base_is_the_fork_point_or_none_for_unrelated_histories() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let cancel = CancelToken::new();
    let base = sb.commit_file(&repo, "f.txt", "base\n", "Base");
    sb.run(&repo, &["switch", "-q", "-c", "topic"]);
    let topic = sb.commit_file(&repo, "t.txt", "topic\n", "Topic");
    sb.run(&repo, &["switch", "-q", "main"]);
    sb.commit_file(&repo, "f.txt", "main\n", "Main moves on");
    let base_of = |a: &str, b: &str| {
        merge_base(
            sb.git(),
            &repo,
            id(&sb, &repo, a),
            id(&sb, &repo, b),
            &cancel,
        )
        .unwrap()
        .map(|m| m.to_string())
    };
    // Diverged: where the two forked.
    assert_eq!(base_of("main", "topic"), Some(base.clone()));
    assert_eq!(base_of("topic", "main"), Some(base));
    // Merged: the merged branch's tip.
    sb.run(
        &repo,
        &["merge", "-q", "--no-ff", "-m", "Merge topic", "topic"],
    );
    assert_eq!(base_of("main", "topic"), Some(topic));

    sb.run(&repo, &["switch", "-q", "--orphan", "other"]);
    sb.commit_file(&repo, "o.txt", "o\n", "Other root");
    assert_eq!(base_of("main", "other"), None);
}

/// Merges `to` has that `from` lacks, counted as `count_commits` counts commits.
#[test]
fn merge_counts_of_a_range() {
    let sb = Sandbox::new();
    let git = sb.git();
    let repo = sb.init_repo("r");
    let cancel = CancelToken::new();
    sb.commit_file(&repo, "a.txt", "a\n", "base");
    sb.run(&repo, &["switch", "-q", "-c", "topic"]);
    for n in 1..=2 {
        let branch = format!("side{n}");
        sb.run(&repo, &["switch", "-q", "-c", &branch, "topic"]);
        sb.commit_file(&repo, &format!("{branch}.txt"), "s\n", &branch);
        sb.run(&repo, &["switch", "-q", "topic"]);
        sb.commit_file(&repo, &format!("t{n}.txt"), "t\n", &format!("topic {n}"));
        let message = format!("merge {branch}");
        sb.run(&repo, &["merge", "-q", "--no-ff", "-m", &message, &branch]);
    }
    let merges = |from: &str, to: &str| count_merges(git, &repo, from, to, &cancel).unwrap();
    assert_eq!(merges("main", "topic"), 2);
    assert_eq!(merges("topic~1", "topic"), 1);
    assert_eq!(merges("topic", "main"), 0);
    assert_eq!(merges("topic", "topic"), 0);
    // Plain commits aren't counted.
    let topic = id(&sb, &repo, "topic");
    let base = id(&sb, &repo, "main");
    assert_eq!(
        count_commits(git, &repo, &base, &topic, &cancel).unwrap(),
        6
    );
    // An unknown revision (or one that looks like an option) is an error.
    assert!(count_merges(git, &repo, "main", "nope", &cancel).is_err());
    assert!(count_merges(git, &repo, "--all", "topic", &cancel).is_err());
}
