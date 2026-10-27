//! Presentation data for a diff, computed on the background job together with the diff
//! itself: syntax colors, word-level change ranges and moved lines. (No UI types here.)

use std::ops::Range;

use gitweft_diff::{LineSide, moved_lines_across, pair_changed_lines, word_changes};
use gitweft_git::gix::bstr::{BString, ByteSlice};
use gitweft_git::read::{FileDiff, LineKind};
use gitweft_syntax::{Spans, highlight_lines, syntax_for_path};

/// Diffs longer than this are shown without syntax colors (highlighting is the costly part).
const MAX_HIGHLIGHT_LINES: usize = 5_000;
/// A diff with more changed lines than this, over all its files, gets no cross-file move
/// detection: the whole diff has to be read and held as text for it.
pub const MAX_ACROSS_LINES: usize = 20_000;

#[derive(Debug, Default)]
pub struct Decorations {
    /// `[hunk][line]` syntax colors (empty when not highlighted).
    pub syntax: Vec<Vec<Spans>>,
    /// `[hunk][line]` byte ranges of changed words.
    pub words: Vec<Vec<Vec<Range<usize>>>>,
    /// `[hunk][line]` the move a changed line belongs to (it left or arrived unchanged).
    pub moved: Vec<Vec<Option<usize>>>,
    /// Per move id (`moved`'s numbers), the other file the block's other end is in, when
    /// that is another file of the same diff; `None` for a move inside this file.
    /// A removed line went *to* it, an added line came *from* it.
    pub moved_across: Vec<Option<BString>>,
    /// Both versions of a binary image, to show them as pictures.
    pub images: Option<ImageSides>,
    /// The tab width the lines were expanded with: the ranges above are into
    /// `display_text(tab_width)`, so the view shows the lines with it too.
    pub tab_width: usize,
}

/// The old and new bytes of an image (`None` where it doesn't exist).
#[derive(Debug, Default)]
pub struct ImageSides {
    /// The path the diff was asked for. A binary diff has no `---`/`+++` lines, so the
    /// parsed diff may not name the file; the image format comes from this.
    pub path: BString,
    pub old: Option<Vec<u8>>,
    pub new: Option<Vec<u8>>,
}

/// Decorations for `diff`. `others` are the diff's other files ([`whole_diff`], without
/// this one), so a block that left this file and arrived in another is found too;
/// pass an empty slice where the rest of the diff isn't at hand.
///
/// [`whole_diff`]: gitweft_git::read::whole_diff
/// Lines are expanded with tab stops `tab` columns apart.
pub fn decorate(diff: &FileDiff, others: &[FileDiff], dark: bool, tab: usize) -> Decorations {
    let syntax = (diff.line_count() <= MAX_HIGHLIGHT_LINES)
        .then(|| syntax_for_path(&diff.path.to_str_lossy()))
        .flatten();
    let texts: Vec<Vec<String>> = diff
        .hunks
        .iter()
        .map(|hunk| hunk.lines.iter().map(|l| l.display_text(tab)).collect())
        .collect();
    let sides: Vec<Vec<LineSide>> = diff
        .hunks
        .iter()
        .map(|hunk| {
            hunk.lines
                .iter()
                .map(|l| match l.kind {
                    LineKind::Context => LineSide::Context,
                    LineKind::Removed => LineSide::Removed,
                    LineKind::Added => LineSide::Added,
                })
                .collect()
        })
        .collect();
    let (moved, moved_across) = moved_by_hunk(&texts, &sides, others, tab);
    let mut decorations = Decorations {
        moved,
        moved_across,
        tab_width: tab,
        ..Decorations::default()
    };

    for ((texts, sides), moved) in texts.iter().zip(&sides).zip(&decorations.moved) {
        // Syntax: highlight the old side (context + removed) and the new side
        // (context + added) as continuous text, then pick each line's colors.
        let mut line_spans: Vec<Spans> = vec![Vec::new(); texts.len()];
        if let Some(syntax) = syntax {
            for side in [LineSide::Removed, LineSide::Added] {
                let indices: Vec<usize> = (0..texts.len())
                    .filter(|&ix| sides[ix] == LineSide::Context || sides[ix] == side)
                    .collect();
                let lines: Vec<&str> = indices.iter().map(|&ix| texts[ix].as_str()).collect();
                for (ix, spans) in indices
                    .into_iter()
                    .zip(highlight_lines(syntax, &lines, dark))
                {
                    if sides[ix] == side || line_spans[ix].is_empty() {
                        line_spans[ix] = spans;
                    }
                }
            }
        }

        // A moved line isn't an edit of its neighbour, so it gets no word highlights.
        let mut line_words: Vec<Vec<Range<usize>>> = vec![Vec::new(); texts.len()];
        for (removed, added) in pair_changed_lines(sides) {
            if moved[removed].is_some() || moved[added].is_some() {
                continue;
            }
            if let Some((old, new)) = word_changes(&texts[removed], &texts[added]) {
                line_words[removed] = old;
                line_words[added] = new;
            }
        }

        decorations.syntax.push(line_spans);
        decorations.words.push(line_words);
    }
    decorations
}

