//! Loading a repository with walk options: the graph built from a first-parent walk
//! still marks merges, but draws no lines to the parents that weren't walked. Each load
//! uses the sandbox's data dir and the repository's own config (#50).

use std::path::Path;

use gitweft_core::repo::{LoadEnv, load_repo_in};
use gitweft_git::read::{TipFilter, WalkOptions};
use gitweft_git::{CancelToken, ObjectId};
use gitweft_testkit::Sandbox;

fn id(sb: &Sandbox, repo: &Path, rev: &str) -> ObjectId {
    sb.run(repo, &["rev-parse", rev]).trim().parse().unwrap()
}

#[test]
fn a_first_parent_walk_keeps_merges_and_one_lane_for_main() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "f.txt", "base\n", "base");
    sb.run(&repo, &["switch", "-q", "-c", "side"]);
    sb.commit_file(&repo, "s.txt", "side\n", "side 1");
    sb.run(&repo, &["switch", "-q", "main"]);
    sb.commit_file(&repo, "f.txt", "main\n", "main 1");
    sb.run(
        &repo,
        &["merge", "-q", "--no-ff", "-m", "merge side", "side"],
    );
    let side = id(&sb, &repo, "side");
    // Merged and gone: its commit is only reachable through the merge's second parent.
    sb.run(&repo, &["branch", "-q", "-D", "side"]);
    sb.run(&repo, &["switch", "-q", "-c", "other", "main~1"]);
    sb.commit_file(&repo, "o.txt", "other\n", "other 1");
    sb.run(&repo, &["switch", "-q", "main"]);
    sb.run(&repo, &["tag", "v1"]);
    let merge = id(&sb, &repo, "main");
    let cancel = CancelToken::new();

    let env = LoadEnv::isolated(sb.path().join("data"));
    let load_repo = |walk, cancel| load_repo_in(&repo, &env, walk, None, cancel);
    let everything = load_repo(Some(WalkOptions::default()), &cancel).unwrap();
    assert_eq!(everything.graph.rows(), 5);
    assert!(everything.graph.layout.lane_count() > 1);

    let first_parent = WalkOptions {
        first_parent: true,
        ..WalkOptions::default()
    };
    let loaded = load_repo(Some(first_parent), &cancel).unwrap();
    assert_eq!(loaded.walk, first_parent);
    assert_eq!(loaded.graph.rows(), 4);
    assert_eq!(loaded.graph.row_of(&side), None);
    let row = loaded.graph.row_of(&merge).unwrap();
    assert!(loaded.graph.merges[row], "still a merge");

    // Only main, first parents only: one straight line.
    let main_line = WalkOptions {
        first_parent: true,
        current_branch: true,
        topo_order: false,
    };
    let loaded = load_repo(Some(main_line), &cancel).unwrap();
    assert_eq!(loaded.graph.rows(), 3);
    assert_eq!(loaded.graph.merges, [true, false, false]);
    assert_eq!(loaded.graph.layout.lane_count(), 1);
}

#[test]
fn load_repo_with_an_explicit_tip_filter() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "f.txt", "base\n", "base");
    sb.run(&repo, &["switch", "-q", "-c", "wip"]);
    sb.commit_file(&repo, "w.txt", "wip\n", "wip 1");
    sb.run(&repo, &["switch", "-q", "-c", "feature", "main"]);
    sb.commit_file(&repo, "x.txt", "feature\n", "feature 1");
    sb.run(&repo, &["switch", "-q", "main"]);
    let wip = id(&sb, &repo, "wip");
    let feature = id(&sb, &repo, "feature");
    let cancel = CancelToken::new();
    let walk = Some(WalkOptions::default());

    let env = LoadEnv::isolated(sb.path().join("data"));
    let load_repo_with = |walk, tips, cancel| load_repo_in(&repo, &env, walk, tips, cancel);
    let everything = load_repo_with(walk, Some(TipFilter::default()), &cancel).unwrap();
    assert_eq!(everything.graph.rows(), 3);
    assert!(everything.tips.is_empty());

    let hidden = TipFilter {
        hidden: vec!["refs/heads/wip".into()],
        solo: None,
    };
    let loaded = load_repo_with(walk, Some(hidden.clone()), &cancel).unwrap();
    assert_eq!(loaded.tips, hidden);
    assert_eq!(loaded.graph.rows(), 2);
    assert_eq!(loaded.graph.row_of(&wip), None);
    assert!(loaded.graph.row_of(&feature).is_some());
    // The hidden branch's ref is still known (the sidebar lists it).
    assert!(
        loaded
            .refs
            .local_branches
            .iter()
            .any(|r| r.short_name == "wip")
    );

    let only_feature = TipFilter {
        hidden: Vec::new(),
        solo: Some("refs/heads/feature".into()),
    };
    let loaded = load_repo_with(walk, Some(only_feature), &cancel).unwrap();
    assert_eq!(loaded.graph.commits, [feature, id(&sb, &repo, "main")]);
}

#[test]
fn a_first_parent_load_folds_a_merged_branch_that_still_exists() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "f.txt", "base\n", "base");
    sb.run(&repo, &["switch", "-q", "-c", "side"]);
    sb.commit_file(&repo, "s.txt", "side\n", "side 1");
    sb.run(&repo, &["switch", "-q", "main"]);
    sb.commit_file(&repo, "f.txt", "main\n", "main 1");
    sb.run(
        &repo,
        &["merge", "-q", "--no-ff", "-m", "merge side", "side"],
    );
    let side = id(&sb, &repo, "side");
    let merge = id(&sb, &repo, "main");
    let cancel = CancelToken::new();
    let first_parent = WalkOptions {
        first_parent: true,
        ..WalkOptions::default()
    };
    let env = LoadEnv::isolated(sb.path().join("data"));
    let load = |walk, tips| load_repo_in(&repo, &env, Some(walk), Some(tips), &cancel).unwrap();

    let everything = load(WalkOptions::default(), TipFilter::default());
    assert_eq!(everything.graph.rows(), 4);
    // side's ref still exists, but its commit folds into the merge.
    let loaded = load(first_parent, TipFilter::default());
    assert_eq!(loaded.graph.rows(), 3);
    assert_eq!(loaded.graph.row_of(&side), None);
    assert!(loaded.graph.row_of(&merge).is_some());

    // A solo branch is walked even when merged.
    let solo = TipFilter {
        hidden: Vec::new(),
        solo: Some("refs/heads/side".into()),
    };
    let loaded = load(first_parent, solo);
    assert_eq!(loaded.graph.commits, [side, id(&sb, &repo, "main~1~1")]);

    // HEAD on the merged commit keeps it a tip.
    sb.run(&repo, &["switch", "-q", "--detach", "side"]);
    let loaded = load(first_parent, TipFilter::default());
    assert!(loaded.graph.row_of(&side).is_some());
    assert!(loaded.graph.row_of(&merge).is_some());
}
