use std::collections::BTreeMap;
use std::time::Instant;

use gitweft_graph::{ConnectorKind, Layout, LayoutBuilder, render_ascii};
use proptest::prelude::*;

/// Builds a layout from `(id, parents)` rows given in topological order.
fn build(rows: &[(u32, &[u32])]) -> Layout {
    let mut builder = LayoutBuilder::new();
    for (id, parents) in rows {
        builder.push(*id, parents, None);
    }
    builder.finish()
}

fn ascii(rows: &[(u32, &[u32])]) -> Vec<String> {
    render_ascii(&build(rows))
}

#[test]
fn linear_history_is_one_straight_lane() {
    assert_eq!(ascii(&[(3, &[2]), (2, &[1]), (1, &[])]), ["*", "*", "*"]);
}

#[test]
fn branch_and_merge() {
    // 4 = merge of 3 (main) and 2 (feature); both fork from 1.
    let rows: &[(u32, &[u32])] = &[(4, &[3, 2]), (3, &[1]), (2, &[1]), (1, &[])];
    // Row 1: the feature line (lane 1) passes alongside main on its way to commit 2.
    assert_eq!(ascii(rows), ["* >1", "*|", "|*", "* <1"]);
    let layout = build(rows);
    // The first parent keeps the merge commit's lane and color.
    assert_eq!(layout.node(1), layout.node(0));
    assert_ne!(layout.node(2).color, layout.node(0).color);
}

#[test]
fn octopus_merge_fans_out_to_the_right() {
    let rows: &[(u32, &[u32])] = &[(5, &[4, 3, 2]), (4, &[1]), (3, &[1]), (2, &[1]), (1, &[])];
    assert_eq!(ascii(rows), ["* >1 >2", "*||", "|*|", "||*", "* <1 <2"]);
}

#[test]
fn sibling_tips_converge_at_their_parent() {
    // Two branch tips (3 and 2) on the same base commit 1.
    assert_eq!(
        ascii(&[(3, &[1]), (2, &[1]), (1, &[])]),
        ["*", "|*", "* <1"]
    );
}

#[test]
fn freed_lanes_are_reused() {
    // Feature branch 2 merged by 4; later tip 5 on an unrelated root reuses lane 1.
    let rows: &[(u32, &[u32])] = &[
        (4, &[3, 2]),
        (3, &[1]),
        (2, &[1]),
        (1, &[]),
        (6, &[5]),
        (5, &[]),
    ];
    let layout = build(rows);
    assert_eq!(layout.lane_count(), 2);
    assert_eq!(
        layout.node(4).lane,
        0,
        "new tip takes the leftmost free lane"
    );
}

#[test]
fn merge_joins_an_existing_line_to_the_same_parent() {
    // Both merges 6 and 5 merge feature tip 2; the second merge joins the first's line.
    let rows: &[(u32, &[u32])] = &[(6, &[5, 2]), (5, &[3, 2]), (3, &[1]), (2, &[1]), (1, &[])];
    let layout = build(rows);
    assert_eq!(layout.lane_count(), 2);
    assert_eq!(render_ascii(&layout), ["* >1", "*| >1", "*|", "|*", "* <1"]);
}

#[test]
fn missing_parents_run_off_the_bottom() {
    let layout = build(&[(2, &[1])]);
    let spans: Vec<_> = layout.all_spans().to_vec();
    assert_eq!(spans.len(), 1);
    assert_eq!((spans[0].from, spans[0].to), (1, 2));
}

#[test]
fn color_hint_pins_tip_color() {
    let mut builder = LayoutBuilder::new();
    builder.push(2u32, &[1], Some(7));
    builder.push(1, &[], None);
    let layout = builder.finish();
    assert_eq!(layout.node(0).color, 7);
    assert_eq!(layout.node(1).color, 7, "first parent inherits the color");
}

#[test]
fn a_claiming_commit_keeps_its_color_under_another_branch() {
    // topic's two commits (hint 5) on top of main's tip (claims 0).
    let mut builder = LayoutBuilder::new();
    builder.push(4u32, &[3], Some(5));
    builder.push(3, &[2], None);
    builder.push_claiming(2, &[1], 0);
    builder.push(1, &[], None);
    let layout = builder.finish();
    assert_eq!(render_ascii(&layout), ["*", "*", "*", "*"]);
    let colors: Vec<_> = (0..4).map(|row| layout.node(row).color).collect();
    assert_eq!(colors, [5, 5, 0, 0]);
    // The line keeps topic's color down to main's node (center of row 2), main's below.
    let main_center = 2 * 2 + 1;
    for span in layout.all_spans() {
        let expected = if span.to <= main_center { 5 } else { 0 };
        assert!(
            span.from >= main_center || span.to <= main_center,
            "{span:?}"
        );
        assert_eq!(span.color, expected, "{span:?}");
    }
}

