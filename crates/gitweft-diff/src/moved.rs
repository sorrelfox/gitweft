//! Moved-line detection within one file's diff, like git's `--color-moved=blocks`.

use std::collections::HashMap;

use crate::LineSide;

/// Blocks with fewer alphanumeric characters than this aren't moves (git's default for
/// `--color-moved`: short blocks such as `}` or blank lines match by accident).
pub const MIN_MOVED_ALNUM: usize = 20;
/// Unused removed lines examined when a block starts; bounds the cost of common lines.
const MAX_CANDIDATES: usize = 256;
/// How many following lines a candidate's chain is compared over when a block starts.
const MAX_LOOKAHEAD: usize = 64;

/// Lines that moved within one file's diff, like git's `--color-moved=blocks`: for each
/// of `lines` (the whole file diff, all hunks, in order), the move it belongs to, or `None`.
/// A move is a run of added lines, adjacent in `lines`, whose texts equal (line by line) a
/// run of removed lines that are adjacent in `lines` too; both runs get the same id. Ids
/// count up from 0 in the order of the added side.
pub fn moved_lines(lines: &[(LineSide, &str)]) -> Vec<Option<usize>> {
    let mut moves = Moves::new(lines);
    let mut block: Option<Block> = None;
    for (ix, &(side, _)) in lines.iter().enumerate() {
        if side != LineSide::Added {
            continue;
        }
        if let Some(b) = &mut block
            && b.added + b.len == ix
            && moves.matches(ix, b.removed + b.len)
        {
            b.len += 1;
            continue;
        }
        if let Some(b) = block.take() {
            moves.close(b);
        }
        block = moves.best_start(ix).map(|removed| Block {
            added: ix,
            removed,
            len: 1,
        });
    }
    if let Some(b) = block {
        moves.close(b);
    }
    moves.out
}

/// Where a move's two ends are: the file its lines were removed from, and the file they
/// were added to (indices into the files given to [`moved_lines_across`]). They are the same
/// file for a move inside one diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MovedBetween {
    pub from: usize,
    pub to: usize,
}

/// Lines that moved, looked for across all the files of one diff, as git's `--color-moved`
/// does: a block that left one file and arrived in another counts too (#69). `files` holds
/// each file's whole diff, in order, as [`moved_lines`] takes it.
///
/// Returns one `Option<move id>` per line of each file, and for each id the files its two
/// ends are in. The ids are the same on both ends, so a view can tint them together and say
/// where the block went.
pub fn moved_lines_across(
    files: &[&[(LineSide, &str)]],
) -> (Vec<Vec<Option<usize>>>, Vec<MovedBetween>) {
    // One list, the files kept apart by a context line, so a run of changes never continues
    // from the end of one file into the start of the next.
    let mut lines: Vec<(LineSide, &str)> = Vec::new();
    for (ix, file) in files.iter().enumerate() {
        if ix > 0 {
            lines.push((LineSide::Context, ""));
        }
        lines.extend(file.iter().copied());
    }
    let found = moved_lines(&lines);
    let mut per_file = Vec::with_capacity(files.len());
    let mut ends: Vec<MovedBetween> = Vec::new();
    let mut at = 0;
    for (ix, file) in files.iter().enumerate() {
        if ix > 0 {
            at += 1; // the separator
        }
        let mut moves = Vec::with_capacity(file.len());
        for (found, &(side, _)) in found[at..at + file.len()].iter().zip(file.iter()) {
            if let Some(id) = *found {
                if ends.len() <= id {
                    ends.resize(id + 1, MovedBetween { from: ix, to: ix });
                }
                match side {
                    LineSide::Added => ends[id].to = ix,
                    LineSide::Removed => ends[id].from = ix,
                    LineSide::Context => {}
                }
            }
            moves.push(*found);
        }
        per_file.push(moves);
        at += file.len();
    }
    (per_file, ends)
}

/// A candidate move: `len` added lines from `added` matching removed lines from `removed`.
struct Block {
    added: usize,
    removed: usize,
    len: usize,
}

/// Removed lines with one text, in order; the first `skip` are all taken by moves.
#[derive(Default)]
struct Candidates {
    ixs: Vec<usize>,
    skip: usize,
}

struct Moves<'a> {
    lines: &'a [(LineSide, &'a str)],
    removed: HashMap<&'a str, Candidates>,
    /// Removed lines that belong to a committed move.
    used: Vec<bool>,
    out: Vec<Option<usize>>,
    next_id: usize,
}

