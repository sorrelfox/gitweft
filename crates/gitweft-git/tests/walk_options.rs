//! The graph's walk options: first parents only, the current branch only, topological
//! order. Both walkers (gix and `git rev-list`) must agree with git for each of them.

use std::path::Path;
use std::sync::atomic::AtomicBool;

use gitweft_git::read::{
    HeadInfo, TipFilter, WalkOptions, WalkedCommit, graph_tips, read_refs, unmerged_tips,
    walk_history, walk_history_cli,
};
use gitweft_git::{CancelToken, ObjectId, gix};
use gitweft_testkit::Sandbox;

/// Runs git with both dates set to `minute` minutes after a fixed epoch.
fn git_at(sb: &Sandbox, repo: &Path, minute: u32, args: &[&str]) {
    let date = format!("@{} +0000", 1_767_225_600 + minute * 60);
    sb.git()
        .command(repo)
        .args(args)
        .env("GIT_COMMITTER_DATE", &date)
        .env("GIT_AUTHOR_DATE", &date)
        .run(&CancelToken::new())
        .unwrap();
}

/// Commits `file` (content = message) at `minute`.
fn commit_at(sb: &Sandbox, repo: &Path, minute: u32, file: &str, message: &str) -> ObjectId {
    std::fs::write(repo.join(file), format!("{message}\n")).unwrap();
    sb.run(repo, &["add", "--", file]);
    git_at(sb, repo, minute, &["commit", "-q", "-m", message]);
    id(sb, repo, "HEAD")
}

fn id(sb: &Sandbox, repo: &Path, rev: &str) -> ObjectId {
    sb.run(repo, &["rev-parse", rev]).trim().parse().unwrap()
}

fn open(repo: &Path) -> gix::Repository {
    gix::open_opts(repo, gix::open::Options::isolated()).unwrap()
}

/// Walks with both walkers, checks they agree, and returns the gix walk.
fn walk(repo: &Path, sb: &Sandbox, options: WalkOptions) -> Vec<WalkedCommit> {
    walk_filtered(repo, sb, options, &TipFilter::default())
}

/// [`walk`] from the tips `filter` leaves.
fn walk_filtered(
    repo: &Path,
    sb: &Sandbox,
    options: WalkOptions,
    filter: &TipFilter,
) -> Vec<WalkedCommit> {
    let gix_repo = open(repo);
    let tips = graph_tips(&gix_repo, &read_refs(&gix_repo).unwrap(), &options, filter);
    walk_tips(repo, sb, &tips, options)
}

/// [`walk`] from these `tips`.
fn walk_tips(
    repo: &Path,
    sb: &Sandbox,
    tips: &[ObjectId],
    options: WalkOptions,
) -> Vec<WalkedCommit> {
    let gix_repo = open(repo);
    let via_gix = walk_history(&gix_repo, tips, &options, &AtomicBool::new(false)).unwrap();
    let via_git = walk_history_cli(
        sb.git(),
        gix_repo.git_dir(),
        tips,
        &options,
        &CancelToken::new(),
    )
    .unwrap();
    let pairs = |walk: &[WalkedCommit]| {
        walk.iter()
            .map(|c| (c.id, c.parents.to_vec()))
            .collect::<Vec<_>>()
    };
    assert_eq!(pairs(&via_gix), pairs(&via_git), "{options:?}");
    via_gix
}

fn ids(walk: &[WalkedCommit]) -> Vec<ObjectId> {
    walk.iter().map(|c| c.id).collect()
}

/// `git rev-list <args>` as ids.
fn rev_list(sb: &Sandbox, repo: &Path, args: &[&str]) -> Vec<ObjectId> {
    let mut all = vec!["rev-list"];
    all.extend_from_slice(args);
    sb.run(repo, &all)
        .lines()
        .map(|line| line.parse().unwrap())
        .collect()
}

