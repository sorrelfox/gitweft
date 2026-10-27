use std::path::Path;

use gitweft_git::read::{
    HARDER_CREATION_FACTOR, InterdiffKind, RangeChange, interdiff_rows, interdiff_section,
    range_diff,
};
use gitweft_git::{CancelToken, ObjectId};
use gitweft_testkit::Sandbox;

fn id(sb: &Sandbox, repo: &Path, rev: &str) -> ObjectId {
    let hex = sb.run(repo, &["rev-parse", rev]);
    ObjectId::from_hex(hex.trim().as_bytes()).unwrap()
}

fn numbers(except: Option<(u32, &str)>) -> String {
    (1..=30)
        .map(|n| match except {
            Some((line, text)) if line == n => format!("{text}\n"),
            _ => format!("{n}\n"),
        })
        .collect()
}

/// `topic` with "add a", "add b" (30 lines) and "add c" on `main`; then `main` moves,
/// and `topic` is rebased onto it with one line of "add b" changed, "add c" dropped and
/// "add d" added.
#[test]
fn a_rebased_branch_pairs_same_changed_removed_and_added_commits() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "base.txt", "base\n", "base");
    sb.run(&repo, &["switch", "-q", "-c", "topic"]);
    sb.commit_file(&repo, "a.txt", "a\n", "add a");
    sb.commit_file(&repo, "b.txt", &numbers(None), "add b");
    sb.commit_file(&repo, "c.txt", "c\n", "add c");
    let (old_base, old_head) = (id(&sb, &repo, "main"), id(&sb, &repo, "topic"));
    let old_a = id(&sb, &repo, "topic~2");

    sb.run(&repo, &["switch", "-q", "main"]);
    sb.commit_file(&repo, "m.txt", "m\n", "main moves");
    sb.run(&repo, &["switch", "-q", "topic"]);
    sb.run(&repo, &["rebase", "-q", "main"]);
    sb.run(&repo, &["reset", "-q", "--hard", "HEAD~1"]);
    std::fs::write(repo.join("b.txt"), numbers(Some((17, "seventeen")))).unwrap();
    sb.run(&repo, &["commit", "-q", "--amend", "-am", "add b"]);
    sb.commit_file(&repo, "d.txt", "d\n", "add d");
    let (new_base, new_head) = (id(&sb, &repo, "main"), id(&sb, &repo, "topic"));

    let pairs = range_diff(
        sb.git(),
        &repo,
        (&old_base, &old_head),
        (&new_base, &new_head),
        None,
        &CancelToken::new(),
    )
    .unwrap();
    let kinds: Vec<(RangeChange, &str)> = pairs
        .iter()
        .map(|pair| (pair.change, pair.summary.as_str()))
        .collect();
    assert_eq!(
        kinds,
        [
            (RangeChange::Same, "add a"),
            (RangeChange::Changed, "add b"),
            (RangeChange::Removed, "add c"),
            (RangeChange::Added, "add d"),
        ]
    );
    assert_eq!(pairs[0].old, Some((1, old_a)));
    assert_eq!(pairs[0].new.map(|(n, _)| n), Some(1));
    assert_eq!(pairs[3].new, Some((3, new_head)));
    let interdiff = pairs[1].interdiff.join("\n");
    assert!(interdiff.contains("-+17"), "{interdiff}");
    assert!(interdiff.contains("++seventeen"), "{interdiff}");

    // Numbered from both sides (#69), on git's own output: every line is in one patch or
    // both, the two `17` lines are one per side, and the file marker names b.txt.
    let rows = interdiff_rows(&pairs[1].interdiff);
    assert_eq!(rows.len(), pairs[1].interdiff.len());
    for row in &rows {
        let has = |side: Option<u32>| side.is_some();
        match row.kind {
            InterdiffKind::Header => assert!(!has(row.old) && !has(row.new), "{row:?}"),
            InterdiffKind::Context => assert!(has(row.old) && has(row.new), "{row:?}"),
            InterdiffKind::Removed => assert!(has(row.old) && !has(row.new), "{row:?}"),
            InterdiffKind::Added => assert!(!has(row.old) && has(row.new), "{row:?}"),
        }
    }
    let numbered: Vec<(Option<u32>, Option<u32>, &str)> = rows
        .iter()
        .filter(|row| row.text.ends_with("17") || row.text.ends_with("seventeen"))
        .map(|row| (row.old, row.new, row.text.as_str()))
        .collect();
    let old_17 = numbered[0].0.expect("the old patch has +17");
    let new_17 = numbered[1].1.expect("the new patch has +seventeen");
    assert_eq!(
        numbered,
        [
            (Some(old_17), None, "+17"),
            (None, Some(new_17), "+seventeen"),
        ]
    );
    // The line each patch has of its own sits at the same place in both.
    assert_eq!(old_17, new_17);
    // Only b.txt's hunk changed, so git's own hunk header names the section.
    let sections: Vec<&str> = rows
        .iter()
        .filter_map(|row| interdiff_section(&row.text))
        .collect();
    assert_eq!(sections, ["b.txt"], "{rows:#?}");
}

#[test]
fn an_unchanged_branch_is_all_the_same() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "base.txt", "base\n", "base");
    sb.run(&repo, &["switch", "-q", "-c", "topic"]);
    sb.commit_file(&repo, "a.txt", "a\n", "add a");
    sb.commit_file(&repo, "b.txt", "b\n", "add b");
    let (base, head) = (id(&sb, &repo, "main"), id(&sb, &repo, "topic"));
    let pairs = range_diff(
        sb.git(),
        &repo,
        (&base, &head),
        (&base, &head),
        None,
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(pairs.len(), 2);
    assert!(
        pairs
            .iter()
            .all(|pair| pair.change == RangeChange::Same && pair.old == pair.new)
    );
}

/// "add b" rewritten in two of every three lines: git's default factor calls it another
/// commit (removed, added); [`HARDER_CREATION_FACTOR`] pairs it as one changed commit.
#[test]
fn a_heavily_reworked_commit_pairs_only_when_matched_harder() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "base.txt", "base\n", "base");
    sb.run(&repo, &["switch", "-q", "-c", "topic"]);
    sb.commit_file(&repo, "b.txt", &numbers(None), "add b");
    let (base, old) = (id(&sb, &repo, "main"), id(&sb, &repo, "topic"));
    sb.run(&repo, &["reset", "-q", "--hard", "HEAD~1"]);
    let reworked: String = (1..=30)
        .map(|n| match n % 3 {
            0 => format!("{n}\n"),
            _ => format!("line {n} rewritten\n"),
        })
        .collect();
    sb.commit_file(&repo, "b.txt", &reworked, "add b");
    let new = id(&sb, &repo, "topic");

    let kinds = |factor: Option<u32>| -> Vec<RangeChange> {
        range_diff(
            sb.git(),
            &repo,
            (&base, &old),
            (&base, &new),
            factor,
            &CancelToken::new(),
        )
        .unwrap()
        .iter()
        .map(|pair| pair.change)
        .collect()
    };
    assert_eq!(kinds(None), [RangeChange::Removed, RangeChange::Added]);
    assert_eq!(kinds(Some(HARDER_CREATION_FACTOR)), [RangeChange::Changed]);
}
