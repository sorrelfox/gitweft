use std::path::{Path, PathBuf};

use gitweft_git::CancelToken;
use gitweft_git::read::{Published, published_commits};
use gitweft_testkit::Sandbox;

fn rev(sb: &Sandbox, repo: &Path, name: &str) -> String {
    sb.run(repo, &["rev-parse", name]).trim().to_owned()
}

fn published(sb: &Sandbox, repo: &Path, tip: &str, keep: Option<&str>) -> Published {
    let keep = keep.map(std::ffi::OsStr::new);
    published_commits(sb.git(), repo, tip, keep, &CancelToken::new()).unwrap()
}

/// `main` with c1..c4; `origin/main` (as if fetched) at c3, with `origin/HEAD` pointing
/// at it; `upstream/old` at c2.
fn repo(sb: &Sandbox) -> PathBuf {
    let repo = sb.init_repo("r");
    for n in 1..=4 {
        sb.commit_file(&repo, "a.txt", &format!("{n}\n"), &format!("c{n}"));
    }
    let c3 = rev(sb, &repo, "HEAD~1");
    let c2 = rev(sb, &repo, "HEAD~2");
    sb.run(&repo, &["update-ref", "refs/remotes/origin/main", &c3]);
    sb.run(
        &repo,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );
    sb.run(&repo, &["update-ref", "refs/remotes/upstream/old", &c2]);
    repo
}

#[test]
fn counts_the_dropped_commits_a_remote_has() {
    let sb = Sandbox::new();
    let repo = repo(&sb);
    let c1 = rev(&sb, &repo, "HEAD~3");
    // Resetting to c1 drops c4 (local only), c3 and c2 (both on origin/main).
    assert_eq!(
        published(&sb, &repo, "HEAD", Some(&c1)),
        Published {
            count: 2,
            remote_branches: vec!["origin/main".into()],
        }
    );
    // Everything HEAD reaches: c1 as well.
    assert_eq!(published(&sb, &repo, "HEAD", None).count, 3);
}

#[test]
fn dropping_only_local_commits_is_fine() {
    let sb = Sandbox::new();
    let repo = repo(&sb);
    let c3 = rev(&sb, &repo, "HEAD~1");
    assert!(published(&sb, &repo, "HEAD", Some(&c3)).is_empty());
    // Moving forward drops nothing.
    assert!(published(&sb, &repo, &c3, Some("HEAD")).is_empty());
}

#[test]
fn amending_a_pushed_head_counts_one_commit() {
    let sb = Sandbox::new();
    let repo = repo(&sb);
    sb.run(&repo, &["reset", "-q", "--hard", "HEAD~1"]); // HEAD = c3 = origin/main
    let found = published(&sb, &repo, "HEAD", Some("HEAD~1"));
    assert_eq!(found.count, 1);
    assert_eq!(found.remote_branches, ["origin/main"]);
    // c2 alone is on both remotes.
    let found = published(&sb, &repo, "HEAD~1", Some("HEAD~2"));
    assert_eq!(found.remote_branches, ["origin/main", "upstream/old"]);
}

#[test]
fn head_alone_is_head_without_its_parents_even_for_a_root_commit() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let root = sb.commit_file(&repo, "a.txt", "1\n", "root");
    assert!(published(&sb, &repo, "HEAD", Some("HEAD^@")).is_empty());
    sb.run(&repo, &["update-ref", "refs/remotes/origin/main", &root]);
    assert_eq!(published(&sb, &repo, "HEAD", Some("HEAD^@")).count, 1);
    // With a parent, only HEAD counts.
    sb.commit_file(&repo, "a.txt", "2\n", "second");
    sb.run(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    assert_eq!(published(&sb, &repo, "HEAD", Some("HEAD^@")).count, 1);
}
