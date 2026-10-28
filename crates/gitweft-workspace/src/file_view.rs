//! Centre panel showing a file as of a commit, with syntax colors. It opens from the tree
//! browser, and its header offers the file's blame and history. It also shows plain text
//! that isn't a file (a review's interdiff), without those. Long lines scroll sideways or
//! wrap ([`crate::code_scroll`]).

use std::ops::Range;

use gitweft_core::review::Anchor;
use gitweft_git::ObjectId;
use gitweft_git::gix::bstr::BString;
use gitweft_git::read::{InterdiffKind, InterdiffRow, interdiff_section};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::{Sizable, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::code_keys::{self, NextPage, PreviousPage, ScrollLeft, ScrollRight};
use crate::code_scroll::{self, HScroll};
use crate::file_text::FileText;
use crate::find::{FindEvent, FindSlot, OpenFind, find_matches};
use crate::list_keys::{self, FirstRow, LastRow, NextRow, PreviousRow, Step};
use gitweft_git::display::columns;
use gitweft_ui::LookExt;
use gitweft_ui::current::mono_font;
use gitweft_ui::widgets::centre_header;

/// The line number column, in design pixels.
const NUMBER_WIDTH: f32 = 48.;
/// Each of the two columns an interdiff has instead.
const SIDE_WIDTH: f32 = 34.;
pub(crate) const CONTEXT: &str = "FileView";

actions!(file, [CloseFile]);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", CloseFile, Some(CONTEXT)),
        KeyBinding::new("secondary-f", OpenFind, Some(CONTEXT)),
    ]);
}

pub enum FileEvent {
    Close,
    /// A partial clone lacks this version of the file: download it.
    FetchMissing {
        commit: ObjectId,
        path: BString,
    },
    Blame {
        commit: ObjectId,
        path: BString,
    },
    History {
        commit: ObjectId,
        path: BString,
    },
    /// A review comment asked for on a line of an interdiff: the file the line
    /// belongs to, and the line with its neighbours on the file's new side. Which line of
    /// the file that is comes from reading it at the new commit, which the workspace does.
    CommentOnInterdiff {
        file: String,
        anchor: Anchor,
    },
}

impl EventEmitter<FileEvent> for FileView {}

impl Focusable for FileView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

enum Content {
    Loading,
    Loaded(FileText),
    Failed(String),
}

pub struct FileView {
    /// The file shown, for its blame and history; `None` for other text.
    source: Option<(ObjectId, BString)>,
    /// It failed because a partial clone lacks this version: offer to fetch it.
    missing: bool,
    title: String,
    subtitle: String,
    content: Content,
    scroll: UniformListScrollHandle,
    /// The rows when long lines wrap (their heights differ).
    list: ListState,
    /// Whether the rows were drawn wrapped last.
    wrapped: bool,
    hscroll: HScroll,
    /// The longest line, in columns.
    longest: usize,
    /// One number column's width, for the largest number (from the last frame).
    number_width: Pixels,
    focus: FocusHandle,
    /// Take keyboard focus on the next frame, so Esc works.
    focus_on_render: bool,
    /// Whether it had the keyboard focus when last drawn (the cursor shows then).
    focused: bool,
    /// The line the keys are on.
    cursor: Option<usize>,
    /// For a review's interdiff: where each line sits in the old and the new commit's
    /// patch, and which of the two has it. Empty for ordinary text.
    sides: Vec<InterdiffRow>,
    /// Which interdiff lines can carry a comment, worked out once: every drawn row
    /// asks, and the answer scans back to the line's section.
    commentable: Vec<bool>,
    /// The interdiff line picked to comment on.
    comment_line: Option<usize>,
    /// Ctrl+F's bar.
    find: FindSlot,
}

impl FileView {
    /// The header's title (the headless UI tests).
    #[cfg(test)]
    pub(crate) fn title(&self) -> &str {
        &self.title
    }

    /// The commit and path shown; `None` for text that isn't a file.
    pub fn source(&self) -> Option<&(ObjectId, BString)> {
        self.source.as_ref()
    }

