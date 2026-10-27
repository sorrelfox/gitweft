//! End-to-end interactive rebases: the plan runs through `git rebase -i`, with the real
//! gitweft binary as the sequence editor.

use std::path::{Path, PathBuf};

use gitweft_git::merge::{Operation, OperationAction, operation_state, run_operation_action};
use gitweft_git::rebase::{
    RebaseOutcome, RebasePlan, TodoAction, autosquash, check, plan, plan_before, rebase_interactive,
};
use gitweft_git::{CancelToken, GitError, GitErrorKind, ObjectId};
use gitweft_testkit::Sandbox;
use proptest::prelude::*;

const HELPER: &str = env!("CARGO_BIN_EXE_gitweft");

fn rev(sb: &Sandbox, repo: &Path, name: &str) -> String {
    sb.run(repo, &["rev-parse", name]).trim().to_owned()
}

/// Messages of the commits after `base`, oldest first.
fn messages(sb: &Sandbox, repo: &Path, base: &str) -> Vec<String> {
    let log = sb.run(
        repo,
        &[
            "log",
            "-z",
            "--reverse",
            "--format=%B",
            &format!("{base}..HEAD"),
        ],
    );
    log.split('\0')
        .filter(|m| !m.trim().is_empty())
        .map(|m| m.trim().to_owned())
        .collect()
}

/// Files each commit after `base` touches, oldest first.
fn files(sb: &Sandbox, repo: &Path, base: &str) -> Vec<Vec<String>> {
    let ids = sb.run(repo, &["rev-list", "--reverse", &format!("{base}..HEAD")]);
    ids.lines()
        .map(|id| {
            let names = sb.run(
                repo,
                &["diff-tree", "--no-commit-id", "--name-only", "-r", id],
            );
            let mut names: Vec<String> = names.lines().map(str::to_owned).collect();
            names.sort();
            names
        })
        .collect()
}

fn run(sb: &Sandbox, repo: &Path, plan: &RebasePlan) -> Result<RebaseOutcome, GitError> {
    rebase_interactive(sb.git(), repo, plan, Path::new(HELPER), &CancelToken::new())
}

fn rebasing(repo: &Path) -> bool {
    let gix_repo = gitweft_git::gix::open(repo).unwrap();
    operation_state(&gix_repo) == Some(Operation::Rebase)
}

fn continue_rebase(sb: &Sandbox, repo: &Path) {
    run_operation_action(
        sb.git(),
        repo,
        Operation::Rebase,
        OperationAction::Continue,
        &CancelToken::new(),
    )
    .unwrap();
}

/// A base commit, then one commit per name, each adding `<name>.txt`.
fn repo(sb: &Sandbox, names: &[&str]) -> (PathBuf, String) {
    let repo = sb.init_repo("r");
    let base = sb.commit_file(&repo, "base.txt", "base\n", "base");
    for name in names {
        sb.commit_file(&repo, &format!("{name}.txt"), &format!("{name}\n"), name);
    }
    (repo, base)
}

#[test]
fn reorder_reword_squash_fixup_and_drop() {
    use TodoAction::*;
    let sb = Sandbox::new();
    let (repo, base) = repo(&sb, &["a", "b", "c", "d", "e"]);
    let mut plan = plan(sb.git(), &repo, &base, &CancelToken::new()).unwrap();
    assert_eq!(plan.items.len(), 5);
    assert_eq!(plan.items[0].summary(), "a");
    // e first (reworded, with b squashed and c fixed up into it), then a; d dropped.
    let [a, b, c, d, e] = <[_; 5]>::try_from(plan.items.clone()).unwrap();
    let mut e = e;
    e.action = Reword;
    e.message = "E, reworded\n\nwith a body".into();
    plan.items = vec![
        e,
        gitweft_git::rebase::TodoItem {
            action: Squash,
            ..b
        },
        gitweft_git::rebase::TodoItem { action: Fixup, ..c },
        a,
        gitweft_git::rebase::TodoItem { action: Drop, ..d },
    ];
    assert_eq!(run(&sb, &repo, &plan).unwrap(), RebaseOutcome::Finished);

    assert_eq!(
        messages(&sb, &repo, &base),
        ["E, reworded\n\nwith a body", "a"]
    );
    assert_eq!(
        files(&sb, &repo, &base),
        [vec!["b.txt", "c.txt", "e.txt"], vec!["a.txt"]]
    );
    assert!(!repo.join("d.txt").exists());
    assert!(!repo.join(".git/gitweft-rebase").exists());
    let gix_repo = gitweft_git::gix::open(&repo).unwrap();
    assert_eq!(operation_state(&gix_repo), None);
}