impl<'a> Moves<'a> {
    fn new(lines: &'a [(LineSide, &'a str)]) -> Self {
        let mut removed: HashMap<&str, Candidates> = HashMap::new();
        for (ix, &(side, text)) in lines.iter().enumerate() {
            if side == LineSide::Removed {
                removed.entry(normalize(text)).or_default().ixs.push(ix);
            }
        }
        Self {
            lines,
            removed,
            used: vec![false; lines.len()],
            out: vec![None; lines.len()],
            next_id: 0,
        }
    }

    /// Whether the added line at `added` can pair with a free removed line at `removed`.
    fn matches(&self, added: usize, removed: usize) -> bool {
        let (Some(a), Some(r)) = (self.lines.get(added), self.lines.get(removed)) else {
            return false;
        };
        a.0 == LineSide::Added
            && r.0 == LineSide::Removed
            && !self.used[removed]
            && normalize(a.1) == normalize(r.1)
    }

    /// How many lines from `added` pair with lines from `removed`, up to the lookahead cap.
    fn chain_len(&self, added: usize, removed: usize) -> usize {
        (0..MAX_LOOKAHEAD)
            .take_while(|&k| self.matches(added + k, removed + k))
            .count()
    }

    /// The free removed line whose chain matches the most lines from `added` (ties: first).
    fn best_start(&mut self, added: usize) -> Option<usize> {
        let cands = self.removed.get_mut(normalize(self.lines[added].1))?;
        while cands.ixs.get(cands.skip).is_some_and(|&ix| self.used[ix]) {
            cands.skip += 1;
        }
        let cands = &self.removed[normalize(self.lines[added].1)];
        // No chain can outrun the added run, so a candidate that covers it wins outright.
        let longest = self.lines[added..]
            .iter()
            .take(MAX_LOOKAHEAD)
            .take_while(|(side, _)| *side == LineSide::Added)
            .count();
        let mut best: Option<(usize, usize)> = None;
        let free = cands.ixs[cands.skip..].iter().filter(|&&ix| !self.used[ix]);
        for &removed in free.take(MAX_CANDIDATES) {
            let len = self.chain_len(added, removed);
            if best.is_none_or(|(_, best_len)| len > best_len) {
                best = Some((removed, len));
                if len == longest {
                    break;
                }
            }
        }
        best.map(|(removed, _)| removed)
    }

    /// Commits `block` as a move if it's long enough; otherwise its lines stay free.
    fn close(&mut self, block: Block) {
        let alnum: usize = (block.added..block.added + block.len)
            .map(|ix| {
                normalize(self.lines[ix].1)
                    .chars()
                    .filter(|c| c.is_alphanumeric())
                    .count()
            })
            .sum();
        if alnum < MIN_MOVED_ALNUM {
            return;
        }
        for k in 0..block.len {
            self.used[block.removed + k] = true;
            self.out[block.added + k] = Some(self.next_id);
            self.out[block.removed + k] = Some(self.next_id);
        }
        self.next_id += 1;
    }
}

/// The text lines compare by: a trailing `\r` doesn't count.
fn normalize(text: &str) -> &str {
    text.strip_suffix('\r').unwrap_or(text)
}

#[cfg(test)]
mod tests {
    use std::ops::Range;

    use proptest::prelude::*;

    use super::*;
    use LineSide::*;

    const FUNCTION: [&str; 5] = [
        "fn parse_header(input: &str) -> Header {",
        "    let (name, value) = input.split_once(':').unwrap();",
        "    let value = value.trim().to_owned();",
        "    Header { name: name.to_owned(), value }",
        "}",
    ];