#[test]
fn a_hinted_tip_inside_a_branch_lane_keeps_the_lane_color() {
    // release/1.0 (hint 3) points at an older main commit: main's lane stays 0.
    let mut builder = LayoutBuilder::new();
    builder.push(3u32, &[2], Some(0));
    builder.push(2, &[1], Some(3));
    builder.push(1, &[], None);
    let layout = builder.finish();
    let colors: Vec<_> = (0..3).map(|row| layout.node(row).color).collect();
    assert_eq!(colors, [0, 0, 0]);
    assert!(layout.all_spans().iter().all(|s| s.color == 0));
}

#[test]
fn an_unhinted_lane_switches_to_the_tip_color() {
    // A commit without a branch on top of a branch tip (hint 4).
    let mut builder = LayoutBuilder::new();
    builder.push(3u32, &[2], None);
    builder.push(2, &[1], Some(4));
    builder.push(1, &[], None);
    let layout = builder.finish();
    let colors: Vec<_> = (0..3).map(|row| layout.node(row).color).collect();
    assert_ne!(colors[0], 4, "a fresh color");
    assert_eq!(colors[1..], [4, 4]);
    // Once hinted, the lane keeps that color below the tip.
    let below: Vec<_> = layout.all_spans().iter().filter(|s| s.from >= 3).collect();
    assert!(!below.is_empty() && below.iter().all(|s| s.color == 4));
}

/// Random DAG in topological order: commit `i` (row `i`) has 0–3 distinct parents among
/// later rows, biased towards nearby rows like real histories.
fn arb_dag() -> impl Strategy<Value = Vec<Vec<u32>>> {
    (1usize..120).prop_flat_map(|n| {
        let rows: Vec<_> = (0..n)
            .map(|i| {
                let later = (n - i - 1) as u32;
                prop::collection::vec((0u32..8, 0u32..=later.max(1)), 0..=3).prop_map(
                    move |choices| {
                        let mut parents = Vec::new();
                        for (near, far) in choices {
                            if later == 0 {
                                break;
                            }
                            let offset = if near < 6 { near % later } else { far % later };
                            let parent = i as u32 + 1 + offset;
                            if !parents.contains(&parent) {
                                parents.push(parent);
                            }
                        }
                        parents
                    },
                )
            })
            .collect();
        rows
    })
}

fn build_dag(dag: &[Vec<u32>]) -> Layout {
    let mut builder = LayoutBuilder::new();
    for (row, parents) in dag.iter().enumerate() {
        builder.push(row as u32, parents, None);
    }
    builder.finish()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    #[test]
    fn layout_invariants(dag in arb_dag()) {
        let layout = build_dag(&dag);
        prop_assert_eq!(layout.rows(), dag.len());

        // Lines on one lane never overlap.
        let mut by_lane: BTreeMap<u16, Vec<(u32, u32)>> = BTreeMap::new();
        for span in layout.all_spans() {
            prop_assert!(span.from < span.to);
            prop_assert!(span.lane < layout.lane_count());
            by_lane.entry(span.lane).or_default().push((span.from, span.to));
        }
        for (lane, mut spans) in by_lane {
            spans.sort();
            for pair in spans.windows(2) {
                prop_assert!(pair[0].1 <= pair[1].0, "overlap on lane {}: {:?}", lane, pair);
            }
        }

        for (row, parents) in dag.iter().enumerate() {
            let node = layout.node(row);
            let center = 2 * row as u32 + 1;
            // No foreign line passes through a node.
            for span in layout.spans(row) {
                if span.lane == node.lane {
                    prop_assert!(span.from >= center || span.to <= center,
                        "line crosses node at row {}", row);
                }
            }
            // Every parent edge leaves the node: a first-parent line down its own lane
            // plus one branch-out connector per further parent.
            let down = layout
                .all_spans()
                .iter()
                .filter(|s| s.lane == node.lane && s.from == center)
                .count();
            let out = layout
                .connectors(row)
                .iter()
                .filter(|c| c.kind == ConnectorKind::BranchOut)
                .count();
            prop_assert_eq!(down + out, parents.len(), "row {}", row);
            // Per-row query agrees with a full scan.
            let queried = layout.spans(row).count();
            let scanned = layout
                .all_spans()
                .iter()
                .filter(|s| s.from < center + 1 && s.to > center - 1)
                .count();
            prop_assert_eq!(queried, scanned);
        }

        // Deterministic.
        prop_assert_eq!(render_ascii(&layout), render_ascii(&build_dag(&dag)));
    }
}

/// `cargo test -p gitweft-graph --release -- --ignored --nocapture`
#[test]
#[ignore = "benchmark"]
fn layout_one_million_commits() {
    const N: u32 = 1_000_000;
    let started = Instant::now();
    let mut builder = LayoutBuilder::with_capacity(N as usize);
    for i in 0..N {
        // Mainline with a merged two-commit feature branch every 10 commits.
        let parents: Vec<u32> = if i + 1 >= N {
            vec![]
        } else if i % 10 == 0 && i + 3 < N {
            vec![i + 1, i + 2]
        } else {
            vec![i + 1]
        };
        builder.push(i, &parents, None);
    }
    let layout = builder.finish();
    println!(
        "laid out {} rows / {} lanes in {:?}",
        layout.rows(),
        layout.lane_count(),
        started.elapsed()
    );
}