struct History {
    base: ObjectId,
    side: [ObjectId; 2],
    main1: ObjectId,
    merge: ObjectId,
    other: ObjectId,
    tagged: ObjectId,
}

/// main: base, main1, then a `--no-ff` merge of `side` (two commits, branch deleted
/// after the merge); `other` forks
/// from base and is never merged; `lonely` tags a commit no branch contains. HEAD is
/// on main.
fn history(sb: &Sandbox) -> (std::path::PathBuf, History) {
    let repo = sb.init_repo("r");
    let base = commit_at(sb, &repo, 1, "base", "base");
    sb.run(&repo, &["switch", "-q", "-c", "side"]);
    let side1 = commit_at(sb, &repo, 2, "s", "side 1");
    let side2 = commit_at(sb, &repo, 3, "s", "side 2");
    sb.run(&repo, &["switch", "-q", "main"]);
    let main1 = commit_at(sb, &repo, 4, "m", "main 1");
    git_at(
        sb,
        &repo,
        5,
        &["merge", "-q", "--no-ff", "-m", "merge side", "side"],
    );
    let merge = id(sb, &repo, "HEAD");
    // Merged and gone: its commits are only reachable through the merge's second parent.
    sb.run(&repo, &["branch", "-q", "-D", "side"]);
    sb.run(&repo, &["switch", "-q", "-c", "other", &base.to_string()]);
    let other = commit_at(sb, &repo, 6, "o", "other");
    sb.run(&repo, &["switch", "-q", "--detach", "main"]);
    let tagged = commit_at(sb, &repo, 7, "t", "tagged");
    sb.run(&repo, &["tag", "lonely"]);
    sb.run(&repo, &["switch", "-q", "main"]);
    let history = History {
        base,
        side: [side1, side2],
        main1,
        merge,
        other,
        tagged,
    };
    (repo, history)
}

#[test]
fn first_parent_skips_the_merged_branch_but_keeps_the_merge() {
    let sb = Sandbox::new();
    let (repo, h) = history(&sb);
    let options = WalkOptions {
        first_parent: true,
        ..WalkOptions::default()
    };
    let walked = walk(&repo, &sb, options);
    let walked_ids = ids(&walked);
    assert!(!walked_ids.contains(&h.side[0]) && !walked_ids.contains(&h.side[1]));
    for commit in [h.base, h.main1, h.merge, h.other, h.tagged] {
        assert!(walked_ids.contains(&commit));
    }
    let merge = walked.iter().find(|c| c.id == h.merge).unwrap();
    assert_eq!(merge.parents.to_vec(), [h.main1, h.side[1]]);
    assert_eq!(
        walked_ids,
        rev_list(
            &sb,
            &repo,
            &[
                "--first-parent",
                "--date-order",
                "HEAD",
                "--branches",
                "--tags"
            ]
        )
    );

    // A merge as a tip (HEAD alone) still lists both parents.
    let head_only = walk(
        &repo,
        &sb,
        WalkOptions {
            current_branch: true,
            ..options
        },
    );
    assert_eq!(ids(&head_only), [h.merge, h.main1, h.base]);
    assert_eq!(head_only[0].parents.len(), 2);
}