#[test]
fn squash_keeps_both_messages() {
    use TodoAction::*;
    let sb = Sandbox::new();
    let (repo, base) = repo(&sb, &["a", "b"]);
    let mut plan = plan(sb.git(), &repo, &base, &CancelToken::new()).unwrap();
    plan.items[1].action = Squash;
    run(&sb, &repo, &plan).unwrap();
    assert_eq!(messages(&sb, &repo, &base), ["a\n\nb"]);
}

#[test]
fn a_branch_that_moved_since_the_plan_is_left_alone() {
    let sb = Sandbox::new();
    let (repo, base) = repo(&sb, &["a", "b"]);
    let mut plan = plan(sb.git(), &repo, &base, &CancelToken::new()).unwrap();
    plan.items.swap(0, 1);
    // A commit from a terminal in between: git's todo now has three picks.
    sb.commit_file(&repo, "late.txt", "late\n", "late");
    let head = rev(&sb, &repo, "HEAD");

    let err = run(&sb, &repo, &plan).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::RefMoved);
    assert_eq!(rev(&sb, &repo, "HEAD"), head);
    let gix_repo = gitweft_git::gix::open(&repo).unwrap();
    assert_eq!(operation_state(&gix_repo), None);
    assert!(!repo.join(".git/gitweft-rebase").exists());
}

#[test]
fn a_conflict_stops_and_a_pending_reword_still_applies_after_continue() {
    use TodoAction::*;
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let cancel = CancelToken::new();
    let base = sb.commit_file(&repo, "a.txt", "0\n", "base");
    sb.commit_file(&repo, "a.txt", "1\n", "one");
    sb.commit_file(&repo, "a.txt", "2\n", "two");
    sb.commit_file(&repo, "b.txt", "b\n", "three");
    let mut plan = plan(sb.git(), &repo, &base, &cancel).unwrap();
    // Without "one", "two" (1 → 2) no longer applies.
    plan.items[0].action = Drop;
    plan.items[2].action = Reword;
    plan.items[2].message = "three, reworded".into();

    let err = run(&sb, &repo, &plan).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::Conflict);
    let gix_repo = gitweft_git::gix::open(&repo).unwrap();
    assert_eq!(operation_state(&gix_repo), Some(Operation::Rebase));
    assert!(repo.join(".git/gitweft-rebase/2.msg").exists());

    std::fs::write(repo.join("a.txt"), "2\n").unwrap();
    sb.run(&repo, &["add", "a.txt"]);
    run_operation_action(
        sb.git(),
        &repo,
        Operation::Rebase,
        OperationAction::Continue,
        &cancel,
    )
    .unwrap();
    assert_eq!(messages(&sb, &repo, &base), ["two", "three, reworded"]);
}

#[test]
fn an_edit_stops_at_its_commit_and_continue_finishes() {
    use TodoAction::*;
    let sb = Sandbox::new();
    let (repo, base) = repo(&sb, &["a", "b", "c"]);
    let mut plan = plan(sb.git(), &repo, &base, &CancelToken::new()).unwrap();
    plan.items[1].action = Edit;

    assert_eq!(run(&sb, &repo, &plan).unwrap(), RebaseOutcome::Stopped);
    assert!(rebasing(&repo));
    assert_eq!(rev(&sb, &repo, "HEAD"), plan.items[1].commit.to_string());
    assert!(repo.join(".git/gitweft-rebase").exists());

    // Amend the stopped commit, as the user would.
    std::fs::write(repo.join("b2.txt"), "b2\n").unwrap();
    sb.run(&repo, &["add", "b2.txt"]);
    sb.run(&repo, &["commit", "-q", "--amend", "--no-edit"]);
    continue_rebase(&sb, &repo);
    assert!(!rebasing(&repo));
    assert_eq!(messages(&sb, &repo, &base), ["a", "b", "c"]);
    assert_eq!(
        files(&sb, &repo, &base),
        [vec!["a.txt"], vec!["b.txt", "b2.txt"], vec!["c.txt"]]
    );
}

