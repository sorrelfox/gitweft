//! Shallow clones: the boundary read, deepening, and walking a history whose oldest
//! commits have parents that aren't there.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use gitweft_git::CancelToken;
use gitweft_git::read::{
    TipFilter, WalkOptions, graph_tips, open_repo, read_refs, shallow_commits, unmerged_tips,
    walk_history, walk_history_cli,
};
use gitweft_git::remote::{Deepen, deepen};
use gitweft_testkit::Sandbox;

/// A 10-commit repository and a `--depth 2` clone of it. The clone goes through a
/// `file://` URL: git ignores `--depth` for a plain local path.
fn shallow_clone(sb: &Sandbox) -> (PathBuf, PathBuf) {
    let seed = sb.init_repo("seed");
    for n in 1..=10 {
        sb.commit_file(&seed, "f.txt", &format!("{n}\n"), &format!("c{n}"));
    }
    let url = format!("file://{}", seed.display());
    sb.run(sb.path(), &["clone", "-q", "--depth", "2", &url, "shallow"]);
    (seed, sb.path().join("shallow"))
}

fn rev(sb: &Sandbox, repo: &Path, rev: &str) -> gitweft_git::ObjectId {
    sb.run(repo, &["rev-parse", rev]).trim().parse().unwrap()
}

fn boundary(repo: &Path) -> Vec<gitweft_git::ObjectId> {
    shallow_commits(&open_repo(repo).unwrap()).unwrap()
}

#[test]
fn deepening_moves_the_boundary_and_unshallow_removes_it() {
    let sb = Sandbox::new();
    let (seed, repo) = shallow_clone(&sb);
    assert!(boundary(&seed).is_empty(), "a full clone has no boundary");
    assert_eq!(boundary(&repo), [rev(&sb, &seed, "HEAD~1")]);

    let cancel = CancelToken::new();
    deepen(sb.git(), &repo, Deepen::By(3), &cancel, |_| {}).unwrap();
    assert_eq!(boundary(&repo), [rev(&sb, &seed, "HEAD~4")]);
    assert_eq!(sb.run(&repo, &["rev-list", "--count", "HEAD"]).trim(), "5");

    deepen(sb.git(), &repo, Deepen::All, &cancel, |_| {}).unwrap();
    assert!(boundary(&repo).is_empty());
    assert!(!repo.join(".git/shallow").exists());
    assert_eq!(sb.run(&repo, &["rev-list", "--count", "HEAD"]).trim(), "10");

    // Unshallowing a complete repository is an error in git.
    let err = deepen(sb.git(), &repo, Deepen::All, &cancel, |_| {}).unwrap_err();
    assert!(err.summary().contains("complete repository"), "{err}");
}

#[test]
fn a_linked_worktree_sees_the_boundary_of_the_common_dir() {
    let sb = Sandbox::new();
    let (seed, repo) = shallow_clone(&sb);
    sb.run(&repo, &["worktree", "add", "-q", "../linked"]);
    assert_eq!(
        boundary(&sb.path().join("linked")),
        [rev(&sb, &seed, "HEAD~1")]
    );
}

#[test]
fn both_walks_end_at_the_boundary_without_parents() {
    let sb = Sandbox::new();
    let (seed, repo) = shallow_clone(&sb);
    let gix_repo = open_repo(&repo).unwrap();
    let tips = graph_tips(
        &gix_repo,
        &read_refs(&gix_repo).unwrap(),
        &WalkOptions::default(),
        &TipFilter::default(),
    );
    let via_gix = walk_history(
        &gix_repo,
        &tips,
        &WalkOptions::default(),
        &AtomicBool::new(false),
    )
    .unwrap();
    let via_git = walk_history_cli(
        sb.git(),
        gix_repo.git_dir(),
        &tips,
        &WalkOptions::default(),
        &CancelToken::new(),
    )
    .unwrap();

    let ids = |walk: &[gitweft_git::read::WalkedCommit]| {
        walk.iter()
            .map(|c| (c.id, c.parents.to_vec()))
            .collect::<Vec<_>>()
    };
    let (head, first) = (rev(&sb, &seed, "HEAD"), rev(&sb, &seed, "HEAD~1"));
    assert_eq!(ids(&via_git), [(head, vec![first]), (first, vec![])]);
    assert_eq!(ids(&via_gix), ids(&via_git));
}

#[test]
fn unmerged_tips_ends_at_the_boundary() {
    let sb = Sandbox::new();
    let (seed, repo) = shallow_clone(&sb);
    let gix_repo = open_repo(&repo).unwrap();
    let (head, first) = (rev(&sb, &seed, "HEAD"), rev(&sb, &seed, "HEAD~1"));
    // The boundary commit's parents were never fetched: the walk mustn't look for them.
    let tips = unmerged_tips(&gix_repo, &[first, head], None, &AtomicBool::new(false)).unwrap();
    assert_eq!(tips, [head]);
}