#[test]
fn current_branch_walks_what_head_reaches() {
    let sb = Sandbox::new();
    let (repo, h) = history(&sb);
    let options = WalkOptions {
        current_branch: true,
        ..WalkOptions::default()
    };
    let gix_repo = open(&repo);
    let refs = read_refs(&gix_repo).unwrap();
    assert_eq!(
        graph_tips(&gix_repo, &refs, &options, &TipFilter::default()),
        [h.merge]
    );
    let walked = ids(&walk(&repo, &sb, options));
    assert_eq!(walked, [h.merge, h.main1, h.side[1], h.side[0], h.base]);
    assert!(!walked.contains(&h.other) && !walked.contains(&h.tagged));

    // Detached HEAD: its commit is the one tip.
    sb.run(&repo, &["switch", "-q", "--detach", &h.side[1].to_string()]);
    assert_eq!(
        ids(&walk(&repo, &sb, options)),
        [h.side[1], h.side[0], h.base]
    );

    // Unborn HEAD (an orphan branch in a repository with history): nothing to walk,
    // though the other branches still give tips without the option.
    sb.run(&repo, &["switch", "-q", "--orphan", "fresh"]);
    let gix_repo = open(&repo);
    let refs = read_refs(&gix_repo).unwrap();
    assert!(graph_tips(&gix_repo, &refs, &options, &TipFilter::default()).is_empty());
    assert!(
        !graph_tips(
            &gix_repo,
            &refs,
            &WalkOptions::default(),
            &TipFilter::default()
        )
        .is_empty()
    );
    assert!(walk(&repo, &sb, options).is_empty());
}

#[test]
fn topo_order_keeps_each_line_of_history_together() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    // Two branches whose commits alternate in time, both merged into main.
    let base = commit_at(&sb, &repo, 1, "base", "base");
    sb.run(&repo, &["branch", "a"]);
    sb.run(&repo, &["branch", "b"]);
    let mut minute = 1;
    let mut on = |branch: &str, file: &str, message: &str| {
        minute += 1;
        sb.run(&repo, &["switch", "-q", branch]);
        commit_at(&sb, &repo, minute, file, message)
    };
    let a1 = on("a", "a", "a 1");
    let b1 = on("b", "b", "b 1");
    let a2 = on("a", "a", "a 2");
    let b2 = on("b", "b", "b 2");
    let a3 = on("a", "a", "a 3");
    sb.run(&repo, &["switch", "-q", "main"]);
    git_at(
        &sb,
        &repo,
        20,
        &["merge", "-q", "--no-ff", "-m", "merge a", "a"],
    );
    git_at(
        &sb,
        &repo,
        21,
        &["merge", "-q", "--no-ff", "-m", "merge b", "b"],
    );
    // An unmerged branch whose commit is older than the merges. It forks from base: a
    // child of a1 could rightly come out between a2 and a1.
    sb.run(&repo, &["switch", "-q", "-c", "c", &base.to_string()]);
    let c1 = on("c", "c", "c 1");
    sb.run(&repo, &["switch", "-q", "main"]);

    let date = WalkOptions::default();
    let topo = WalkOptions {
        topo_order: true,
        ..date
    };
    let rev_list_all = |order: &str| rev_list(&sb, &repo, &[order, "HEAD", "--branches", "--tags"]);
    for commit_graph in [false, true] {
        if commit_graph {
            sb.run(&repo, &["commit-graph", "write", "--reachable"]);
        }
        let by_date = ids(&walk(&repo, &sb, date));
        let by_topo = ids(&walk(&repo, &sb, topo));
        assert_eq!(by_date, rev_list_all("--date-order"));
        assert_eq!(
            by_topo,
            rev_list_all("--topo-order"),
            "commit-graph {commit_graph}"
        );

        // Date order interleaves a and b; topological order doesn't.
        let at = |walk: &[ObjectId], id| walk.iter().position(|c| *c == id).unwrap();
        assert!(at(&by_date, b2) < at(&by_date, a2) && at(&by_date, a2) < at(&by_date, b1));
        let b_rows = [at(&by_topo, b2), at(&by_topo, b1)];
        let a_rows = [at(&by_topo, a3), at(&by_topo, a2), at(&by_topo, a1)];
        assert_eq!(b_rows[1], b_rows[0] + 1, "{by_topo:?}");
        assert!(a_rows.windows(2).all(|w| w[1] == w[0] + 1), "{by_topo:?}");
        assert_eq!(*by_topo.last().unwrap(), base);
        assert!(by_topo.contains(&c1));
    }
}

fn hiding(names: &[&str]) -> TipFilter {
    TipFilter {
        hidden: names.iter().map(|name| (*name).to_owned()).collect(),
        solo: None,
    }
}