    pub fn new(commit: ObjectId, path: BString, cx: &mut Context<Self>) -> Self {
        let mut view = Self::text(
            path.to_string(),
            t!("file.at_commit", sha = commit.to_hex_with_len(8)).into_owned(),
            cx,
        );
        view.source = Some((commit, path));
        view
    }

    /// Text that isn't a file in the repository: no blame or history.
    pub fn text(title: String, subtitle: String, cx: &mut Context<Self>) -> Self {
        Self {
            source: None,
            missing: false,
            title,
            subtitle,
            content: Content::Loading,
            scroll: UniformListScrollHandle::new(),
            list: code_scroll::list_state(0),
            wrapped: code_scroll::wrapping(cx),
            hscroll: HScroll::default(),
            longest: 0,
            number_width: px(NUMBER_WIDTH),
            focus: cx.focus_handle(),
            focus_on_render: true,
            focused: false,
            cursor: None,
            sides: Vec::new(),
            commentable: Vec::new(),
            comment_line: None,
            find: FindSlot::default(),
        }
    }

    /// A review's interdiff: the patch lines (the interdiff's own `+`/`-`/` ` stripped)
    /// with `rows` saying where each sits in the two patches.
    pub fn set_interdiff(
        &mut self,
        rows: Vec<InterdiffRow>,
        result: Result<FileText, String>,
        cx: &mut Context<Self>,
    ) {
        self.commentable = (0..rows.len())
            .map(|ix| interdiff_comment(&rows, ix).is_some())
            .collect();
        self.sides = rows;
        self.comment_line = None;
        self.set_text(result, cx);
    }

    /// Picks interdiff row `ix` to comment on, or unpicks it.
    fn pick_comment_line(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.comment_line = (self.comment_line != Some(ix)).then_some(ix);
        cx.notify();
    }

    /// Asks for a comment on the picked interdiff line.
    fn comment_on_picked(&mut self, cx: &mut Context<Self>) {
        let Some((file, anchor)) = self
            .comment_line
            .and_then(|ix| interdiff_comment(&self.sides, ix))
        else {
            return;
        };
        self.comment_line = None;
        cx.emit(FileEvent::CommentOnInterdiff { file, anchor });
        cx.notify();
    }

