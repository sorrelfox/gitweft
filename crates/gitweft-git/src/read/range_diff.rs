//! `git range-diff`: how the commits of a branch changed between two versions of it
//! (typically before and after a rebase). Each commit of either version is paired with
//! its counterpart if git finds one: the same patch, a changed one (with the diff
//! between the two patches), or no counterpart (removed, added).

use std::path::Path;

use gix::ObjectId;

use crate::cancel::CancelToken;
use crate::cli::{GitCli, GitError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeChange {
    /// The same patch in both versions (`=`).
    Same,
    /// Counterparts whose patches differ (`!`).
    Changed,
    /// Only in the old version (`<`).
    Removed,
    /// Only in the new version (`>`).
    Added,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangePair {
    /// The commit in the old version, with its 1-based position there.
    pub old: Option<(usize, ObjectId)>,
    /// The commit in the new version, with its 1-based position there.
    pub new: Option<(usize, ObjectId)>,
    pub change: RangeChange,
    /// The summary git shows (the new commit's, or the old one's when removed).
    pub summary: String,
    /// For [`RangeChange::Changed`]: git's diff of the two patches, one entry per line,
    /// without the indentation.
    pub interdiff: Vec<String>,
}

/// A `--creation-factor` that pairs heavily reworked commits: at git's default (60) a
/// commit with two thirds of its lines rewritten shows as removed plus added; at 100 it's
/// one changed commit.
pub const HARDER_CREATION_FACTOR: u32 = 100;

/// Pairs the commits `old_base..old_head` with `new_base..new_head`. `creation_factor`
/// (git's `--creation-factor`, a percentage; `None`: git's default) is how much a patch may
/// change and still count as the same commit changed.
pub fn range_diff(
    git: &GitCli,
    workdir: &Path,
    old: (&ObjectId, &ObjectId),
    new: (&ObjectId, &ObjectId),
    creation_factor: Option<u32>,
    cancel: &CancelToken,
) -> Result<Vec<RangePair>, GitError> {
    let output = git
        .command(workdir)
        // Full ids, no colour, no notes. Git 2.39's range-diff ignores `--abbrev`, but
        // not `core.abbrev=no`; newer git takes `--abbrev=64` (clamped to the hash length).
        .args([
            "-c",
            "core.abbrev=no",
            "range-diff",
            "--no-color",
            "--no-notes",
        ])
        .arg("--abbrev=64")
        .args(creation_factor.map(|factor| format!("--creation-factor={factor}")))
        .arg(format!("{}..{}", old.0, old.1))
        .arg(format!("{}..{}", new.0, new.1))
        .read_only()
        .run(cancel)?;
    Ok(parse_range_diff(&output.stdout_str()))
}

/// Parses `git range-diff --no-color` output with full ids.
pub fn parse_range_diff(text: &str) -> Vec<RangePair> {
    let mut pairs: Vec<RangePair> = Vec::new();
    for line in text.lines() {
        if let Some(pair) = parse_header(line) {
            pairs.push(pair);
        } else if let Some(pair) = pairs.last_mut()
            && pair.change == RangeChange::Changed
        {
            // The interdiff is indented by four spaces; blank lines may lose theirs.
            let body = line.strip_prefix("    ").unwrap_or(line.trim_start());
            pair.interdiff.push(body.to_owned());
        }
    }
    for pair in &mut pairs {
        while pair
            .interdiff
            .last()
            .is_some_and(|line| line.trim().is_empty())
        {
            pair.interdiff.pop();
        }
    }
    pairs
}

/// What an interdiff line is, in the diff of the two patches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterdiffKind {
    /// The diff-of-patches' own hunk header (`@@ Metadata`, `@@ path`): in neither patch.
    Header,
    /// The same line in both patches.
    Context,
    /// In the old commit's patch only.
    Removed,
    /// In the new commit's patch only.
    Added,
}

/// One line of an interdiff, with where it sits in each commit's patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterdiffRow {
    pub kind: InterdiffKind,
    /// 1-based line in the old commit's patch, where the line is in it.
    pub old: Option<u32>,
    /// 1-based line in the new commit's patch, where the line is in it.
    pub new: Option<u32>,
    /// The patch line itself, without the interdiff's own `+`/`-`/` `. It keeps the
    /// patch's own marker, so ` one` is a context line of the file's diff and `+TWO` an
    /// added one; `## path ##` names the file the lines after it belong to, and a bare
    /// `@@` is that file's hunk header (git prints no line numbers in it).
    pub text: String,
}

