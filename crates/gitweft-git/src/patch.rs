//! Partial staging: patches built from selected lines of a [`FileDiff`].
//!
//! A patch keeps every context line and the *selected* changed lines. Unselected changes
//! are neutralised so they stay exactly as they are in the file the patch is applied to:
//!
//! | target                                   | unselected `-` | unselected `+` |
//! |------------------------------------------|----------------|----------------|
//! | [`PatchTarget::Forward`] (stage)          | context        | dropped        |
//! | [`PatchTarget::Reverse`] (unstage/discard)| dropped        | context        |
//!
//! Apply with [`crate::ops::apply_patch`] (`git apply --recount`, all-or-nothing).

use std::collections::BTreeSet;
use std::io::Write;

use gix::bstr::BString;

use crate::read::{FileDiff, Hunk, LineKind};

/// Selected changed lines as `(hunk index, line index)`; context lines are always kept.
pub type LineSelection = BTreeSet<(usize, usize)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchTarget {
    /// Applied forward onto the diff's old side: stage unstaged changes into the index.
    Forward,
    /// Applied in reverse onto the diff's new side: unstage from the index, or discard
    /// from the working tree.
    Reverse,
}

/// Whether single hunks/lines of this diff can be applied. Deletions, renames, symlinks,
/// binary and truncated diffs are handled as whole files instead.
pub fn supports_partial(diff: &FileDiff) -> bool {
    let deletion = diff
        .hunks
        .first()
        .is_some_and(|h| h.new_start == 0 && h.new_len == 0);
    let symlink = diff.new_file_mode.as_deref() == Some("120000");
    !diff.binary
        && !diff.truncated
        && diff.old_path.is_none()
        && !diff.hunks.is_empty()
        && !deletion
        && !symlink
}

/// The diff creates the file: an untracked file, or a new file compared with HEAD.
///
/// An all-added hunk (`@@ -0,0 +1,n @@`) alone isn't enough: a tracked empty file that
/// gains lines shows the same way, but it exists on the old side (no `new file mode`).
pub fn is_creation(diff: &FileDiff) -> bool {
    diff.new_file_mode.is_some()
        && diff
            .hunks
            .first()
            .is_some_and(|h| h.old_start == 0 && h.old_len == 0)
}

/// Every changed line of the given hunks (for "stage hunk").
pub fn select_hunks(diff: &FileDiff, hunks: impl IntoIterator<Item = usize>) -> LineSelection {
    let mut selection = LineSelection::new();
    for h in hunks {
        if let Some(hunk) = diff.hunks.get(h) {
            selection.extend(
                hunk.lines
                    .iter()
                    .enumerate()
                    .filter(|(_, line)| line.kind != LineKind::Context)
                    .map(|(l, _)| (h, l)),
            );
        }
    }
    selection
}

/// Builds the patch for `selection`, or `None` when no changed line is selected.
pub fn build_patch(
    diff: &FileDiff,
    selection: &LineSelection,
    target: PatchTarget,
) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    // Staging part of a new file creates it in the index with just those lines. Unstaging
    // or discarding part of one changes a file that exists on the target side.
    let creation_mode = (target == PatchTarget::Forward && is_creation(diff))
        .then(|| diff.new_file_mode.as_deref().unwrap_or("100644"));
    write_header(&mut out, &diff.path, creation_mode);

    // Line-count difference (new - old) of the hunks already written: it moves the start
    // of later hunks on the side that is *not* the target (the target side is exact).
    let mut shift: i64 = 0;
    let mut wrote_any = false;
    for (h, hunk) in diff.hunks.iter().enumerate() {
        let Some(body) = hunk_body(h, hunk, selection, target) else {
            continue;
        };
        let (old_start, new_start) = match target {
            PatchTarget::Forward => {
                let old = start(hunk.old_start, body.old_len, hunk.old_len);
                (old, shifted(old, shift, body.new_len))
            }
            PatchTarget::Reverse => {
                let new = start(hunk.new_start, body.new_len, hunk.new_len);
                (shifted(new, -shift, body.old_len), new)
            }
        };
        let _ = writeln!(
            out,
            "@@ -{old_start},{} +{new_start},{} @@",
            body.old_len, body.new_len
        );
        out.extend_from_slice(&body.text);
        shift += i64::from(body.new_len) - i64::from(body.old_len);
        wrote_any = true;
    }
    wrote_any.then_some(out)
}

struct HunkBody {
    text: Vec<u8>,
    old_len: u32,
    new_len: u32,
}

