//! The commit columns that the file history and the comparison share with the graph: the
//! message, then the author, date and SHA at the graph's design widths, in its fonts and
//! colors. The header and the rows line up because both sit in the same padding and gaps
//! ([`widgets::list_header`], [`widgets::list_row`]).

use gitweft_git::ObjectId;
use gitweft_ui::current::mono_font;
use gitweft_ui::truncated::{Truncated, truncated};
use gitweft_ui::{Metrics, Palette, widgets};
use gpui_kit::*;
use rust_i18n::t;

use crate::graph_view::{
    DEFAULT_AUTHOR_WIDTH, DEFAULT_DATE_WIDTH, MIN_COLUMN, MIN_MESSAGE_WIDTH, SHA_WIDTH,
};

/// One commit as a list row shows it.
pub(crate) struct CommitRow {
    pub summary: String,
    /// Shown after the summary, in the tertiary color ("renamed from …").
    pub note: Option<String>,
    pub author: String,
    /// Already formatted (`util::format_time`).
    pub date: String,
    pub id: ObjectId,
}

/// The columns' widths in this frame (zoomed).
struct Widths {
    min_message: Pixels,
    author: Pixels,
    min_author: Pixels,
    date: Pixels,
    sha: Pixels,
}

impl Widths {
    fn new(m: &Metrics) -> Self {
        let z = |value: f32| px(value * m.zoom);
        Self {
            min_message: z(MIN_MESSAGE_WIDTH),
            author: z(DEFAULT_AUTHOR_WIDTH),
            min_author: z(MIN_COLUMN),
            date: z(DEFAULT_DATE_WIDTH),
            sha: z(SHA_WIDTH),
        }
    }
}

/// The message column takes the room left, the author shrinks in a narrow panel (down to
/// the graph's minimum), and the date and SHA keep their widths.
/// Each cell's text is cut to fit, and shows whole on hover when it is.
fn message(w: &Widths, text: impl Into<SharedString>) -> Truncated {
    truncated("message", text).flex_1().min_w(w.min_message)
}

fn author(w: &Widths, text: impl Into<SharedString>) -> Truncated {
    truncated("author", text).w(w.author).min_w(w.min_author)
}

fn fixed(id: &'static str, width: Pixels, text: impl Into<SharedString>) -> Truncated {
    truncated(id, text).w(width).flex_shrink_0()
}

/// The column labels over rows of [`cells`]: Commit message, Author, Date, SHA.
pub(crate) fn header(p: &Palette, m: &Metrics) -> Div {
    let w = Widths::new(m);
    widgets::list_header(p, m)
        .child(message(&w, t!("graph.column.message").into_owned()))
        .child(author(&w, t!("graph.column.author").into_owned()))
        .child(fixed("date", w.date, t!("graph.column.date").into_owned()))
        .child(fixed("sha", w.sha, t!("graph.column.sha").into_owned()))
}

/// A commit's cells, to put in a [`widgets::list_row`]: the summary in the text color
/// (its note after it in the tertiary one), the author in the secondary color, the date
/// and SHA in mono.
pub(crate) fn cells(row: CommitRow, p: &Palette, m: &Metrics) -> [AnyElement; 4] {
    let w = Widths::new(m);
    let mono = mono_font();
    // The note follows the summary in the same line of text, so a narrow column cuts the
    // note before the summary.
    let summary = match row.note {
        Some(note) => {
            let start = row.summary.len() + 2;
            let range = start..start + note.len();
            let note_style = HighlightStyle {
                color: Some(p.fg3),
                ..HighlightStyle::default()
            };
            message(&w, format!("{}  {note}", row.summary))
                .highlights(vec![(range, note_style)])
                .text_color(p.fg)
        }
        None => message(&w, row.summary).text_color(p.fg),
    };
    [
        summary.into_any_element(),
        author(&w, row.author).text_color(p.fg2).into_any_element(),
        fixed("date", w.date, row.date)
            .font_family(mono.clone())
            .text_size(m.text.mono)
            .text_color(p.fg2)
            .into_any_element(),
        fixed("sha", w.sha, row.id.to_hex_with_len(8).to_string())
            .font_family(mono)
            .text_size(m.text.mono)
            .text_color(p.fg3)
            .into_any_element(),
    ]
}