/// An interdiff ([`RangePair::interdiff`]) with a line number from each side: the line's
/// place in the old commit's patch and in the new commit's patch. A line only one
/// side has is numbered only there, and the interdiff's own `@@` headers have no number
/// at all.
pub fn interdiff_rows(lines: &[String]) -> Vec<InterdiffRow> {
    let (mut old, mut new) = (0, 0);
    lines
        .iter()
        .map(|line| {
            let (kind, text) = match line.as_bytes().first() {
                Some(b'@') => (InterdiffKind::Header, line.clone()),
                Some(b'-') => (InterdiffKind::Removed, line[1..].to_owned()),
                Some(b'+') => (InterdiffKind::Added, line[1..].to_owned()),
                Some(b' ') => (InterdiffKind::Context, line[1..].to_owned()),
                // A line whose marker and content were both blank (git prints " ", and
                // trailing whitespace may be lost on the way).
                _ => (InterdiffKind::Context, line.clone()),
            };
            let count = |side: &mut u32| {
                *side += 1;
                Some(*side)
            };
            let (in_old, in_new) = match kind {
                InterdiffKind::Header => (None, None),
                InterdiffKind::Context => (count(&mut old), count(&mut new)),
                InterdiffKind::Removed => (count(&mut old), None),
                InterdiffKind::Added => (None, count(&mut new)),
            };
            InterdiffRow {
                kind,
                old: in_old,
                new: in_new,
                text,
            }
        })
        .collect()
}

/// The section an interdiff line opens, if it opens one: the lines after it belong to it.
/// Git writes ` ## name ##` as a line of the patch, and repeats the name in its own hunk
/// header, where it may carry more: `@@ name`, `@@ name: <the patch's own hunk context>`,
/// and `(new)`, `(gone)` or a mode change after the name. `Metadata` and `Commit message`
/// are sections of git's own; every other name is a file's path.
///
/// A path with `: ` in it would be cut there; git gives no way to tell the two apart.
pub fn interdiff_section(text: &str) -> Option<&str> {
    let name = match text.strip_prefix("@@ ") {
        // The patch's own hunk context follows the name.
        Some(name) => name.split_once(": ").map_or(name, |(name, _)| name),
        // The patch's own marker comes first ([`InterdiffRow::text`]).
        None => text
            .strip_prefix(' ')?
            .strip_prefix("## ")?
            .strip_suffix(" ##")?,
    };
    Some(name.split_once(" (").map_or(name, |(name, _)| name))
}

/// `1:  <id> ! 2:  <id> summary`, with `-` for a missing position and dashes for a
/// missing id.
fn parse_header(line: &str) -> Option<RangePair> {
    let (old_position, rest) = line.split_once(':')?;
    let (old_id, rest) = rest.trim_start().split_once(' ')?;
    let (mark, rest) = rest.split_once(' ')?;
    let (new_position, rest) = rest.split_once(':')?;
    let rest = rest.trim_start();
    let (new_id, summary) = rest.split_once(' ').unwrap_or((rest, ""));
    let change = match mark {
        "=" => RangeChange::Same,
        "!" => RangeChange::Changed,
        "<" => RangeChange::Removed,
        ">" => RangeChange::Added,
        _ => return None,
    };
    let old = side(old_position, old_id)?;
    let new = side(new_position, new_id)?;
    // The mark must agree with which sides are there.
    let expected = match change {
        RangeChange::Same | RangeChange::Changed => old.is_some() && new.is_some(),
        RangeChange::Removed => old.is_some() && new.is_none(),
        RangeChange::Added => old.is_none() && new.is_some(),
    };
    expected.then(|| RangePair {
        old,
        new,
        change,
        summary: summary.to_owned(),
        interdiff: Vec::new(),
    })
}