    /// The bar over the lines while one is picked to comment on.
    fn render_comment_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let ix = self.comment_line?;
        let (file, _) = interdiff_comment(&self.sides, ix)?;
        let (p, m) = (*cx.palette(), *cx.metrics());
        Some(
            h_flex()
                .px_3()
                .py_1()
                .gap_2()
                .text_size(m.text.small)
                .bg(p.panel2)
                .border_b_1()
                .border_color(p.line)
                .child(
                    div()
                        .flex_1()
                        .child(t!("review.picked_patch_line", file = file).into_owned()),
                )
                .child(
                    Button::new("comment-patch-line")
                        .xsmall()
                        .primary()
                        .label(t!("review.comment_button").into_owned())
                        .on_click(cx.listener(|this, _, _, cx| this.comment_on_picked(cx))),
                )
                .child(
                    Button::new("clear-patch-line")
                        .xsmall()
                        .ghost()
                        .label(t!("diff.clear_selection").into_owned())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.comment_line = None;
                            cx.notify();
                        })),
                )
                .into_any_element(),
        )
    }

    /// How wide the numbers are: two columns for an interdiff, one otherwise.
    fn number_room(&self) -> Pixels {
        if self.sides.is_empty() {
            self.number_width
        } else {
            self.number_width * 2.
        }
    }

    /// The largest number a column shows: the last line's, or an interdiff's largest.
    fn largest_number(&self) -> usize {
        if self.sides.is_empty() {
            match &self.content {
                Content::Loaded(text) => text.lines.len(),
                _ => 0,
            }
        } else {
            let numbers = self.sides.iter().flat_map(|row| [row.old, row.new]);
            numbers.flatten().max().unwrap_or(0) as usize
        }
    }

    /// This version isn't in the partial clone, and reads don't download it: says
    /// so, with "Fetch missing contents".
    pub fn set_missing(&mut self, cx: &mut Context<Self>) {
        self.set_text(Err(t!("file.missing_objects").into_owned()), cx);
        self.missing = true;
        cx.notify();
    }

    pub fn set_text(&mut self, result: Result<FileText, String>, cx: &mut Context<Self>) {
        self.missing = false;
        self.content = match result {
            Ok(text) => {
                self.longest = text
                    .lines
                    .iter()
                    .map(|line| columns(line))
                    .max()
                    .unwrap_or(0);
                self.list.reset(text.lines.len());
                self.cursor = self
                    .cursor
                    .filter(|_| !text.lines.is_empty())
                    .map(|ix| ix.min(text.lines.len() - 1));
                Content::Loaded(text)
            }
            Err(message) => Content::Failed(message),
        };
        // New text: an open find bar's matches are found in it again.
        self.refind(cx);
        cx.notify();
    }

    /// Takes the keys again once a view shown on top of it closes.
    pub fn refocus(&mut self, cx: &mut Context<Self>) {
        self.focus_on_render = true;
        cx.notify();
    }

    fn close(&mut self, _: &CloseFile, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(FileEvent::Close);
    }

    fn line_count(&self) -> usize {
        match &self.content {
            Content::Loaded(text) => text.lines.len(),
            Content::Loading | Content::Failed(_) => 0,
        }
    }

    /// Puts the cursor on line `ix` and scrolls by the least that shows it.
    fn put_cursor(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.cursor = Some(ix);
        code_keys::reveal(self.wrapped, &self.scroll, &self.list, ix);
        cx.notify();
    }

    /// The arrows, Home and End move the cursor through the lines.
    fn move_cursor(&mut self, to: Step, cx: &mut Context<Self>) {
        if let Some(ix) = list_keys::step(self.line_count(), self.cursor, to) {
            self.put_cursor(ix, cx);
        }
    }

    /// Page Down / Page Up: a view's worth of lines (less one).
    fn page(&mut self, down: bool, cx: &mut Context<Self>) {
        let height = code_keys::view_height(self.wrapped, &self.scroll, &self.list);
        let rows = code_keys::page_rows(height, cx.metrics().code_line);
        if let Some(ix) = code_keys::page_target(self.line_count(), self.cursor, down, rows) {
            self.put_cursor(ix, cx);
        }
    }

    /// Left / Right: the lines scrolled sideways. Ctrl+F: the find bar.
    fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.find.open(window, cx, Self::on_find);
    }

    fn on_find(&mut self, event: &FindEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            FindEvent::Changed => self.refind(cx),
            FindEvent::Show => {
                if let Some(found) = self.find.current(cx) {
                    self.put_cursor(found.row, cx);
                    if let Content::Loaded(text) = &self.content
                        && !self.wrapped
                    {
                        let line = &text.lines[found.row];
                        let width = code_scroll::char_width(window, cx.metrics().text.mono);
                        let at = |byte: usize| width * columns(&line[..byte]) as f32;
                        self.hscroll
                            .reveal(at(found.range.start), at(found.range.end));
                    }
                }
            }
            FindEvent::Closed => {
                self.focus.focus(window, cx);
                cx.notify();
            }
        }
    }

    /// Finds the bar's query in the lines again: it changed, or the text did.
    fn refind(&mut self, cx: &mut Context<Self>) {
        let matches = match (self.find.query(cx), &self.content) {
            (Some((query, case)), Content::Loaded(text)) => find_matches(
                text.lines.iter().map(|line| line.as_ref()).enumerate(),
                &query,
                case,
            ),
            _ => Vec::new(),
        };
        self.find.set_matches(matches, self.cursor.unwrap_or(0), cx);
        cx.notify();
    }

    fn scroll_sideways(&mut self, left: bool, window: &mut Window, cx: &mut Context<Self>) {
        let step = code_keys::sideways_step(window, cx);
        if self.hscroll.scroll_by(if left { step } else { -step }) {
            cx.notify();
        }
    }

    fn render_rows(
        &mut self,
        range: Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        range.map(|ix| self.render_row(ix, false, cx)).collect()
    }

    /// Line `ix`, with the keys' cursor outlined while the view has the focus.
    fn render_row(&self, ix: usize, wrap: bool, cx: &mut Context<Self>) -> AnyElement {
        let row = self.render_line(ix, wrap, cx);
        if self.focused && self.cursor == Some(ix) {
            code_keys::cursor_ring(row, cx)
        } else {
            row
        }
    }

    fn render_line(&self, ix: usize, wrap: bool, cx: &mut Context<Self>) -> AnyElement {
        let Content::Loaded(text) = &self.content else {
            return div().into_any_element();
        };
        let (p, m) = (*cx.palette(), *cx.metrics());
        let width = self.number_width;
        let number = |value: String| {
            div()
                .w(width)
                .flex_shrink_0()
                .pr_2()
                .text_right()
                .text_color(p.fg3)
                .child(value)
        };
        // An interdiff carries the line's place in each patch, and the tint of the side
        // that has it; ordinary text is numbered from 1 and has no tint.
        let row = self.sides.get(ix);
        let numbers: Vec<Div> = match row {
            None => vec![number((ix + 1).to_string())],
            Some(row) => [row.old, row.new]
                .into_iter()
                .map(|side| number(side.map(|n| n.to_string()).unwrap_or_default()))
                .collect(),
        };
        let picked = self.comment_line == Some(ix);
        let background = match row.map(|row| row.kind) {
            _ if picked => Some(p.acc.opacity(0.35)),
            Some(InterdiffKind::Header) => Some(p.panel2),
            Some(InterdiffKind::Removed) => Some(p.del_bg),
            Some(InterdiffKind::Added) => Some(p.add_bg),
            Some(InterdiffKind::Context) | None => None,
        };
        // A line of the new commit's patch can carry a review comment.
        let pickable = self.commentable.get(ix).copied().unwrap_or(false);
        code_scroll::code_row(wrap, m.code_line)
            .id(("file-line", ix))
            .w_full()
            .when_some(background, |this, bg| this.bg(bg))
            .when(pickable, |this| {
                this.cursor_pointer().on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                        this.pick_comment_line(ix, cx)
                    }),
                )
            })
            .children(numbers)
            .child(code_scroll::code_text(
                StyledText::new(text.lines[ix].clone()).with_highlights(combine_highlights(
                    text.highlights(ix),
                    self.find.highlights(ix, p.find_bg, cx),
                )),
                wrap,
                self.hscroll.offset(),
            ))
            .into_any_element()
    }
}