fn solo(name: &str) -> TipFilter {
    TipFilter {
        hidden: Vec::new(),
        solo: Some(name.to_owned()),
    }
}

/// The default walk from the tips `filter` leaves, as ids.
fn filtered(sb: &Sandbox, repo: &Path, filter: &TipFilter) -> Vec<ObjectId> {
    ids(&walk_filtered(repo, sb, WalkOptions::default(), filter))
}

#[test]
fn hides_names_exactly_or_everything_under_a_slash() {
    let filter = hiding(&["refs/heads/feat", "refs/remotes/origin/"]);
    assert!(filter.hides(b"refs/heads/feat"));
    assert!(!filter.hides(b"refs/heads/feature"));
    assert!(!filter.hides(b"refs/heads/feat/x"));
    assert!(filter.hides(b"refs/remotes/origin/main"));
    assert!(filter.hides(b"refs/remotes/origin/a/b"));
    assert!(!filter.hides(b"refs/remotes/origin"));
    assert!(!filter.hides(b"refs/remotes/originals/main"));
    // Bytes, not text: a non-UTF-8 name never equals an entry, but can be under one.
    assert!(!filter.hides(b"refs/heads/fe\xffat"));
    assert!(filter.hides(b"refs/remotes/origin/\xff"));

    assert!(!filter.is_empty());
    assert!(TipFilter::default().is_empty());
    assert!(!solo("refs/heads/x").is_empty());
}

#[test]
fn a_hidden_branch_loses_only_its_own_commits() {
    let sb = Sandbox::new();
    let (repo, h) = history(&sb);
    let everything = filtered(&sb, &repo, &TipFilter::default());
    assert!(everything.contains(&h.other));

    let walked = filtered(&sb, &repo, &hiding(&["refs/heads/other"]));
    assert!(!walked.contains(&h.other));
    // Its parent is still reachable from main.
    assert!(walked.contains(&h.base));
    assert_eq!(
        walked,
        rev_list(&sb, &repo, &["--date-order", "HEAD", "refs/tags/lonely"])
    );

    // Another branch on the same commit keeps it a tip.
    sb.run(&repo, &["branch", "copy", "other"]);
    let walked = filtered(&sb, &repo, &hiding(&["refs/heads/other"]));
    assert!(walked.contains(&h.other));

    // A hidden tag's commit, which no branch contains, vanishes.
    let walked = filtered(&sb, &repo, &hiding(&["refs/tags/lonely"]));
    assert!(!walked.contains(&h.tagged));
    assert!(walked.contains(&h.other));
}

#[test]
fn a_remote_prefix_hides_every_branch_of_that_remote() {
    let sb = Sandbox::new();
    let (repo, h) = history(&sb);
    // Only remote branches point at `other` and `tagged` now.
    for (name, target) in [
        ("refs/remotes/origin/o", h.other),
        ("refs/remotes/origin/t", h.tagged),
        ("refs/remotes/upstream/t", h.tagged),
    ] {
        sb.run(&repo, &["update-ref", name, &target.to_string()]);
    }
    sb.run(&repo, &["branch", "-q", "-D", "other"]);
    sb.run(&repo, &["tag", "-d", "lonely"]);
    let everything = filtered(&sb, &repo, &TipFilter::default());
    assert!(everything.contains(&h.other) && everything.contains(&h.tagged));

    let walked = filtered(&sb, &repo, &hiding(&["refs/remotes/origin/"]));
    assert!(!walked.contains(&h.other));
    // Still on upstream.
    assert!(walked.contains(&h.tagged));
    let walked = filtered(
        &sb,
        &repo,
        &hiding(&["refs/remotes/origin/", "refs/remotes/upstream/"]),
    );
    assert_eq!(walked, rev_list(&sb, &repo, &["--date-order", "HEAD"]));
}

