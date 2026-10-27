//! Pure diff algorithms for Gitweft (no git, no UI): word-level changes between two
//! lines, the pairing of removed/added lines inside a hunk, the side-by-side layout, and
//! lines that moved within a file.

pub mod conflict;
mod moved;

use std::ops::Range;

pub use moved::{MIN_MOVED_ALNUM, MovedBetween, moved_lines, moved_lines_across};

use similar::{ChangeTag, TextDiff};

/// Lines longer than this (typically minified code) get no word-level highlights.
pub const MAX_WORD_DIFF_LEN: usize = 1_000;
/// Below this similarity the lines are simply different; word highlights would be noise.
const MIN_SIMILARITY: f32 = 0.3;

/// The kind of a line inside a hunk, as far as pairing is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineSide {
    Context,
    Removed,
    Added,
}

/// Pairs removed and added lines for word-level highlighting. In each run of removed
/// lines directly followed by a run of added lines, the i-th removed line pairs with the
/// i-th added line. Returns `(removed index, added index)` into `sides`.
pub fn pair_changed_lines(sides: &[LineSide]) -> Vec<(usize, usize)> {
    let mut pairs = Vec::new();
    let mut ix = 0;
    while ix < sides.len() {
        if sides[ix] != LineSide::Removed {
            ix += 1;
            continue;
        }
        let removed_start = ix;
        while ix < sides.len() && sides[ix] == LineSide::Removed {
            ix += 1;
        }
        let added_start = ix;
        while ix < sides.len() && sides[ix] == LineSide::Added {
            ix += 1;
        }
        pairs.extend((removed_start..added_start).zip(added_start..ix));
    }
    pairs
}

/// Side-by-side layout of a hunk: each row is `(old-side line, new-side line)` as indices
/// into `sides`. Context lines fill both columns. In each run of removed lines followed
/// by added lines, the i-th removed line sits next to the i-th added line (the pairing of
/// [`pair_changed_lines`]), and the longer run continues against a blank.
pub fn split_rows(sides: &[LineSide]) -> Vec<(Option<usize>, Option<usize>)> {
    let mut rows = Vec::with_capacity(sides.len());
    let mut ix = 0;
    while ix < sides.len() {
        if sides[ix] == LineSide::Context {
            rows.push((Some(ix), Some(ix)));
            ix += 1;
            continue;
        }
        let removed_start = ix;
        while ix < sides.len() && sides[ix] == LineSide::Removed {
            ix += 1;
        }
        let added_start = ix;
        while ix < sides.len() && sides[ix] == LineSide::Added {
            ix += 1;
        }
        let (removed, added) = (removed_start..added_start, added_start..ix);
        for k in 0..removed.len().max(added.len()) {
            rows.push((removed.clone().nth(k), added.clone().nth(k)));
        }
    }
    rows
}

/// Byte ranges of changed words within one line.
pub type WordRanges = Vec<Range<usize>>;

/// Byte ranges that differ between two similar lines: `(ranges in old, ranges in new)`.
/// `None` when the lines are too long or too different for word highlights to help.
pub fn word_changes(old: &str, new: &str) -> Option<(WordRanges, WordRanges)> {
    if old.len() > MAX_WORD_DIFF_LEN || new.len() > MAX_WORD_DIFF_LEN || old == new {
        return None;
    }
    // Unicode word boundaries split punctuation from identifiers (`b);` → `b`, `)`, `;`),
    // which is what code needs; plain whitespace splitting would not.
    let diff = TextDiff::from_unicode_words(old, new);
    if diff.ratio() < MIN_SIMILARITY {
        return None;
    }
    let (mut old_pos, mut new_pos) = (0, 0);
    let (mut old_ranges, mut new_ranges) = (Vec::new(), Vec::new());
    for change in diff.iter_all_changes() {
        let len = change.value().len();
        match change.tag() {
            ChangeTag::Equal => {
                old_pos += len;
                new_pos += len;
            }
            ChangeTag::Delete => {
                push_merged(&mut old_ranges, old_pos..old_pos + len);
                old_pos += len;
            }
            ChangeTag::Insert => {
                push_merged(&mut new_ranges, new_pos..new_pos + len);
                new_pos += len;
            }
        }
    }
    Some((old_ranges, new_ranges))
}

/// Appends `range`, extending the previous one when they touch.
fn push_merged(ranges: &mut Vec<Range<usize>>, range: Range<usize>) {
    match ranges.last_mut() {
        Some(last) if last.end == range.start => last.end = range.end,
        _ => ranges.push(range),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use LineSide::*;

    #[test]
    fn pairs_runs_of_removed_then_added() {
        let sides = [
            Context, Removed, Removed, Added, Added, Added, Context, Removed, Context, Added,
        ];
        // 1↔3, 2↔4 (the third added line is unpaired); the lone removed line at 7 is
        // followed by context, so it has no partner; the lone added line at 9 neither.
        assert_eq!(pair_changed_lines(&sides), [(1, 3), (2, 4)]);
    }

    #[test]
    fn split_rows_put_runs_side_by_side() {
        let sides = [Context, Removed, Removed, Added, Context, Added, Removed];
        assert_eq!(
            split_rows(&sides),
            [
                (Some(0), Some(0)),
                (Some(1), Some(3)),
                (Some(2), None),
                (Some(4), Some(4)),
                (None, Some(5)),
                (Some(6), None),
            ]
        );
    }

    #[test]
    fn split_rows_show_every_line_once_per_side() {
        let patterns: [&[LineSide]; 4] = [
            &[],
            &[Added, Added],
            &[Removed, Added, Added, Added, Context, Removed, Removed],
            &[Context, Removed, Context, Added, Removed, Added],
        ];
        for sides in patterns {
            let rows = split_rows(sides);
            let old: Vec<usize> = rows.iter().filter_map(|r| r.0).collect();
            let new: Vec<usize> = rows.iter().filter_map(|r| r.1).collect();
            let expect = |keep: LineSide| {
                (0..sides.len())
                    .filter(|&ix| sides[ix] == Context || sides[ix] == keep)
                    .collect::<Vec<_>>()
            };
            assert_eq!(old, expect(Removed), "{sides:?}");
            assert_eq!(new, expect(Added), "{sides:?}");
        }
    }

    #[test]
    fn word_ranges_point_at_the_changed_words() {
        let old = "let value = compute(a, b);";
        let new = "let result = compute(a, c);";
        let (old_ranges, new_ranges) = word_changes(old, new).unwrap();
        let pick = |text: &str, ranges: &[Range<usize>]| {
            ranges
                .iter()
                .map(|r| text[r.clone()].to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(pick(old, &old_ranges), ["value", "b"]);
        assert_eq!(pick(new, &new_ranges), ["result", "c"]);
    }

    #[test]
    fn unrelated_long_or_equal_lines_get_no_word_ranges() {
        assert!(word_changes("fn main() {}", "SELECT * FROM users WHERE id = 1").is_none());
        let long = "x".repeat(MAX_WORD_DIFF_LEN + 1);
        assert!(word_changes(&long, "x").is_none());
        assert!(word_changes("same", "same").is_none());
    }

    #[test]
    fn ranges_are_valid_utf8_boundaries() {
        let (old_ranges, new_ranges) = word_changes("привет мир", "привет всем").unwrap();
        for range in old_ranges {
            assert!("привет мир".get(range).is_some());
        }
        for range in new_ranges {
            assert!("привет всем".get(range).is_some());
        }
    }
}