/// A row that ends a run of consecutive file lines: the interdiff's own hunk header (git
/// left part of the patch out there), a `## name ##` marker, or the patch's own `@@`.
fn breaks_run(row: &InterdiffRow) -> bool {
    row.kind == InterdiffKind::Header || row.text == "@@" || interdiff_section(&row.text).is_some()
}

/// What a comment picked on interdiff row `ix` is about: the file, and the line with its
/// neighbours on that file's new side. `None` when the row can't carry a comment.
///
/// Only a line the **new** commit's patch has can, and only where it is on the file's new
/// side — the patch's own `+` (a line the commit adds) or ` ` (one it keeps). A line only
/// the old patch has belongs to the commit as it was, which the branch no longer has;
/// git's own `Metadata` and `Commit message` sections are not files.
fn interdiff_comment(rows: &[InterdiffRow], ix: usize) -> Option<(String, Anchor)> {
    let row = rows.get(ix)?;
    if row.new.is_none() || breaks_run(row) {
        return None;
    }
    row.text.strip_prefix(['+', ' '])?;
    let file = rows[..=ix]
        .iter()
        .rev()
        .find_map(|row| interdiff_section(&row.text))
        .filter(|name| *name != "Metadata" && *name != "Commit message")?;
    // The run around it: the lines of the file's new side that are consecutive in the file.
    let start = rows[..ix]
        .iter()
        .rposition(breaks_run)
        .map_or(0, |at| at + 1);
    let end = ix
        + 1
        + rows[ix + 1..]
            .iter()
            .position(breaks_run)
            .unwrap_or(rows.len() - ix - 1);
    let on_side: Vec<(usize, &str)> = rows[start..end]
        .iter()
        .enumerate()
        .filter(|(_, row)| row.new.is_some())
        .filter_map(|(k, row)| Some((start + k, row.text.strip_prefix(['+', ' '])?)))
        .collect();
    let position = on_side.iter().position(|(at, _)| *at == ix)?;
    let texts: Vec<&str> = on_side.iter().map(|(_, text)| *text).collect();
    Some((file.to_owned(), Anchor::at(&texts, position)))
}