#[test]
fn the_checked_out_commit_stays_when_its_branch_is_hidden() {
    let sb = Sandbox::new();
    let (repo, h) = history(&sb);
    let gix_repo = open(&repo);
    let refs = read_refs(&gix_repo).unwrap();
    let filter = hiding(&["refs/heads/main", "refs/heads/other", "refs/tags/"]);
    assert_eq!(
        graph_tips(&gix_repo, &refs, &WalkOptions::default(), &filter),
        [h.merge]
    );
    assert_eq!(
        filtered(&sb, &repo, &filter),
        [h.merge, h.main1, h.side[1], h.side[0], h.base]
    );
}

#[test]
fn solo_walks_one_branch_and_a_missing_solo_walks_everything() {
    let sb = Sandbox::new();
    let (repo, h) = history(&sb);
    let current = WalkOptions {
        current_branch: true,
        ..WalkOptions::default()
    };
    // The solo ref wins over current_branch (and over its own entry in `hidden`).
    let mut filter = solo("refs/heads/other");
    filter.hidden.push("refs/heads/other".into());
    for options in [WalkOptions::default(), current] {
        assert_eq!(
            ids(&walk_filtered(&repo, &sb, options, &filter)),
            [h.other, h.base]
        );
    }
    assert_eq!(
        filtered(&sb, &repo, &solo("refs/tags/lonely")),
        rev_list(&sb, &repo, &["--date-order", "lonely"])
    );

    // No such ref (a branch deleted since, or a short name): as if there were no solo.
    for missing in ["refs/heads/gone", "other"] {
        assert_eq!(
            filtered(&sb, &repo, &solo(missing)),
            filtered(&sb, &repo, &TipFilter::default())
        );
        assert_eq!(
            ids(&walk_filtered(&repo, &sb, current, &solo(missing))),
            [h.merge, h.main1, h.side[1], h.side[0], h.base]
        );
    }
}

const FIRST_PARENT: WalkOptions = WalkOptions {
    first_parent: true,
    current_branch: false,
    topo_order: false,
};

struct Merged {
    base: ObjectId,
    topic: [ObjectId; 2],
    main1: ObjectId,
    merge: ObjectId,
    wip: ObjectId,
}

/// main: base, main1, then a `--no-ff` merge of `topic` (two commits; the branch stays);
/// `old` points at main1, behind main on its first-parent chain; `wip` forks from base
/// and is never merged. HEAD is on main.
fn merged_history(sb: &Sandbox) -> (std::path::PathBuf, Merged) {
    let repo = sb.init_repo("r");
    let base = commit_at(sb, &repo, 1, "base", "base");
    sb.run(&repo, &["switch", "-q", "-c", "topic"]);
    let topic1 = commit_at(sb, &repo, 2, "t", "topic 1");
    let topic2 = commit_at(sb, &repo, 3, "t", "topic 2");
    sb.run(&repo, &["switch", "-q", "main"]);
    let main1 = commit_at(sb, &repo, 4, "m", "main 1");
    sb.run(&repo, &["branch", "old"]);
    git_at(
        sb,
        &repo,
        5,
        &["merge", "-q", "--no-ff", "-m", "merge topic", "topic"],
    );
    let merge = id(sb, &repo, "HEAD");
    sb.run(&repo, &["switch", "-q", "-c", "wip", &base.to_string()]);
    let wip = commit_at(sb, &repo, 6, "w", "wip");
    sb.run(&repo, &["switch", "-q", "main"]);
    let merged = Merged {
        base,
        topic: [topic1, topic2],
        main1,
        merge,
        wip,
    };
    (repo, merged)
}