/// `[hunk][line]` moves, found across the whole file and the diff's `others`, and for each
/// move id the other file its other end is in (`None` when both ends are in this file).
/// Hunks are kept apart by a blank context line, so a run of changes never continues from
/// one hunk into the next; [`moved_lines_across`] keeps the files apart the same way.
fn moved_by_hunk(
    texts: &[Vec<String>],
    sides: &[Vec<LineSide>],
    others: &[FileDiff],
    tab: usize,
) -> (Vec<Vec<Option<usize>>>, Vec<Option<BString>>) {
    let mut lines: Vec<(LineSide, &str)> = Vec::new();
    for (h, (texts, sides)) in texts.iter().zip(sides).enumerate() {
        if h > 0 {
            lines.push((LineSide::Context, ""));
        }
        lines.extend(sides.iter().copied().zip(texts.iter().map(String::as_str)));
    }
    // This file is index 0, so its ends are the ones to compare against.
    let other_texts: Vec<Vec<(LineSide, String)>> =
        others.iter().map(|file| file_lines(file, tab)).collect();
    let mut all: Vec<&[(LineSide, &str)]> = Vec::with_capacity(1 + others.len());
    let borrowed: Vec<Vec<(LineSide, &str)>> = other_texts
        .iter()
        .map(|file| file.iter().map(|(s, t)| (*s, t.as_str())).collect())
        .collect();
    all.push(&lines);
    all.extend(borrowed.iter().map(Vec::as_slice));
    let (per_file, ends) = moved_lines_across(&all);

    let across = ends
        .iter()
        .map(|end| match (end.from, end.to) {
            (0, 0) => None,
            (0, to) => Some(others[to - 1].path.clone()),
            (from, _) => Some(others[from - 1].path.clone()),
        })
        .collect();
    let mut moved = per_file.into_iter().next().unwrap_or_default().into_iter();
    let by_hunk = texts
        .iter()
        .enumerate()
        .map(|(h, texts)| {
            if h > 0 {
                moved.next();
            }
            texts.iter().map(|_| moved.next().flatten()).collect()
        })
        .collect();
    (by_hunk, across)
}