    fn run(side: LineSide, texts: &[&'static str]) -> Vec<(LineSide, &'static str)> {
        texts.iter().map(|&t| (side, t)).collect()
    }

    fn ctx(texts: &[&'static str]) -> Vec<(LineSide, &'static str)> {
        run(Context, texts)
    }

    #[test]
    fn a_function_moved_to_another_file_is_one_move_across_both() {
        let gone = [ctx(&["use std::io;"]), run(Removed, &FUNCTION)].concat();
        let arrived = [run(Added, &FUNCTION), ctx(&["fn main() {}"])].concat();
        let (per_file, ends) = moved_lines_across(&[&gone, &arrived]);
        assert_eq!(per_file[0], [vec![None], vec![Some(0); 5]].concat());
        assert_eq!(per_file[1], [vec![Some(0); 5], vec![None]].concat());
        // The same id at both ends, and it says which way the block went.
        assert_eq!(ends, [MovedBetween { from: 0, to: 1 }]);
    }

    #[test]
    fn a_move_inside_one_file_stays_inside_it() {
        let untouched = ctx(&["fn main() {}"]);
        let shuffled = [
            run(Removed, &FUNCTION),
            ctx(&["", "fn other() {}"]),
            run(Added, &FUNCTION),
        ]
        .concat();
        let (per_file, ends) = moved_lines_across(&[&untouched, &shuffled]);
        assert_eq!(per_file[0], [None]);
        assert_eq!(
            per_file[1],
            [vec![Some(0); 5], vec![None; 2], vec![Some(0); 5]].concat()
        );
        assert_eq!(ends, [MovedBetween { from: 1, to: 1 }]);
    }

    #[test]
    fn a_run_does_not_continue_from_one_file_into_the_next() {
        // The same lines added at the end of one file and removed at the start of the next:
        // they are a move, not one longer block, and the separator keeps them apart.
        let first = [ctx(&["mod a;"]), run(Added, &FUNCTION[..3])].concat();
        let second = [run(Removed, &FUNCTION[..3]), ctx(&["mod b;"])].concat();
        let (per_file, ends) = moved_lines_across(&[&first, &second]);
        assert_eq!(per_file[0], [vec![None], vec![Some(0); 3]].concat());
        assert_eq!(per_file[1], [vec![Some(0); 3], vec![None]].concat());
        assert_eq!(ends, [MovedBetween { from: 1, to: 0 }]);
    }

    #[test]
    fn function_moved_to_the_bottom_is_one_move() {
        let lines = [
            run(Removed, &FUNCTION),
            ctx(&["use std::io;", "", "fn main() {}"]),
            run(Added, &FUNCTION),
        ]
        .concat();
        let expect = [vec![Some(0); 5], vec![None; 3], vec![Some(0); 5]].concat();
        assert_eq!(moved_lines(&lines), expect);
    }

    #[test]
    fn short_blocks_are_not_moves() {
        let lines = [
            run(Removed, &["}", "", "x"]),
            ctx(&["fn keep_this_line_in_place() {}"]),
            run(Added, &["}", "", "x"]),
        ]
        .concat();
        assert_eq!(moved_lines(&lines), vec![None; 7]);
    }

    #[test]
    fn separate_moves_count_up_in_added_order() {
        let a = ["let first_block_line = 1;", "let first_block_other = 2;"];
        let b = ["let second_block_line = 3;", "let second_block_other = 4;"];
        let lines = [
            run(Removed, &a),
            ctx(&["// middle"]),
            run(Removed, &b),
            ctx(&["// later"]),
            run(Added, &b),
            ctx(&["// end"]),
            run(Added, &a),
        ]
        .concat();
        let (a_id, b_id) = (Some(1), Some(0));
        let expect = [
            vec![a_id; 2],
            vec![None],
            vec![b_id; 2],
            vec![None],
            vec![b_id; 2],
            vec![None],
            vec![a_id; 2],
        ]
        .concat();
        assert_eq!(moved_lines(&lines), expect);
    }

    #[test]
    fn a_removed_line_moves_only_once() {
        let lines = [
            run(Removed, &FUNCTION),
            ctx(&["// between"]),
            run(Added, &FUNCTION),
            ctx(&["// and a copy"]),
            run(Added, &FUNCTION),
        ]
        .concat();
        let expect = [
            vec![Some(0); 5],
            vec![None],
            vec![Some(0); 5],
            vec![None],
            vec![None; 5],
        ]
        .concat();
        assert_eq!(moved_lines(&lines), expect);
    }

    #[test]
    fn a_modified_line_splits_the_move() {
        let mut changed = FUNCTION;
        changed[2] = "    let value = value.to_lowercase();";
        let lines = [
            run(Removed, &FUNCTION),
            ctx(&["// between"]),
            run(Added, &changed),
        ]
        .concat();
        let expect = [
            vec![Some(0), Some(0), None, Some(1), Some(1)],
            vec![None],
            vec![Some(0), Some(0), None, Some(1), Some(1)],
        ]
        .concat();
        assert_eq!(moved_lines(&lines), expect);
    }

    #[test]
    fn a_trailing_carriage_return_still_matches() {
        let crlf = [
            "fn parse_header(input: &str) -> Header {\r",
            "    let (name, value) = input.split_once(':').unwrap();\r",
        ];
        let lines = [
            run(Removed, &crlf),
            ctx(&["// between"]),
            run(Added, &FUNCTION[..2]),
        ]
        .concat();
        let expect = [vec![Some(0); 2], vec![None], vec![Some(0); 2]].concat();
        assert_eq!(moved_lines(&lines), expect);
    }

    /// git's `--color-moved=blocks` tints a blank line inside a moved block (checked against
    /// git 2.5x on this very diff, #35): it moves with the block, on both sides.
    #[test]
    fn a_blank_line_inside_a_moved_block_moves_with_it() {
        let function = [
            "fn first_function_here() {",
            "    do_the_first_thing();",
            "",
            "    do_the_second_thing();",
            "}",
        ];
        let lines = [
            run(Removed, &function),
            ctx(&[
                "fn stays_one() {}",
                "fn stays_two() {}",
                "fn stays_three() {}",
            ]),
            run(Added, &function),
        ]
        .concat();
        let expect = [vec![Some(0); 5], vec![None; 3], vec![Some(0); 5]].concat();
        assert_eq!(moved_lines(&lines), expect);
    }

    #[test]
    fn moves_stay_linear_on_large_diffs() {
        let texts: Vec<String> = (0..25_000)
            .map(|i| format!("line number {i} of the file"))
            .collect();
        let mut lines: Vec<(LineSide, &str)> =
            texts.iter().map(|t| (Removed, t.as_str())).collect();
        lines.extend(texts.iter().map(|t| (Added, t.as_str())));
        // Many identical short lines: each added `}` has thousands of candidates.
        lines.extend((0..10_000).flat_map(|_| [(Removed, "}"), (Context, "")]));
        lines.extend((0..10_000).flat_map(|_| [(Added, "}"), (Context, "")]));
        let moved = moved_lines(&lines);
        assert!(moved[..50_000].iter().all(|m| *m == Some(0)));
        assert!(moved[50_000..].iter().all(Option::is_none));
    }

    /// A file of `n` lines, the block `cut` moved to position `to` among the other lines.
    fn arb_move() -> impl Strategy<Value = (usize, Range<usize>, usize)> {
        (3usize..60)
            .prop_flat_map(|n| (Just(n), 0..n - 1))
            .prop_flat_map(|(n, start)| (Just(n), Just(start), start + 2..=n))
            .prop_flat_map(|(n, start, end)| (Just(n), Just(start..end), 0..=n - (end - start)))
            .prop_filter("the block lands elsewhere", |(_, cut, to)| *to != cut.start)
    }

    const ALPHABET: [&str; 7] = [
        "alpha beta gamma delta",
        "}",
        "",
        "epsilon zeta eta theta",
        "iota kappa lambda\r",
        "iota kappa lambda",
        "x",
    ];

    fn arb_lines() -> impl Strategy<Value = Vec<(LineSide, &'static str)>> {
        let side = prop_oneof![Just(Context), Just(Removed), Just(Added)];
        proptest::collection::vec((side, proptest::sample::select(&ALPHABET[..])), 0..200)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(400))]

        #[test]
        fn a_moved_block_is_found_whole((n, cut, to) in arb_move()) {
            let texts: Vec<String> = (0..n).map(|i| format!("line number {i} of the file")).collect();
            let block: Vec<&str> = texts[cut.clone()].iter().map(String::as_str).collect();
            let rest: Vec<&str> = texts[..cut.start]
                .iter()
                .chain(&texts[cut.end..])
                .map(String::as_str)
                .collect();
            let mut lines = Vec::new();
            for k in 0..=rest.len() {
                if k == cut.start {
                    lines.extend(block.iter().map(|&t| (Removed, t)));
                }
                if k == to {
                    lines.extend(block.iter().map(|&t| (Added, t)));
                }
                if let Some(&t) = rest.get(k) {
                    lines.push((Context, t));
                }
            }
            let moved = moved_lines(&lines);
            for (&(side, _), m) in lines.iter().zip(&moved) {
                prop_assert_eq!(*m, (side != Context).then_some(0));
            }
        }

        #[test]
        fn moves_pair_equal_runs(lines in arb_lines()) {
            let moved = moved_lines(&lines);
            let count = moved.iter().flatten().max().map_or(0, |m| m + 1);
            let mut first_added = Vec::new();
            for id in 0..count {
                let of = |side: LineSide| -> Vec<usize> {
                    (0..lines.len()).filter(|&ix| lines[ix].0 == side && moved[ix] == Some(id)).collect()
                };
                let (removed, added) = (of(Removed), of(Added));
                prop_assert!(!added.is_empty());
                prop_assert_eq!(removed.len(), added.len());
                prop_assert_eq!(removed.len() + added.len(), moved.iter().filter(|m| **m == Some(id)).count());
                for run in [&removed, &added] {
                    prop_assert!(run.windows(2).all(|w| w[1] == w[0] + 1), "{run:?} isn't adjacent");
                }
                for (&r, &a) in removed.iter().zip(&added) {
                    prop_assert_eq!(normalize(lines[r].1), normalize(lines[a].1));
                }
                let alnum: usize = added
                    .iter()
                    .map(|&ix| lines[ix].1.chars().filter(|c| c.is_alphanumeric()).count())
                    .sum();
                prop_assert!(alnum >= MIN_MOVED_ALNUM);
                first_added.push(added[0]);
            }
            prop_assert!(first_added.is_sorted(), "ids don't follow the added side");
        }
    }
}