/// `Some(None)` for a missing side (`-` and dashes), `None` if it isn't a side at all.
fn side(position: &str, id: &str) -> Option<Option<(usize, ObjectId)>> {
    if position == "-" {
        return id.bytes().all(|b| b == b'-').then_some(None);
    }
    let position: usize = position.parse().ok()?;
    let id = ObjectId::from_hex(id.as_bytes()).ok()?;
    Some(Some((position, id)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "3a60057fc33a5ba2a8714e287786996240a18ac2";
    const B: &str = "55bec0e2c3654e1e82ff2cf1ab52d6a5e99fdeb8";
    const C: &str = "fd87a23cf6301a320b786f8b7594aaa9bedb74a0";
    const D: &str = "1f4d8456f0f1a0c93aa19f6dba4fcce04dcd7b71";
    const DASHES: &str = "----------------------------------------";

    fn id(hex: &str) -> ObjectId {
        ObjectId::from_hex(hex.as_bytes()).unwrap()
    }

    #[test]
    fn every_kind_of_pair() {
        let text = format!(
            "1:  {A} = 1:  {B} add a\n\
             2:  {C} ! 2:  {D} add b: with a colon\n    @@ b.txt (new)\n     16\n    -+17\n    ++seventeen\n\n\
             3:  {C} < -:  {DASHES} add c\n\
             -:  {DASHES} > 3:  {D} add d\n"
        );
        let pairs = parse_range_diff(&text);
        assert_eq!(pairs.len(), 4);
        assert_eq!(pairs[0].change, RangeChange::Same);
        assert_eq!(pairs[0].old, Some((1, id(A))));
        assert_eq!(pairs[0].new, Some((1, id(B))));
        assert_eq!(pairs[0].summary, "add a");
        assert!(pairs[0].interdiff.is_empty());

        assert_eq!(pairs[1].change, RangeChange::Changed);
        assert_eq!(pairs[1].summary, "add b: with a colon");
        assert_eq!(
            pairs[1].interdiff,
            ["@@ b.txt (new)", " 16", "-+17", "++seventeen"]
        );

        assert_eq!(pairs[2].change, RangeChange::Removed);
        assert_eq!((pairs[2].old, pairs[2].new), (Some((3, id(C))), None));
        assert_eq!(pairs[3].change, RangeChange::Added);
        assert_eq!((pairs[3].old, pairs[3].new), (None, Some((3, id(D)))));
    }

    /// Real `git range-diff` output, as `parse_range_diff` leaves it: the interdiff of a
    /// commit whose message and patch both changed.
    const INTERDIFF: [&str; 18] = [
        "@@ Metadata",
        " Author: F <f@e.com>",
        " ",
        "  ## Commit message ##",
        "-    change two",
        "+    change two, differently",
        " ",
        "  ## f.txt ##",
        " @@",
        "  one",
        " -two",
        "-+TWO",
        "++TWO!",
        "  three",
        "  four",
        "++five",
        " ",
        "  ## g.txt (new) ##",
    ];

    #[test]
    fn an_interdiff_line_is_numbered_in_each_patch() {
        let lines: Vec<String> = INTERDIFF.iter().map(|l| (*l).to_owned()).collect();
        let rows = interdiff_rows(&lines);
        let seen: Vec<(InterdiffKind, Option<u32>, Option<u32>, &str)> = rows
            .iter()
            .map(|row| (row.kind, row.old, row.new, row.text.as_str()))
            .collect();
        use InterdiffKind::*;
        assert_eq!(
            seen,
            [
                (Header, None, None, "@@ Metadata"),
                (Context, Some(1), Some(1), "Author: F <f@e.com>"),
                (Context, Some(2), Some(2), ""),
                (Context, Some(3), Some(3), " ## Commit message ##"),
                // The message line each patch has of its own.
                (Removed, Some(4), None, "    change two"),
                (Added, None, Some(4), "    change two, differently"),
                (Context, Some(5), Some(5), ""),
                (Context, Some(6), Some(6), " ## f.txt ##"),
                (Context, Some(7), Some(7), "@@"),
                (Context, Some(8), Some(8), " one"),
                (Context, Some(9), Some(9), "-two"),
                (Removed, Some(10), None, "+TWO"),
                (Added, None, Some(10), "+TWO!"),
                (Context, Some(11), Some(11), " three"),
                (Context, Some(12), Some(12), " four"),
                (Added, None, Some(13), "+five"),
                (Context, Some(13), Some(14), ""),
                (Context, Some(14), Some(15), " ## g.txt (new) ##"),
            ]
        );
    }

    #[test]
    fn the_section_markers_name_their_section() {
        assert_eq!(interdiff_section(" ## f.txt ##"), Some("f.txt"));
        assert_eq!(interdiff_section(" ## g.txt (new) ##"), Some("g.txt"));
        assert_eq!(interdiff_section(" ## a b.txt (gone) ##"), Some("a b.txt"));
        assert_eq!(
            interdiff_section(" ## Commit message ##"),
            Some("Commit message")
        );
        // Git repeats the name in its own hunk header when a hunk starts inside one, with
        // the patch's own hunk context after it.
        assert_eq!(interdiff_section("@@ Metadata"), Some("Metadata"));
        assert_eq!(interdiff_section("@@ b.txt (new)"), Some("b.txt"));
        assert_eq!(interdiff_section("@@ code.txt: line 1"), Some("code.txt"));
        assert_eq!(
            interdiff_section("@@ a.rs (new): fn main() {"),
            Some("a.rs")
        );
        // Not a marker: an ordinary patch line, or the patch's own hunk header.
        assert_eq!(interdiff_section("+TWO"), None);
        assert_eq!(interdiff_section("@@"), None);
        assert_eq!(interdiff_section(" one ## two"), None);
    }

    #[test]
    fn lines_that_only_look_like_headers_are_not_pairs() {
        // A mark that doesn't fit the sides, a bad id, a stray line.
        let text = format!("1:  {A} < 1:  {B} x\n1:  zzz = 1:  {B} y\nwarning: something\n");
        assert!(parse_range_diff(&text).is_empty());
    }
}