impl Render for FileView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.focus_on_render) {
            self.focus.focus(window, cx);
        }
        self.focused = self.focus.contains_focused(window, cx);
        let (p, m) = (*cx.palette(), *cx.metrics());
        let (muted, danger) = (p.fg3, p.del);
        let wrap = code_scroll::wrapping(cx);
        if wrap != self.wrapped {
            if let Content::Loaded(text) = &self.content {
                let lines = text.lines.len();
                code_scroll::switch_mode(wrap, &self.scroll, &self.list, lines, m.code_line);
            }
            self.wrapped = wrap;
        }
        // Wide enough for the largest number, with the column's `pr_2`.
        let minimum = if self.sides.is_empty() {
            NUMBER_WIDTH
        } else {
            SIDE_WIDTH
        };
        self.number_width = code_scroll::number_width(
            window,
            m.text.mono,
            self.largest_number(),
            code_scroll::pr_2(&m),
            px(minimum * m.zoom),
        );
        if !wrap {
            let content = code_scroll::text_width(window, m.text.mono, self.longest);
            let visible = self.hscroll.viewport() - self.number_room();
            self.hscroll.update(content, visible);
        }

        let header = centre_header(self.title.clone(), self.subtitle.clone(), &p, &m)
            .when_some(self.source.clone(), |this, (commit, path)| {
                let history_path = path.clone();
                this.child(
                    Button::new("file-blame")
                        .small()
                        .ghost()
                        .label(t!("blame.open").into_owned())
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(FileEvent::Blame {
                                commit,
                                path: path.clone(),
                            })
                        })),
                )
                .child(
                    Button::new("file-history")
                        .small()
                        .ghost()
                        .label(t!("history.open").into_owned())
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(FileEvent::History {
                                commit,
                                path: history_path.clone(),
                            })
                        })),
                )
            })
            .child(code_scroll::wrap_button(wrap))
            .child(
                Button::new("close-file")
                    .small()
                    .ghost()
                    .label(t!("diff.close").into_owned())
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(FileEvent::Close))),
            );

        let placeholder = |text: String, color: Hsla| {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_color(color)
                .child(text)
                .into_any_element()
        };
        let body = match &self.content {
            Content::Loading => placeholder(t!("repo.loading").into_owned(), muted),
            Content::Failed(message) if self.missing => v_flex()
                .flex_1()
                .gap_3()
                .items_center()
                .justify_center()
                .text_color(muted)
                .child(message.clone())
                .children(self.source.clone().map(|(commit, path)| {
                    Button::new("fetch-missing-file")
                        .debug_selector(|| "fetch-missing-file".into())
                        .small()
                        .primary()
                        .label(t!("diff.fetch_missing").into_owned())
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(FileEvent::FetchMissing {
                                commit,
                                path: path.clone(),
                            })
                        }))
                }))
                .into_any_element(),
            Content::Failed(message) => placeholder(message.clone(), danger),
            Content::Loaded(text) if text.lines.is_empty() => {
                placeholder(t!("file.empty").into_owned(), muted)
            }
            Content::Loaded(text) => {
                let lines = if wrap {
                    list(
                        self.list.clone(),
                        cx.processor(|this, ix, _, cx| this.render_row(ix, true, cx)),
                    )
                    .size_full()
                    .into_any_element()
                } else {
                    uniform_list(
                        "file-lines",
                        text.lines.len(),
                        cx.processor(Self::render_rows),
                    )
                    .track_scroll(&self.scroll)
                    .size_full()
                    .into_any_element()
                };
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .font_family(mono_font())
                    .text_size(m.text.mono)
                    .child(lines)
                    .when(!wrap, |this| {
                        this.child(code_scroll::overlay(
                            cx.entity().downgrade(),
                            |this: &mut Self| &mut this.hscroll,
                            &self.hscroll,
                            &p,
                            &m,
                        ))
                    })
                    .into_any_element()
            }
        };

        v_flex()
            .size_full()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::close))
            .on_action(cx.listener(|this, _: &OpenFind, window, cx| this.open_find(window, cx)))
            .on_action(cx.listener(|this, _: &NextRow, _, cx| this.move_cursor(Step::Next, cx)))
            .on_action(
                cx.listener(|this, _: &PreviousRow, _, cx| this.move_cursor(Step::Previous, cx)),
            )
            .on_action(cx.listener(|this, _: &FirstRow, _, cx| this.move_cursor(Step::First, cx)))
            .on_action(cx.listener(|this, _: &LastRow, _, cx| this.move_cursor(Step::Last, cx)))
            .on_action(cx.listener(|this, _: &NextPage, _, cx| this.page(true, cx)))
            .on_action(cx.listener(|this, _: &PreviousPage, _, cx| this.page(false, cx)))
            .on_action(cx.listener(|this, _: &ScrollLeft, window, cx| {
                this.scroll_sideways(true, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ScrollRight, window, cx| {
                this.scroll_sideways(false, window, cx)
            }))
            .child(header)
            .children(self.find.element(cx))
            .children(self.render_comment_bar(cx))
            .child(body)
    }
}