/// Every ref's tip (as `graph_tips` gives them), and those `unmerged_tips` keeps, with
/// HEAD's commit (`None`: no commit) kept when `keep_head`.
fn first_parent_tips(repo: &Path, keep_head: bool) -> (Vec<ObjectId>, Vec<ObjectId>) {
    let gix_repo = open(repo);
    let refs = read_refs(&gix_repo).unwrap();
    let all = graph_tips(&gix_repo, &refs, &FIRST_PARENT, &TipFilter::default());
    let head = match &refs.head {
        HeadInfo::Branch { oid, .. } | HeadInfo::Detached { oid } => Some(*oid),
        HeadInfo::Unborn { .. } => None,
    };
    let keep = head.filter(|_| keep_head);
    let unmerged = unmerged_tips(&gix_repo, &all, keep, &AtomicBool::new(false)).unwrap();
    (all, unmerged)
}

#[test]
fn first_parent_folds_a_merged_branch_into_its_merge() {
    let sb = Sandbox::new();
    let (repo, h) = merged_history(&sb);
    for commit_graph in [false, true] {
        if commit_graph {
            sb.run(&repo, &["commit-graph", "write", "--reachable"]);
        }
        let (all, tips) = first_parent_tips(&repo, true);
        // topic (merged) and old (behind main) are in main's history; wip isn't.
        assert!(all.contains(&h.topic[1]) && all.contains(&h.main1));
        let mut expected = vec![h.merge, h.wip];
        expected.sort();
        assert_eq!(tips, expected, "commit-graph {commit_graph}");

        let walked = ids(&walk_tips(&repo, &sb, &tips, FIRST_PARENT));
        // Only a second parent reaches topic's commits: they fold into the merge.
        assert!(!walked.contains(&h.topic[0]) && !walked.contains(&h.topic[1]));
        // old's commit is on main's first-parent chain, so it is still walked; so is
        // the unmerged branch.
        for commit in [h.base, h.main1, h.merge, h.wip] {
            assert!(walked.contains(&commit));
        }
        assert_eq!(
            walked,
            rev_list(
                &sb,
                &repo,
                &["--first-parent", "--date-order", "main", "wip"]
            )
        );

        // git's `--first-parent --all` walks the merged branch from its own ref.
        let every_ref = ids(&walk_tips(&repo, &sb, &all, FIRST_PARENT));
        assert!(every_ref.contains(&h.topic[0]) && every_ref.contains(&h.topic[1]));
    }
}

#[test]
fn a_detached_head_on_a_merged_commit_stays_a_tip() {
    let sb = Sandbox::new();
    let (repo, h) = merged_history(&sb);
    sb.run(
        &repo,
        &["switch", "-q", "--detach", &h.topic[1].to_string()],
    );

    let (_, tips) = first_parent_tips(&repo, true);
    assert!(tips.contains(&h.topic[1]));
    let walked = ids(&walk_tips(&repo, &sb, &tips, FIRST_PARENT));
    for commit in [h.topic[1], h.topic[0], h.base, h.merge] {
        assert!(walked.contains(&commit));
    }

    // Not kept, it is merged like any other.
    let (_, tips) = first_parent_tips(&repo, false);
    assert!(!tips.contains(&h.topic[1]));
}

#[test]
fn unmerged_tips_leaves_a_single_tip_and_stops_when_cancelled() {
    let sb = Sandbox::new();
    let (repo, h) = merged_history(&sb);
    let gix_repo = open(&repo);
    let running = AtomicBool::new(false);
    assert_eq!(
        unmerged_tips(&gix_repo, &[h.topic[1]], None, &running).unwrap(),
        [h.topic[1]]
    );
    assert!(
        unmerged_tips(&gix_repo, &[], None, &running)
            .unwrap()
            .is_empty()
    );
    // A tip given twice isn't in its own history.
    assert_eq!(
        unmerged_tips(&gix_repo, &[h.wip, h.merge, h.wip], None, &running).unwrap(),
        [h.wip, h.merge, h.wip]
    );
    let cancelled = AtomicBool::new(true);
    assert!(unmerged_tips(&gix_repo, &[h.merge, h.wip], None, &cancelled).is_err());
}