/// One file's whole diff as [`moved_lines_across`] takes it: every hunk's lines in order,
/// the hunks kept apart by a blank context line.
fn file_lines(diff: &FileDiff, tab: usize) -> Vec<(LineSide, String)> {
    let mut lines = Vec::with_capacity(diff.line_count() + diff.hunks.len());
    for (h, hunk) in diff.hunks.iter().enumerate() {
        if h > 0 {
            lines.push((LineSide::Context, String::new()));
        }
        lines.extend(hunk.lines.iter().map(|l| {
            let side = match l.kind {
                LineKind::Context => LineSide::Context,
                LineKind::Removed => LineSide::Removed,
                LineKind::Added => LineSide::Added,
            };
            (side, l.display_text(tab))
        }));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use LineSide::*;
    use gitweft_git::CancelToken;
    use gitweft_git::read::{DiffLine, DiffOptions, DiffSource, Hunk, file_diff, whole_diff};
    use gitweft_testkit::Sandbox;

    #[test]
    fn moves_are_found_across_hunks_and_mapped_back() {
        let block = [
            "fn parse_header(line: &str) -> Option<u32> {",
            "    line.split_once(':')?.0.trim().parse().ok()",
            "}",
        ];
        let first: Vec<String> = block.iter().map(|l| l.to_string()).collect();
        let second: Vec<String> = ["context line".to_owned()]
            .into_iter()
            .chain(first.clone())
            .collect();
        let texts = vec![first, second];
        let sides = vec![
            vec![Removed, Removed, Removed],
            vec![Context, Added, Added, Added],
        ];
        let (moved, across) = moved_by_hunk(&texts, &sides, &[], 4);
        assert_eq!(moved[0], [Some(0), Some(0), Some(0)]);
        assert_eq!(moved[1], [None, Some(0), Some(0), Some(0)]);
        assert_eq!(across, [None]);
    }

    /// A block that left this file and arrived in another one of the same diff.
    #[test]
    fn a_move_into_another_file_says_where_it_went() {
        let block = [
            "fn parse_header(line: &str) -> Option<u32> {",
            "    line.split_once(':')?.0.trim().parse().ok()",
            "}",
        ];
        let texts = vec![block.iter().map(|l| l.to_string()).collect::<Vec<_>>()];
        let sides = vec![vec![Removed, Removed, Removed]];
        let other = FileDiff {
            path: "src/header.rs".into(),
            hunks: vec![Hunk {
                lines: block
                    .iter()
                    .map(|text| DiffLine {
                        kind: LineKind::Added,
                        old_line: None,
                        new_line: None,
                        text: (*text).into(),
                        no_newline: false,
                    })
                    .collect(),
                old_start: 0,
                old_len: 0,
                new_start: 1,
                new_len: 3,
                section: String::new(),
            }],
            ..FileDiff::default()
        };

        let (moved, across) = moved_by_hunk(&texts, &sides, &[other], 4);
        assert_eq!(moved[0], [Some(0), Some(0), Some(0)]);
        assert_eq!(across, [Some("src/header.rs".into())]);
        // Without the other file there is no move at all: nothing arrived.
        let (alone, across) = moved_by_hunk(&texts, &sides, &[], 4);
        assert_eq!(alone[0], [None, None, None]);
        assert!(across.is_empty());
    }

    /// The whole way through real git: a commit that takes a block out of one file and
    /// puts it in another tints both ends and names the file at each.
    #[test]
    fn a_commit_that_moves_a_block_between_files_is_decorated_at_both_ends() {
        let block = "fn parse_header(line: &str) -> Option<u32> {\n    \
                     line.split_once(':')?.0.trim().parse().ok()\n}\n";
        let sb = Sandbox::new();
        let repo = sb.init_repo("r");
        std::fs::write(repo.join("a.rs"), format!("// a\n{block}")).unwrap();
        std::fs::write(repo.join("b.rs"), "// b\n").unwrap();
        sb.run(&repo, &["add", "."]);
        sb.run(&repo, &["commit", "-qm", "base"]);
        let base = sb
            .run(&repo, &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .unwrap();

        std::fs::write(repo.join("a.rs"), "// a\n").unwrap();
        std::fs::write(repo.join("b.rs"), format!("// b\n{block}")).unwrap();
        sb.run(&repo, &["commit", "-qam", "move the parser"]);
        let head = sb
            .run(&repo, &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .unwrap();

        let source = DiffSource::Commit {
            parent: Some(base),
            commit: head,
        };
        let cancel = CancelToken::new();
        let options = DiffOptions::default();
        let all = whole_diff(sb.git(), &repo, &source, options, &cancel).unwrap();
        assert_eq!(all.len(), 2);
        let one = |path: &str| {
            let diff = file_diff(
                sb.git(),
                &repo,
                &source,
                &BString::from(path),
                None,
                options,
                &cancel,
            )
            .unwrap();
            let others: Vec<FileDiff> = all.iter().filter(|f| f.path != path).cloned().collect();
            let decorations = decorate(&diff, &others, true, 4);
            let moved: Vec<Option<usize>> = decorations.moved.concat();
            let other: Vec<Option<String>> = moved
                .iter()
                .flatten()
                .map(|id| {
                    decorations.moved_across[*id]
                        .as_ref()
                        .map(|p| p.to_str_lossy().into_owned())
                })
                .collect();
            (moved, other)
        };

        // The three lines left a.rs for b.rs, and arrived in b.rs from a.rs.
        let (moved, other) = one("a.rs");
        assert_eq!(moved.iter().filter(|m| m.is_some()).count(), 3);
        assert!(
            other.iter().all(|p| p.as_deref() == Some("b.rs")),
            "{other:?}"
        );
        let (moved, other) = one("b.rs");
        assert_eq!(moved.iter().filter(|m| m.is_some()).count(), 3);
        assert!(
            other.iter().all(|p| p.as_deref() == Some("a.rs")),
            "{other:?}"
        );
    }
}