fn hunk_body(
    h: usize,
    hunk: &Hunk,
    selection: &LineSelection,
    target: PatchTarget,
) -> Option<HunkBody> {
    let mut body = HunkBody {
        text: Vec::new(),
        old_len: 0,
        new_len: 0,
    };
    let mut changes = 0;
    for (l, line) in hunk.lines.iter().enumerate() {
        let selected = selection.contains(&(h, l));
        let tag = match (line.kind, selected, target) {
            (LineKind::Context, _, _) => b' ',
            (LineKind::Removed, true, _) => b'-',
            (LineKind::Added, true, _) => b'+',
            (LineKind::Removed, false, PatchTarget::Forward) => b' ',
            (LineKind::Added, false, PatchTarget::Reverse) => b' ',
            // Unselected and not part of the target: leave it out entirely.
            (LineKind::Removed, false, PatchTarget::Reverse)
            | (LineKind::Added, false, PatchTarget::Forward) => continue,
        };
        match tag {
            b' ' => {
                body.old_len += 1;
                body.new_len += 1;
            }
            b'-' => {
                body.old_len += 1;
                changes += 1;
            }
            _ => {
                body.new_len += 1;
                changes += 1;
            }
        }
        body.text.push(tag);
        body.text.extend_from_slice(&line.text);
        body.text.push(b'\n');
        if line.no_newline {
            body.text
                .extend_from_slice(b"\\ No newline at end of file\n");
        }
    }
    (changes > 0).then_some(body)
}

/// Unified-diff convention: an empty side starts at the line *before* the hunk.
fn start(original_start: u32, len: u32, original_len: u32) -> u32 {
    if len == 0 && original_len > 0 {
        original_start.saturating_sub(1)
    } else {
        original_start
    }
}

fn shifted(start: u32, shift: i64, len: u32) -> u32 {
    let moved = (i64::from(start) + shift).max(0) as u32;
    if len == 0 { moved } else { moved.max(1) }
}

/// `diff --git`, `---` and `+++` lines; with `creation_mode`, a new-file header instead
/// (`new file mode …`, `--- /dev/null`).
fn write_header(out: &mut Vec<u8>, path: &BString, creation_mode: Option<&str>) {
    let (old, new) = (quote_path(b"a/", path), quote_path(b"b/", path));
    // Like git, end `---`/`+++` names containing spaces with a TAB so they parse exactly.
    let tab: &[u8] = if path.contains(&b' ') { b"\t" } else { b"" };
    out.extend_from_slice(b"diff --git ");
    out.extend_from_slice(&old);
    out.push(b' ');
    out.extend_from_slice(&new);
    out.push(b'\n');
    if let Some(mode) = creation_mode {
        let _ = writeln!(out, "new file mode {mode}");
        out.extend_from_slice(b"--- /dev/null\n");
    } else {
        out.extend_from_slice(b"--- ");
        out.extend_from_slice(&old);
        out.extend_from_slice(tab);
        out.push(b'\n');
    }
    out.extend_from_slice(b"+++ ");
    out.extend_from_slice(&new);
    out.extend_from_slice(tab);
    out.push(b'\n');
}

/// `prefix + path`, C-quoted the way git quotes when the path has special bytes.
fn quote_path(prefix: &[u8], path: &[u8]) -> Vec<u8> {
    let needs_quotes = path
        .iter()
        .any(|&b| b == b'"' || b == b'\\' || b < 0x20 || b == 0x7f);
    let mut out = Vec::with_capacity(path.len() + prefix.len() + 2);
    if !needs_quotes {
        out.extend_from_slice(prefix);
        out.extend_from_slice(path);
        return out;
    }
    out.push(b'"');
    out.extend_from_slice(prefix);
    for &byte in path {
        match byte {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b if b < 0x20 || b == 0x7f => {
                let _ = write!(out, "\\{b:03o}");
            }
            b => out.push(b),
        }
    }
    out.push(b'"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::parse_unified;

    const DIFF: &str = "\
diff --git a/f b/f
--- a/f
+++ b/f
@@ -1,4 +1,4 @@
 a
-b
+B
 c
-d
+D
";

    #[test]
    fn forward_keeps_unselected_removals_as_context() {
        let diff = &parse_unified(DIFF.as_bytes())[0];
        // Select only "-b" and "+B" (lines 1 and 2).
        let selection: LineSelection = [(0, 1), (0, 2)].into();
        let patch = String::from_utf8(build_patch(diff, &selection, PatchTarget::Forward).unwrap())
            .unwrap();
        assert!(
            patch.ends_with("@@ -1,4 +1,4 @@\n a\n-b\n+B\n c\n d\n"),
            "{patch}"
        );
    }

    #[test]
    fn reverse_keeps_unselected_additions_as_context() {
        let diff = &parse_unified(DIFF.as_bytes())[0];
        let selection: LineSelection = [(0, 4), (0, 5)].into(); // "-d", "+D"
        let patch = String::from_utf8(build_patch(diff, &selection, PatchTarget::Reverse).unwrap())
            .unwrap();
        assert!(
            patch.ends_with("@@ -1,4 +1,4 @@\n a\n B\n c\n-d\n+D\n"),
            "{patch}"
        );
    }

    #[test]
    fn nothing_selected_means_no_patch() {
        let diff = &parse_unified(DIFF.as_bytes())[0];
        assert!(build_patch(diff, &LineSelection::new(), PatchTarget::Forward).is_none());
        assert_eq!(select_hunks(diff, [0]).len(), 4);
    }

    #[test]
    fn quoting_matches_git() {
        assert_eq!(quote_path(b"a/", b"plain name.txt"), b"a/plain name.txt");
        assert_eq!(quote_path(b"a/", b"tab\there"), b"\"a/tab\\there\"");
        assert_eq!(quote_path(b"b/", b"q\"x"), b"\"b/q\\\"x\"");
    }
}
