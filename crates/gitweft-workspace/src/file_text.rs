//! A file's text prepared for display: lines with tabs expanded to tab stops, plus syntax colors. It is
//! built on a background job; the views only map the colors to text styles.

use std::ops::Range;

use gitweft_syntax::{Spans, highlight_lines, syntax_for_path};
use gpui_kit::{HighlightStyle, SharedString};

use crate::diff_view::rgb;

/// Files longer than this are shown without syntax colors, so the text appears quickly.
const MAX_HIGHLIGHT_LINES: usize = 5_000;
/// Bytes checked for a NUL to tell binary files apart (git's own heuristic).
const BINARY_PROBE: usize = 8_000;

pub struct FileText {
    pub lines: Vec<SharedString>,
    /// One span list per line; empty when not highlighted.
    pub syntax: Vec<Spans>,
}

/// `None` for a binary file. Tabs stop `tab` columns apart.
pub fn file_text(bytes: &[u8], path: &str, dark: bool, tab: usize) -> Option<FileText> {
    if bytes[..bytes.len().min(BINARY_PROBE)].contains(&0) {
        return None;
    }
    // Git counts lines by `\n`, like `str::lines`, which also drops a `\r` before it.
    let lines: Vec<String> = String::from_utf8_lossy(bytes)
        .lines()
        .map(|line| gitweft_git::display::expand_tabs(line, tab).into_owned())
        .collect();
    let syntax = (lines.len() <= MAX_HIGHLIGHT_LINES)
        .then(|| syntax_for_path(path))
        .flatten()
        .map(|syntax| {
            let texts: Vec<&str> = lines.iter().map(String::as_str).collect();
            highlight_lines(syntax, &texts, dark)
        })
        .unwrap_or_default();
    Some(FileText {
        lines: lines.into_iter().map(SharedString::from).collect(),
        syntax,
    })
}

impl FileText {
    /// Text styles for line `ix`'s syntax colors.
    pub fn highlights(&self, ix: usize) -> Vec<(Range<usize>, HighlightStyle)> {
        self.syntax
            .get(ix)
            .into_iter()
            .flatten()
            .map(|(range, color)| {
                let style = HighlightStyle {
                    color: Some(rgb(color).into()),
                    ..HighlightStyle::default()
                };
                (range.clone(), style)
            })
            .collect()
    }
}