#[test]
fn a_stop_after_a_reword_comes_once_the_message_and_folds_are_in() {
    use TodoAction::*;
    let sb = Sandbox::new();
    let (repo, base) = repo(&sb, &["a", "b", "c", "d"]);
    let mut plan = plan(sb.git(), &repo, &base, &CancelToken::new()).unwrap();
    plan.items[1].action = Reword;
    plan.items[1].message = "B, reworded".into();
    plan.items[1].stop_after = true;
    plan.items[2].action = Fixup;

    assert_eq!(run(&sb, &repo, &plan).unwrap(), RebaseOutcome::Stopped);
    assert!(rebasing(&repo));
    assert_eq!(messages(&sb, &repo, &base), ["a", "B, reworded"]);
    assert_eq!(
        files(&sb, &repo, &base),
        [vec!["a.txt"], vec!["b.txt", "c.txt"]]
    );

    continue_rebase(&sb, &repo);
    assert!(!rebasing(&repo));
    assert_eq!(messages(&sb, &repo, &base), ["a", "B, reworded", "d"]);
}

#[test]
fn autosquash_folds_a_fixup_commit_into_its_target() {
    use TodoAction::*;
    let sb = Sandbox::new();
    let cancel = CancelToken::new();
    let (repo, base) = repo(&sb, &["a", "b"]);
    let target = gitweft_git::ObjectId::from_hex(rev(&sb, &repo, "HEAD~1").as_bytes()).unwrap();
    std::fs::write(repo.join("a.txt"), "a, fixed\n").unwrap();
    sb.run(&repo, &["add", "a.txt"]);
    gitweft_git::rewrite::commit_fixup(sb.git(), &repo, target, &cancel).unwrap();

    let mut plan = plan(sb.git(), &repo, &base, &cancel).unwrap();
    plan.items = autosquash(&plan.items);
    let actions: Vec<_> = plan.items.iter().map(|i| i.action).collect();
    assert_eq!(actions, [Pick, Fixup, Pick]);
    assert_eq!(run(&sb, &repo, &plan).unwrap(), RebaseOutcome::Finished);

    assert_eq!(messages(&sb, &repo, &base), ["a", "b"]);
    assert_eq!(files(&sb, &repo, &base), [vec!["a.txt"], vec!["b.txt"]]);
    assert_eq!(sb.run(&repo, &["show", "HEAD~1:a.txt"]), "a, fixed\n");
}

#[test]
fn plans_refuse_other_branches_and_merges() {
    let sb = Sandbox::new();
    let (repo, base) = repo(&sb, &["a"]);
    let cancel = CancelToken::new();
    // `base` is the repository's first commit: nothing before it to plan after.
    assert!(plan(sb.git(), &repo, &format!("{base}^"), &cancel).is_err());
    sb.run(&repo, &["switch", "-q", "-c", "side", &base]);
    let side = sb.commit_file(&repo, "side.txt", "s\n", "side");
    sb.run(&repo, &["switch", "-q", "main"]);
    let err = plan(sb.git(), &repo, &side, &cancel).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::NotInHistory, "{}", err.stderr);

    sb.run(
        &repo,
        &["merge", "-q", "--no-ff", "-m", "merge side", "side"],
    );
    // HEAD is the merge: no straight line of commits follows it.
    let merges = GitErrorKind::MergesInRange {
        count: 1,
        after: None,
    };
    let err = plan(sb.git(), &repo, &base, &cancel).unwrap_err();
    assert_eq!(err.kind, merges, "{}", err.stderr);

    let err = plan(sb.git(), &repo, "HEAD", &cancel).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::NothingToRebase, "{}", err.stderr);
    // From the first commit on, the merge is still in the way.
    let first: ObjectId = base.parse().unwrap();
    let err = plan_before(sb.git(), &repo, first, 1, &cancel).unwrap_err();
    assert_eq!(err.kind, merges, "{}", err.stderr);
}

