use std::collections::{BTreeSet, HashSet};

use gitweft_git::CancelToken;
use gitweft_git::gix::bstr::BString;
use gitweft_git::read::{dirs_of, ignored_paths, open_repo, tracked_dirs};
use gitweft_testkit::Sandbox;

fn set(items: &[&str]) -> BTreeSet<BString> {
    items.iter().map(|s| BString::from(*s)).collect()
}

#[test]
fn tracked_dirs_lists_every_directory_holding_tracked_files() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    for dir in ["src/deep", "docs/a/b"] {
        std::fs::create_dir_all(repo.join(dir)).unwrap();
    }
    sb.commit_file(&repo, "top.txt", "t\n", "top");
    sb.commit_file(&repo, "src/deep/x.rs", "x\n", "x");
    sb.commit_file(&repo, "src/lib.rs", "l\n", "lib");
    sb.commit_file(&repo, "docs/a/b/c.md", "c\n", "c");

    let dirs = tracked_dirs(&open_repo(&repo).unwrap()).unwrap();
    assert_eq!(
        dirs,
        set(&["", "docs", "docs/a", "docs/a/b", "src", "src/deep"])
    );
}

#[test]
fn tracked_dirs_of_an_empty_repository_is_the_root() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    assert_eq!(
        tracked_dirs(&open_repo(&repo).unwrap()).unwrap(),
        set(&[""])
    );
}

#[test]
fn tracked_dirs_of_a_truncated_index_is_an_error_not_a_panic() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "a\n", "a");
    // Shorter than a header and a hash: gix-index 0.55 panics on it (#62).
    for damaged in [&b""[..], b"DIRC", b"DIRC\0\0\0\x02\0\0\0\x01"] {
        std::fs::write(repo.join(".git/index"), damaged).unwrap();
        assert!(tracked_dirs(&open_repo(&repo).unwrap()).is_err());
    }
}

#[test]
fn dirs_of_includes_all_ancestors_and_the_root() {
    let paths = ["a/b/c.txt", "a/d.txt", "e.txt", "f/g/h/i"];
    assert_eq!(dirs_of(paths), set(&["", "a", "a/b", "f", "f/g", "f/g/h"]));
}

#[test]
fn ignored_paths_follow_gitignore_but_never_report_tracked_files() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, ".gitignore", "*.log\ntarget/\n", "ignore rules");
    // A tracked file matching an ignore pattern is not ignored.
    std::fs::write(repo.join("kept.log"), "k\n").unwrap();
    sb.run(&repo, &["add", "-f", "kept.log"]);
    std::fs::create_dir(repo.join("target")).unwrap();

    let paths = ["x.log", "target", "kept.log", "new.txt", "src/y.log"].map(BString::from);
    let ignored = ignored_paths(sb.git(), &repo, &paths, &CancelToken::new()).unwrap();
    let expected: HashSet<BString> = ["x.log", "target", "src/y.log"]
        .map(BString::from)
        .into_iter()
        .collect();
    assert_eq!(ignored, expected);

    // Nothing ignored makes git exit with 1, which is not an error.
    let none = ignored_paths(
        sb.git(),
        &repo,
        &[BString::from("new.txt")],
        &CancelToken::new(),
    )
    .unwrap();
    assert!(none.is_empty());
}