/// Which lines of an interdiff can carry a review comment, and what it anchors to.
#[cfg(test)]
mod tests {
    use gitweft_git::read::interdiff_rows;
    // Rust's own `#[test]`: `gpui_kit::*` (through `super::*`) brings GPUI's `test` macro.
    use std::prelude::v1::test;

    use super::*;

    /// Real `git range-diff` output for a commit whose message and patch both changed,
    /// as `parse_range_diff` leaves it.
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

    fn rows() -> Vec<InterdiffRow> {
        let lines: Vec<String> = INTERDIFF.iter().map(|l| (*l).to_owned()).collect();
        interdiff_rows(&lines)
    }

    #[test]
    fn only_the_new_patchs_lines_on_a_files_new_side_can_be_commented_on() {
        let rows = rows();
        let commentable: Vec<(usize, &str)> = (0..rows.len())
            .filter(|ix| interdiff_comment(&rows, *ix).is_some())
            .map(|ix| (ix, INTERDIFF[ix]))
            .collect();
        assert_eq!(
            commentable,
            [
                (9, "  one"),   // kept by both patches, a context line of f.txt
                (12, "++TWO!"), // only the new patch has it, and it adds the line
                (13, "  three"),
                (14, "  four"),
                (15, "++five"),
            ]
        );
        // Left out, each for its own reason:
        //  0  the interdiff's own header       3  a `## name ##` marker
        //  1  the Metadata section             5  the commit message, not a file
        //  8  the patch's own `@@`            10  the file's old side (`-two`)
        //  11 only the old patch has it       17 a `## name ##` marker
    }

    #[test]
    fn the_anchor_is_the_files_new_side_around_the_line() {
        let rows = rows();
        let (file, anchor) = interdiff_comment(&rows, 12).expect("`+TWO!` can be commented on");
        assert_eq!(file, "f.txt");
        assert_eq!(anchor.text, "TWO!");
        // The lines around it on f.txt's new side, the patch's own markers stripped: the
        // old side's `-two` and the line only the old patch has are not there.
        assert_eq!(anchor.before, ["one"]);
        assert_eq!(anchor.after, ["three", "four"]);

        // A run stops at the interdiff's own gaps, so `one` has nothing before it.
        let (_, anchor) = interdiff_comment(&rows, 9).unwrap();
        assert_eq!(anchor.text, "one");
        assert!(anchor.before.is_empty(), "{anchor:?}");
        assert_eq!(anchor.after, ["TWO!", "three"]);
    }