#[test]
fn merges_in_the_range_name_where_a_straight_plan_starts() {
    let sb = Sandbox::new();
    let (repo, base) = repo(&sb, &["a"]);
    let cancel = CancelToken::new();
    for side in ["one", "two"] {
        sb.run(&repo, &["switch", "-q", "-c", side, &base]);
        sb.commit_file(&repo, &format!("{side}.txt"), "s\n", side);
    }
    sb.run(&repo, &["switch", "-q", "main"]);
    for side in ["one", "two"] {
        let message = format!("merge {side}");
        sb.run(&repo, &["merge", "-q", "--no-ff", "-m", &message, side]);
    }
    let newest = rev(&sb, &repo, "HEAD");
    sb.commit_file(&repo, "b.txt", "b\n", "b");
    sb.commit_file(&repo, "c.txt", "c\n", "c");

    let err = plan(sb.git(), &repo, &base, &cancel).unwrap_err();
    let after = Some(newest.parse().unwrap());
    assert_eq!(
        err.kind,
        GitErrorKind::MergesInRange { count: 2, after },
        "{}",
        err.stderr
    );
    // The plan from there is the straight commits after it.
    let straight = plan(sb.git(), &repo, &newest, &cancel).unwrap();
    let summaries: Vec<_> = straight.items.iter().map(|item| item.summary()).collect();
    assert_eq!(summaries, ["b", "c"]);
}

#[test]
fn a_commit_too_close_to_the_first_one_is_planned_from_the_first_commit() {
    let sb = Sandbox::new();
    let (repo, base) = repo(&sb, &["a", "b"]);
    let cancel = CancelToken::new();
    let id = |name: &str| -> ObjectId { rev(&sb, &repo, name).parse().unwrap() };
    // b has a before it: the plan after a.
    let after_a = plan_before(sb.git(), &repo, id("HEAD"), 1, &cancel).unwrap();
    assert_eq!(after_a, plan(sb.git(), &repo, "HEAD~1", &cancel).unwrap());
    assert_eq!(after_a.base, Some(id("HEAD~1")));
    for (commit, back) in [(id("HEAD~1"), 2), (id(&base), 1), (id(&base), 5)] {
        let whole = plan_before(sb.git(), &repo, commit, back, &cancel).unwrap();
        assert_eq!(whole.base, None);
        let summaries: Vec<&str> = whole.items.iter().map(|item| item.summary()).collect();
        assert_eq!(summaries, ["base", "a", "b"]);
        assert_eq!(whole.original, [id(&base), id("HEAD~1"), id("HEAD")]);
    }
}

#[test]
fn the_first_commit_is_reworded_folded_into_and_dropped() {
    use TodoAction::*;
    let sb = Sandbox::new();
    let (repo, base) = repo(&sb, &["a", "b"]);
    let cancel = CancelToken::new();
    let subjects = || sb.run(&repo, &["log", "--reverse", "--format=%s"]);
    let tree = |rev: &str| sb.run(&repo, &["ls-tree", "--name-only", rev]);
    let roots = || sb.run(&repo, &["rev-list", "--max-parents=0", "HEAD"]);

    let mut whole = plan_before(sb.git(), &repo, base.parse().unwrap(), 1, &cancel).unwrap();
    whole.items[0].action = Reword;
    whole.items[0].message = "The start\n\nWith a body".into();
    assert_eq!(run(&sb, &repo, &whole).unwrap(), RebaseOutcome::Finished);
    assert_eq!(subjects(), "The start\na\nb\n");
    assert_eq!(roots().lines().count(), 1);
    assert_eq!(
        sb.run(&repo, &["log", "-1", "--format=%B", "HEAD~2"])
            .trim(),
        "The start\n\nWith a body"
    );

    // a folded into the first commit as a fixup: a's file, the first commit's message.
    let a = rev(&sb, &repo, "HEAD~1").parse().unwrap();
    let mut whole = plan_before(sb.git(), &repo, a, 2, &cancel).unwrap();
    whole.items[1].action = Fixup;
    assert_eq!(run(&sb, &repo, &whole).unwrap(), RebaseOutcome::Finished);
    assert_eq!(subjects(), "The start\nb\n");
    assert_eq!(tree("HEAD~1"), "a.txt\nbase.txt\n");

    // The first commit dropped: b is the first commit now, with only its own file.
    let first = roots().trim().parse().unwrap();
    let mut whole = plan_before(sb.git(), &repo, first, 1, &cancel).unwrap();
    whole.items[0].action = Drop;
    assert_eq!(run(&sb, &repo, &whole).unwrap(), RebaseOutcome::Finished);
    assert_eq!(subjects(), "b\n");
    assert_eq!(tree("HEAD"), "b.txt\n");
    assert_eq!(roots().trim(), rev(&sb, &repo, "HEAD"));
    assert!(!repo.join("a.txt").exists());
}

