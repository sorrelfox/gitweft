//! Syntax highlighting for diff lines: syntect with bat's syntax collection (two-face),
//! using the pure-Rust regex engine. Everything is compiled in; nothing is downloaded.
//!
//! Loading the syntax set takes tens of milliseconds, so it happens lazily on the first
//! highlight, which runs on a background job, never on the UI thread.

use std::ops::Range;
use std::sync::OnceLock;

use two_face::re_exports::syntect;
use two_face::theme::EmbeddedThemeName;

use syntect::easy::HighlightLines;
use syntect::highlighting::Theme;
use syntect::parsing::{SyntaxReference, SyntaxSet};

/// A foreground color, independent of any UI toolkit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgb(pub u8, pub u8, pub u8);

/// Colored byte ranges of one line.
pub type Spans = Vec<(Range<usize>, Rgb)>;

fn syntaxes() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(two_face::syntax::extra_newlines)
}

fn theme(dark: bool) -> &'static Theme {
    static DARK: OnceLock<Theme> = OnceLock::new();
    static LIGHT: OnceLock<Theme> = OnceLock::new();
    let (cell, name) = if dark {
        (&DARK, EmbeddedThemeName::OneHalfDark)
    } else {
        (&LIGHT, EmbeddedThemeName::OneHalfLight)
    };
    cell.get_or_init(|| two_face::theme::extra().get(name).clone())
}

/// A handle to the syntax detected for a file.
#[derive(Clone, Copy)]
pub struct Syntax(&'static SyntaxReference);

impl Syntax {
    pub fn name(&self) -> &'static str {
        &self.0.name
    }
}

/// The syntax for a repository path, by extension or file name (`Cargo.toml`,
/// `Dockerfile`, `Makefile`). `None` for plain text or unknown files.
pub fn syntax_for_path(path: &str) -> Option<Syntax> {
    let set = syntaxes();
    let file_name = path.rsplit('/').next().unwrap_or(path);
    let by_extension = file_name
        .rsplit_once('.')
        .and_then(|(_, ext)| set.find_syntax_by_extension(ext));
    by_extension
        .or_else(|| set.find_syntax_by_extension(file_name))
        .filter(|syntax| syntax.name != "Plain Text")
        .map(Syntax)
}

/// Lines longer than this (minified code, data) are shown without colors: syntect's
/// regexes on a multi-megabyte line take seconds (#25).
pub const MAX_HIGHLIGHT_LINE: usize = 4 * 1024;

/// Highlights consecutive lines of one file (e.g. one side of a hunk) as continuous text,
/// so constructs spanning lines (block comments, strings) are colored correctly within it.
/// Returns one span list per input line; ranges are byte offsets into that line. A line
/// over [`MAX_HIGHLIGHT_LINE`] gets none and is left out of the text the others are read in.
pub fn highlight_lines(syntax: Syntax, lines: &[&str], dark: bool) -> Vec<Spans> {
    let set = syntaxes();
    let mut highlighter = HighlightLines::new(syntax.0, theme(dark));
    let mut buffer = String::new();
    lines
        .iter()
        .map(|line| {
            if line.len() > MAX_HIGHLIGHT_LINE {
                return Vec::new();
            }
            buffer.clear();
            buffer.push_str(line);
            buffer.push('\n');
            let Ok(regions) = highlighter.highlight_line(&buffer, set) else {
                return Vec::new();
            };
            let mut spans = Vec::with_capacity(regions.len());
            let mut offset = 0;
            for (style, text) in regions {
                let end = (offset + text.len()).min(line.len());
                if offset < end {
                    let color = style.foreground;
                    spans.push((offset..end, Rgb(color.r, color.g, color.b)));
                }
                offset += text.len();
            }
            spans
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minified_line_is_left_plain_and_its_neighbours_colored() {
        let syntax = syntax_for_path("app.js").unwrap();
        let long = "var a=1;".repeat(200_000);
        let started = std::time::Instant::now();
        let spans = highlight_lines(syntax, &["let x = 1;", &long, "let y = 2;"], true);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert!(!spans[0].is_empty());
        assert!(spans[1].is_empty());
        assert!(!spans[2].is_empty());
    }

    #[test]
    fn detects_syntaxes_by_extension_and_name() {
        assert_eq!(syntax_for_path("src/main.rs").unwrap().name(), "Rust");
        assert!(syntax_for_path("Cargo.toml").is_some());
        assert!(syntax_for_path("web/app.tsx").is_some());
        assert!(syntax_for_path("notes.unknown-ext").is_none());
        assert!(syntax_for_path("README").is_none());
    }

    #[test]
    fn highlights_rust_with_several_colors_and_valid_ranges() {
        let syntax = syntax_for_path("x.rs").unwrap();
        let lines = ["fn main() {", "    let s = \"text\"; // comment", "}"];
        let spans = highlight_lines(syntax, &lines, true);
        assert_eq!(spans.len(), 3);
        for (line, line_spans) in lines.iter().zip(&spans) {
            for (range, _) in line_spans {
                assert!(line.get(range.clone()).is_some(), "{range:?} in {line:?}");
            }
        }
        let colors: std::collections::HashSet<_> = spans[1].iter().map(|(_, c)| *c).collect();
        assert!(
            colors.len() >= 3,
            "keyword, string and comment differ: {colors:?}"
        );
    }

    #[test]
    fn block_comments_continue_across_lines() {
        let syntax = syntax_for_path("x.rs").unwrap();
        let spans = highlight_lines(syntax, &["/* start", "still comment */ fn x() {}"], false);
        let comment = spans[0][0].1;
        assert_eq!(
            spans[1][0].1, comment,
            "second line starts inside the comment"
        );
    }
}

/// The licenses of the syntax definitions and themes compiled in (bat's collection, through
/// two-face), as Markdown. `assets/notices/syntaxes-and-themes.md` holds this text and the
/// release notices append it (`scripts/dist/sources.py`); the test below fails when
/// two-face's collection changes, so the file can't go stale (#57).
pub fn acknowledgements() -> String {
    two_face::acknowledgement::listing().to_md()
}

#[cfg(test)]
mod acknowledgement_tests {
    /// The committed notices are what two-face ships. Rewrite them with
    /// `GITWEFT_BLESS=1 cargo test -p gitweft-syntax`. Line endings don't count: a few of
    /// the license texts come with CRLF, which git normalises on a checkout or commit
    /// depending on the user's `core.autocrlf`, so the file is kept and compared with LF.
    #[test]
    fn the_committed_notices_are_two_faces_own() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets/notices/syntaxes-and-themes.md");
        let theirs = super::acknowledgements().replace("\r\n", "\n");
        if std::env::var_os("GITWEFT_BLESS").is_some() {
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("the notices dir");
            std::fs::write(&path, &theirs).expect("writing the notices");
            return;
        }
        let ours = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .replace("\r\n", "\n");
        assert!(
            ours == theirs,
            "assets/notices/syntaxes-and-themes.md is stale ({} bytes, two-face has {}): \
             rerun with GITWEFT_BLESS=1",
            ours.len(),
            theirs.len(),
        );
    }
}