    /// When git leaves the start of a patch out, no `## name ##` line is shown: the file
    /// comes from git's own header, which carries the patch's hunk context after the name
    /// (`@@ code.txt: line 1`). The review fixture's reworked commit, as git prints it.
    #[test]
    fn the_file_can_come_from_gits_own_header() {
        let lines: Vec<String> = [
            "@@ code.txt: line 1",
            "  line 4",
            "-+line 5, reviewed",
            "- line 6",
            "+-line 6",
            "++line 5, reviewed again",
            "++line 6, now too",
            "  line 11",
        ]
        .iter()
        .map(|l| (*l).to_owned())
        .collect();
        let rows = interdiff_rows(&lines);
        let (file, anchor) = interdiff_comment(&rows, 5).expect("a line the new patch adds");
        assert_eq!(file, "code.txt");
        assert_eq!(anchor.text, "line 5, reviewed again");
        // The new side around it: `line 4` before (the old-only lines and the new patch's
        // `-line 6` aren't on it), `line 6, now too` and `line 11` after.
        assert_eq!(anchor.before, ["line 4"]);
        assert_eq!(anchor.after, ["line 6, now too", "line 11"]);
        // Old-only lines, and the new patch's removal of `line 6`, take no comment.
        for ix in [2, 3, 4] {
            assert!(interdiff_comment(&rows, ix).is_none(), "row {ix}");
        }
    }
}

/// Find in a real view: Ctrl+F, typing, Enter, Shift+Enter, Alt+C and Escape.
#[cfg(test)]
mod find_tests {
    use gitweft_testkit::Sandbox;
    use gitweft_ui::SettingsStore;
    // Rust's own `#[test]`: `gpui_kit::*` (through `super::*`) brings GPUI's `test` macro.
    use std::prelude::v1::test;

    use super::*;

    #[test]
    fn ctrl_f_finds_and_the_keys_move_between_matches() {
        let sb = Sandbox::new();
        let mut cx = TestAppContext::single();
        cx.update(|cx| {
            gpui_kit::init(cx);
            SettingsStore::init_with_file(cx, &sb.path().join("settings.json"));
            bind_keys(cx);
            crate::find::bind_keys(cx);
            crate::list_keys::bind_keys(cx);
            code_keys::bind_keys(cx);
        });
        let (view, window) = cx.add_window_view(|_, cx| {
            let mut view = FileView::text("t.txt".into(), String::new(), cx);
            let bytes = b"alpha\nbeta\nAlphabet\ngamma alpha\n";
            let text = crate::file_text::file_text(bytes, "t.txt", true, 4).unwrap();
            view.set_text(Ok(text), cx);
            view
        });
        let settle = |window: &mut VisualTestContext| {
            window.run_until_parked();
            window.update(|window, cx| window.draw(cx).clear(cx));
            window.run_until_parked();
        };
        let cursor = |window: &mut VisualTestContext| window.update(|_, cx| view.read(cx).cursor);
        settle(window);

        window.simulate_keystrokes("secondary-f");
        settle(window);
        window.simulate_input("alpha");
        settle(window);
        // Case doesn't matter: lines 0, 2 and 3 match, the first is current.
        assert_eq!(cursor(window), Some(0));
        window.simulate_keystrokes("enter");
        settle(window);
        assert_eq!(cursor(window), Some(2));
        window.simulate_keystrokes("enter enter");
        settle(window);
        assert_eq!(cursor(window), Some(0), "wraps around");
        window.simulate_keystrokes("shift-enter");
        settle(window);
        assert_eq!(cursor(window), Some(3));

        // Matching case: "Alphabet" is out; from line 3, Enter goes back to line 0.
        window.simulate_keystrokes("alt-c");
        settle(window);
        window.simulate_keystrokes("f3");
        settle(window);
        assert_eq!(cursor(window), Some(0));
        window.simulate_keystrokes("f3");
        settle(window);
        assert_eq!(cursor(window), Some(3));

        // Escape closes the bar (not the view) and gives the keys back to the view.
        window.simulate_keystrokes("escape");
        settle(window);
        let open = window.update(|_, cx| view.read(cx).find.query(cx).is_some());
        assert!(!open, "the bar closed");
        window.simulate_keystrokes("up");
        settle(window);
        assert_eq!(
            cursor(window),
            Some(2),
            "the arrows move the view's cursor again"
        );
    }
}