fn action() -> impl Strategy<Value = TodoAction> {
    prop_oneof![
        Just(TodoAction::Pick),
        Just(TodoAction::Reword),
        Just(TodoAction::Edit),
        Just(TodoAction::Squash),
        Just(TodoAction::Fixup),
        Just(TodoAction::Drop),
    ]
}

/// Commit `n` adds `c<n>.txt` with message `c<n>`, so any order applies cleanly. Each
/// item gets an action and whether to stop after it.
fn scenario() -> impl Strategy<Value = (Vec<usize>, Vec<(TodoAction, bool)>)> {
    (2usize..=5).prop_flat_map(|n| {
        (
            Just((0..n).collect::<Vec<_>>()).prop_shuffle(),
            prop::collection::vec((action(), prop::bool::weighted(0.2)), n),
        )
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    /// The rebased branch has exactly the commits a model of pick, reword, squash,
    /// fixup and drop predicts: their messages and the files each one touches. Edits and
    /// stops pause the rebase, and continuing from each pause changes nothing.
    #[test]
    fn rebases_match_the_model((order, specs) in scenario()) {
        let actions: Vec<TodoAction> = specs.iter().map(|&(action, _)| action).collect();
        let sb = Sandbox::new();
        let names: Vec<String> = (0..order.len()).map(|n| format!("c{n}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let (repo, base) = repo(&sb, &refs);
        let mut plan = plan(sb.git(), &repo, &base, &CancelToken::new()).unwrap();
        let original = plan.items.clone();
        plan.items = order
            .iter()
            .zip(&specs)
            .map(|(&n, &(action, stop_after))| {
                let mut item = original[n].clone();
                item.action = action;
                item.stop_after = stop_after;
                if action == TodoAction::Reword {
                    item.message = format!("r{n}\n\nnew body {n}");
                }
                item
            })
            .collect();
        prop_assume!(check(&plan.items).is_ok());

        // The model: groups of a kept commit and what folds into it.
        struct Group { files: Vec<String>, message: String, reworded: bool }
        let mut groups: Vec<Group> = Vec::new();
        for (&n, &action) in order.iter().zip(&actions) {
            let file = format!("c{n}.txt");
            match action {
                TodoAction::Drop => {}
                TodoAction::Pick | TodoAction::Edit => groups.push(Group { files: vec![file], message: format!("c{n}"), reworded: false }),
                TodoAction::Reword => groups.push(Group { files: vec![file], message: format!("r{n}\n\nnew body {n}"), reworded: true }),
                TodoAction::Squash | TodoAction::Fixup => {
                    let group = groups.last_mut().unwrap();
                    group.files.push(file);
                    if action == TodoAction::Squash && !group.reworded {
                        group.message.push_str(&format!("\n\nc{n}"));
                    }
                }
            }
        }
        for group in &mut groups {
            group.files.sort();
        }

        // At most one pause per edit and per stop (an edit that also stops pauses twice:
        // at the commit, then after its group).
        let pauses: usize = plan
            .items
            .iter()
            .map(|i| usize::from(i.action == TodoAction::Edit) + usize::from(i.stop_after))
            .sum();
        let mut outcome = run(&sb, &repo, &plan).unwrap();
        prop_assert_eq!(outcome == RebaseOutcome::Stopped, pauses > 0);
        let mut continued = 0;
        while outcome == RebaseOutcome::Stopped {
            prop_assert!(rebasing(&repo));
            prop_assert!(continued < pauses, "more pauses than edits and stops");
            continue_rebase(&sb, &repo);
            continued += 1;
            if !rebasing(&repo) {
                outcome = RebaseOutcome::Finished;
            }
        }
        let expected_messages: Vec<String> = groups.iter().map(|g| g.message.clone()).collect();
        let expected_files: Vec<Vec<String>> = groups.iter().map(|g| g.files.clone()).collect();
        prop_assert_eq!(messages(&sb, &repo, &base), expected_messages);
        prop_assert_eq!(files(&sb, &repo, &base), expected_files);
    }
}
