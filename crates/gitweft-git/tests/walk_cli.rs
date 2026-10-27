//! Both history walkers (gix and `git rev-list`) must produce the identical graph order.

use std::path::Path;
use std::sync::atomic::AtomicBool;

use gitweft_git::CancelToken;
use gitweft_git::read::{
    TipFilter, WalkOptions, graph_tips, open_repo, read_refs, walk_history, walk_history_cli,
};
use gitweft_testkit::Sandbox;

fn commit_at(sb: &Sandbox, repo: &Path, minute: u32, message: &str) {
    std::fs::write(repo.join("f"), format!("{message}\n")).unwrap();
    sb.run(repo, &["add", "f"]);
    let date = format!("@{} +0000", 1_767_225_600 + minute * 60);
    sb.git()
        .command(repo)
        .args(["commit", "-q", "-m", message])
        .env("GIT_COMMITTER_DATE", &date)
        .env("GIT_AUTHOR_DATE", &date)
        .run(&CancelToken::new())
        .unwrap();
}

#[test]
fn cli_walk_matches_gix_walk() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    commit_at(&sb, &repo, 1, "base");
    sb.run(&repo, &["checkout", "-q", "-b", "topic"]);
    commit_at(&sb, &repo, 2, "topic 1");
    sb.run(&repo, &["checkout", "-q", "main"]);
    commit_at(&sb, &repo, 3, "main 1");
    // Conflicting edits of `f`: resolve by taking ours, then commit the merge.
    let _ = sb
        .git()
        .command(&repo)
        .args(["merge", "-q", "--no-ff", "-m", "merge", "topic"])
        .run(&CancelToken::new());
    sb.run(&repo, &["checkout", "--ours", "f"]);
    sb.run(&repo, &["add", "f"]);
    commit_at(&sb, &repo, 4, "merge topic");
    sb.run(&repo, &["checkout", "-q", "-b", "later", "topic"]);
    commit_at(&sb, &repo, 5, "topic 2");
    sb.run(&repo, &["tag", "-a", "v2", "-m", "v2"]);
    // HEAD on main, so its history alone has the merge too.
    sb.run(&repo, &["checkout", "-q", "main"]);

    let ids = |walk: &[gitweft_git::read::WalkedCommit]| {
        walk.iter()
            .map(|c| (c.id, c.parents.to_vec()))
            .collect::<Vec<_>>()
    };
    // Every combination of the walk options, without and then with a commit-graph file
    // (git sorts by another algorithm when it has generation numbers).
    let mut gix_without_graph = Vec::new();
    for commit_graph in [false, true] {
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
            let via_git = walk_history_cli(
                sb.git(),
                gix_repo.git_dir(),
                &tips,
                &options,
                &CancelToken::new(),
            )
            .unwrap();
            // Without a commit-graph, git sorts `--first-parent --topo-order` by its older
            // algorithm, which still waits for the children behind un-walked second
            // parents (here `topic 1` comes after the merge; with a commit-graph, before
            // it). gix always sorts as git does with a commit-graph. Same commits, though.
            if options.first_parent && options.topo_order && !commit_graph {
                let mut a = ids(&via_git);
                let mut b = ids(&via_gix);
                a.sort();
                b.sort();
                assert_eq!(a, b, "{options:?}");
            } else {
                assert_eq!(
                    ids(&via_git),
                    ids(&via_gix),
                    "{options:?}, commit-graph {commit_graph}"
                );
            }
            // gix's order doesn't depend on the commit-graph file.
            if commit_graph {
                assert_eq!(ids(&via_gix), gix_without_graph[bits], "{options:?}");
            } else {
                gix_without_graph.push(ids(&via_gix));
            }
            // The merge keeps both parents, also when only first parents are followed.
            assert!(
                via_git.iter().any(|c| c.parents.len() == 2),
                "has a merge: {options:?}"
            );
        }
    }
}
