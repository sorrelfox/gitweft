//! The gix history walk must match `git rev-list --date-order` exactly.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use gitweft_git::CancelToken;
use gitweft_git::read::{
    TipFilter, WalkOptions, commit_meta, graph_tips, open_repo, read_refs, walk_history,
};
use gitweft_testkit::Sandbox;

/// Commits with strictly increasing committer dates, so date order is unambiguous.
struct Clock(u32);

impl Clock {
    fn commit(&mut self, sb: &Sandbox, repo: &Path, file: &str, message: &str) {
        self.0 += 1;
        std::fs::write(repo.join(file), format!("{message}\n")).unwrap();
        sb.run(repo, &["add", "--", file]);
        let date = format!("@{} +0000", 1_767_225_600 + self.0 * 60);
        sb.git()
            .command(repo)
            .args(["commit", "-q", "-m", message])
            .env("GIT_COMMITTER_DATE", &date)
            .env("GIT_AUTHOR_DATE", &date)
            .run(&CancelToken::new())
            .unwrap();
    }
}

fn rev_list(sb: &Sandbox, repo: &Path) -> Vec<String> {
    sb.run(
        repo,
        &[
            "rev-list",
            "--date-order",
            "HEAD",
            "--branches",
            "--remotes",
            "--tags",
        ],
    )
    .lines()
    .map(str::to_owned)
    .collect()
}

#[test]
fn walk_matches_git_rev_list_date_order() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let mut clock = Clock(0);
    clock.commit(&sb, &repo, "a", "base");
    sb.run(&repo, &["branch", "feature"]);
    clock.commit(&sb, &repo, "a", "main 1");
    sb.run(&repo, &["checkout", "-q", "feature"]);
    clock.commit(&sb, &repo, "b", "feature 1");
    clock.commit(&sb, &repo, "b", "feature 2");
    sb.run(&repo, &["checkout", "-q", "main"]);
    clock.commit(&sb, &repo, "a", "main 2");
    sb.run(
        &repo,
        &["merge", "-q", "--no-ff", "-m", "merge feature", "feature"],
    );
    sb.run(&repo, &["tag", "v1"]);
    // A tag on a tree must not break the walk.
    sb.run(&repo, &["tag", "tree-tag", "HEAD^{tree}"]);
    sb.run(&repo, &["checkout", "-q", "-b", "side", "HEAD~2"]);
    clock.commit(&sb, &repo, "c", "side 1");
    sb.run(&repo, &["checkout", "-q", "main"]);

    let gix_repo = open_repo(&repo).unwrap();
    let refs = read_refs(&gix_repo).unwrap();
    let tips = graph_tips(
        &gix_repo,
        &refs,
        &WalkOptions::default(),
        &TipFilter::default(),
    );
    let walked = walk_history(
        &gix_repo,
        &tips,
        &WalkOptions::default(),
        &AtomicBool::new(false),
    )
    .unwrap();

    let ours: Vec<String> = walked.iter().map(|c| c.id.to_string()).collect();
    assert_eq!(ours, rev_list(&sb, &repo));

    // Children always come before their parents.
    let row: HashMap<_, _> = walked.iter().enumerate().map(|(i, c)| (c.id, i)).collect();
    for (i, commit) in walked.iter().enumerate() {
        for parent in &commit.parents {
            assert!(row[parent] > i, "parent listed before child");
        }
    }

    let merge = walked.iter().find(|c| c.parents.len() == 2).unwrap();
    assert_eq!(
        &*commit_meta(&gix_repo, merge.id).unwrap().summary,
        "merge feature"
    );
}

#[test]
fn unborn_repository_has_no_history() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("empty");
    let gix_repo = open_repo(&repo).unwrap();
    let tips = graph_tips(
        &gix_repo,
        &read_refs(&gix_repo).unwrap(),
        &WalkOptions::default(),
        &TipFilter::default(),
    );
    assert!(tips.is_empty());
    assert!(
        walk_history(
            &gix_repo,
            &tips,
            &WalkOptions::default(),
            &AtomicBool::new(false)
        )
        .unwrap()
        .is_empty()
    );
}

#[test]
fn cancelled_walk_stops() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a", "1", "one");
    let gix_repo = open_repo(&repo).unwrap();
    let tips = graph_tips(
        &gix_repo,
        &read_refs(&gix_repo).unwrap(),
        &WalkOptions::default(),
        &TipFilter::default(),
    );
    assert!(
        walk_history(
            &gix_repo,
            &tips,
            &WalkOptions::default(),
            &AtomicBool::new(true)
        )
        .is_err()
    );
}
