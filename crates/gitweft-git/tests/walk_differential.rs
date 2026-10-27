//! Differential test of the two history walkers on generated histories: gix's and
//! `git rev-list`'s must give the same graph order, as `walk_cli.rs` checks on one
//! hand-made history. Branches, commits and `--no-ff` merges in random order; each commit
//! adds its own file, so merges never conflict; times increase by a minute a commit.

use std::path::Path;
use std::sync::atomic::AtomicBool;

use gitweft_git::CancelToken;
use gitweft_git::read::{
    TipFilter, WalkOptions, WalkedCommit, graph_tips, open_repo, read_refs, walk_history,
    walk_history_cli,
};
use gitweft_testkit::Sandbox;
use proptest::prelude::*;

/// One step of a generated history; the indices pick among the branches so far.
#[derive(Debug, Clone)]
enum Step {
    Commit(usize),
    Branch(usize),
    Merge { from: usize, into: usize },
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        3 => any::<usize>().prop_map(Step::Commit),
        1 => any::<usize>().prop_map(Step::Branch),
        1 => (any::<usize>(), any::<usize>()).prop_map(|(from, into)| Step::Merge { from, into }),
    ]
}

/// Runs git with the commit time `minute` minutes into 2026.
fn git_at(sb: &Sandbox, repo: &Path, minute: usize, args: &[&str]) {
    let date = format!("@{} +0000", 1_767_225_600 + minute * 60);
    sb.git()
        .command(repo)
        .args(args)
        .env("GIT_COMMITTER_DATE", &date)
        .env("GIT_AUTHOR_DATE", &date)
        .run(&CancelToken::new())
        .unwrap();
}

/// Builds the history of `steps` in a new repository; returns it.
fn build(sb: &Sandbox, steps: &[Step]) -> std::path::PathBuf {
    let repo = sb.init_repo("r");
    let mut branches = vec!["main".to_owned()];
    let mut minute = 0;
    let commit = |branch: Option<&str>, minute: &mut usize| {
        *minute += 1;
        if let Some(branch) = branch {
            sb.run(&repo, &["switch", "-q", branch]);
        }
        let file = format!("f{minute}");
        std::fs::write(repo.join(&file), format!("{minute}\n")).unwrap();
        sb.run(&repo, &["add", &file]);
        git_at(sb, &repo, *minute, &["commit", "-q", "-m", &file]);
    };
    // The repository is on its unborn `main`, which `git switch` can't name yet.
    commit(None, &mut minute);
    for step in steps {
        match *step {
            Step::Commit(n) => {
                let branch = branches[n % branches.len()].clone();
                commit(Some(&branch), &mut minute);
            }
            Step::Branch(n) => {
                let name = format!("b{}", branches.len());
                let start = branches[n % branches.len()].clone();
                sb.run(&repo, &["branch", &name, &start]);
                branches.push(name);
            }
            Step::Merge { from, into } => {
                let (from, into) = (
                    branches[from % branches.len()].clone(),
                    branches[into % branches.len()].clone(),
                );
                if from == into {
                    continue;
                }
                minute += 1;
                sb.run(&repo, &["switch", "-q", &into]);
                let message = format!("merge {from} into {into}");
                // Already merged: `--no-ff` still makes a merge commit of nothing new,
                // except when `from` is behind; `--allow-empty` isn't a merge option, so
                // an up-to-date merge is simply skipped.
                let merged = sb
                    .git()
                    .command(&repo)
                    .args(["merge-base", "--is-ancestor", &from, &into])
                    .allow_exit_codes(&[1])
                    .run(&CancelToken::new())
                    .unwrap()
                    .status
                    == 0;
                if !merged {
                    git_at(
                        sb,
                        &repo,
                        minute,
                        &["merge", "-q", "--no-ff", "-m", &message, &from],
                    );
                }
            }
        }
    }
    sb.run(&repo, &["switch", "-q", "main"]);
    repo
}

fn ids(walk: &[WalkedCommit]) -> Vec<(gitweft_git::ObjectId, Vec<gitweft_git::ObjectId>)> {
    walk.iter().map(|c| (c.id, c.parents.to_vec())).collect()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    /// Every walk option, with and without a commit-graph file: both walkers agree.
    #[test]
    fn both_walkers_agree_on_generated_histories(
        steps in prop::collection::vec(step(), 1..14),
        commit_graph in any::<bool>(),
    ) {
        let sb = Sandbox::new();
        let repo = build(&sb, &steps);
        if commit_graph {
            sb.run(&repo, &["commit-graph", "write", "--reachable"]);
        }
        let gix_repo = open_repo(&repo).unwrap();
        let refs = read_refs(&gix_repo).unwrap();
        for bits in 0..8 {
            let options = WalkOptions {
                first_parent: bits & 1 != 0,
                current_branch: bits & 2 != 0,
                topo_order: bits & 4 != 0,
            };
            let tips = graph_tips(&gix_repo, &refs, &options, &TipFilter::default());
            let via_gix =
                walk_history(&gix_repo, &tips, &options, &AtomicBool::new(false)).unwrap();
            let via_git =
                walk_history_cli(sb.git(), gix_repo.git_dir(), &tips, &options, &CancelToken::new())
                    .unwrap();
            // Without a commit-graph git sorts `--first-parent --topo-order` by its older
            // algorithm (see walk_cli.rs): the same commits, perhaps in another order.
            if options.first_parent && options.topo_order && !commit_graph {
                let (mut a, mut b) = (ids(&via_git), ids(&via_gix));
                a.sort();
                b.sort();
                prop_assert_eq!(a, b, "{:?}", options);
            } else {
                prop_assert_eq!(ids(&via_git), ids(&via_gix), "{:?}, commit-graph {}", options, commit_graph);
            }
        }
    }
}
