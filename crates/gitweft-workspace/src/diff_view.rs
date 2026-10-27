//! Centre panel showing one file's diff (replaces the graph while open).
//!
//! Unified or side-by-side (split) layout. Rows between hunks show how many unchanged
//! lines are hidden; clicking one asks for more context, and "Full file" shows everything.
//! Whitespace changes can be ignored (`git diff -w`).
//!
//! For working-tree diffs, hunks and single lines can be staged, unstaged or discarded
//! here: click a changed line to select it, Shift-click to extend, Ctrl-click to toggle.
//! Actions are emitted as ready-built patches; the workspace applies them.

use std::sync::Arc;

use gitweft_core::review::{Anchor, Comment, Side};
use gitweft_diff::{LineSide, split_rows};
use gitweft_git::display::columns;
use gitweft_git::gix::bstr::{BString, ByteSlice};
use gitweft_git::lfs::{LfsPointer, pointer_sides};
use gitweft_git::patch::{
    LineSelection, PatchTarget, build_patch, is_creation, select_hunks, supports_partial,
};
use gitweft_git::read::{DiffOptions, DiffSource, FULL_CONTEXT, FileDiff, LineKind};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Selectable, Sizable, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::code_keys::{
    self, NextHunk, NextPage, PreviousHunk, PreviousPage, ScrollLeft, ScrollRight,
};
use crate::code_scroll::{self, HScroll};
use crate::diff_decor::{Decorations, ImageSides};
use crate::find::{FindEvent, FindSlot, OpenFind, find_matches};
use crate::list_keys::{
    self, ExtendToFirstRow, ExtendToLastRow, ExtendToNextRow, ExtendToPreviousRow, FirstRow,
    LastRow, NextRow, PreviousRow, Step,
};
use crate::palette::{PaletteCommand, view_command};
use gitweft_ui::LookExt;
use gitweft_ui::current::mono_font;
use gitweft_ui::widgets::centre_header;

pub(crate) const CONTEXT: &str = "DiffView";
/// Extra context lines per click on a hidden-lines row.
const CONTEXT_STEP: u32 = 10;

/// The diff's letter keys: not while a text field in it (find) has the focus.
const LETTER_KEYS: &str = "DiffView && !Input";

actions!(diff, [CloseDiff, CommentOnLine]);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", CloseDiff, Some(CONTEXT)),
        KeyBinding::new("ctrl-f", OpenFind, Some(CONTEXT)),
        KeyBinding::new("c", CommentOnLine, Some(LETTER_KEYS)),
    ]);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchAction {
    Stage,
    Unstage,
    Discard,
}

/// How the two sides are laid out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DiffLayout {
    #[default]
    Unified,
    Split,
}

pub enum DiffEvent {
    Close,
    /// Apply `patch` to the file at `path` (discards are already confirmed).
    Apply {
        action: PatchAction,
        patch: Vec<u8>,
        path: BString,
    },
    /// The user changed how the diff is computed: reload it with these options.
    Reload(DiffOptions),
    /// The user switched layouts (remembered for the next diff).
    LayoutChanged(DiffLayout),
    /// Unstage or discard the whole file: every line of a new file was selected, and a
    /// patch would leave an empty file behind (discards are already confirmed).
    WholeFile {
        action: PatchAction,
        path: BString,
    },
    /// Review diffs: comment on a line (its side, its 1-based number, and its anchor).
    Comment {
        side: Side,
        line: u32,
        anchor: Anchor,
    },
    /// Review diffs: a comment row was clicked.
    OpenComment(u64),
    /// A partial clone lacks this diff's file contents: download them (#82).
    FetchMissing,
}

impl EventEmitter<DiffEvent> for DiffView {}

impl Focusable for DiffView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// What can be done with the lines of the shown diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineActions {
    None,
    /// Unstaged changes: stage into the index or discard from the working tree.
    StageOrDiscard,
    /// Staged changes: unstage from the index.
    Unstage,
}

enum Content {
    Loading,
    Loaded(FileDiff, Box<Decorations>),
    Failed(String),
}

/// One visible row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    /// Unchanged lines hidden before the next hunk.
    Gap {
        hidden: u32,
    },
    Hunk(usize),
    /// Unified layout: line `.1` of hunk `.0`.
    Line(usize, usize),
    /// Split layout: old-side and new-side line of hunk `.0` (the same index for context).
    Split(usize, Option<usize>, Option<usize>),
    /// Review diffs: comment `.0` (of the file's comments), under its line.
    Comment(usize),
}

/// Line `l` of hunk `h` counted through the whole diff: find's key for it (#43).
fn line_key(diff: &FileDiff, h: usize, l: usize) -> usize {
    diff.hunks[..h]
        .iter()
        .map(|hunk| hunk.lines.len())
        .sum::<usize>()
        + l
}

/// The `(hunk, line)` of [`line_key`] `key`.
fn line_at(diff: &FileDiff, mut key: usize) -> Option<(usize, usize)> {
    for (h, hunk) in diff.hunks.iter().enumerate() {
        if key < hunk.lines.len() {
            return Some((h, key));
        }
        key -= hunk.lines.len();
    }
    None
}

/// The changed or context lines a row shows, as `(hunk, line)`.
fn row_lines(row: Row) -> [Option<(usize, usize)>; 2] {
    match row {
        Row::Line(h, l) => [Some((h, l)), None],
        Row::Split(h, old, new) => [
            old.map(|l| (h, l)),
            new.filter(|n| Some(*n) != old).map(|l| (h, l)),
        ],
        Row::Gap { .. } | Row::Hunk(_) | Row::Comment(_) => [None, None],
    }
}

/// Which side a comment on `line` belongs to, and its number there: removed lines are on
/// the old side, context and added lines on the new one.
fn comment_place(line: &gitweft_git::read::DiffLine) -> Option<(Side, u32)> {
    match line.kind {
        LineKind::Removed => line.old_line.map(|n| (Side::Old, n)),
        LineKind::Context | LineKind::Added => line.new_line.map(|n| (Side::New, n)),
    }
}

/// The first line of a comment, for a one-line row, with "…" when more lines follow.
pub(crate) fn first_line(text: &str) -> String {
    let mut lines = text.lines();
    let first = lines.next().unwrap_or("");
    match lines.next() {
        Some(_) => format!("{first} …"),
        None => first.to_owned(),
    }
}

/// What a whole hunk can do for `actions`, with the labels: the hunk header's links and
/// the palette.
fn hunk_actions(actions: LineActions) -> Vec<(PatchAction, String)> {
    match actions {
        LineActions::StageOrDiscard => vec![
            (PatchAction::Stage, t!("diff.stage_hunk").into_owned()),
            (PatchAction::Discard, t!("diff.discard_hunk").into_owned()),
        ],
        LineActions::Unstage => vec![(PatchAction::Unstage, t!("diff.unstage_hunk").into_owned())],
        LineActions::None => Vec::new(),
    }
}

/// What the selected lines can do for `actions`, with the selection bar's labels.
fn selection_actions(actions: LineActions) -> Vec<(PatchAction, String)> {
    match actions {
        LineActions::StageOrDiscard => vec![
            (PatchAction::Stage, t!("diff.stage_lines").into_owned()),
            (
                PatchAction::Discard,
                t!("diff.discard_lines_button").into_owned(),
            ),
        ],
        LineActions::Unstage => vec![(PatchAction::Unstage, t!("diff.unstage_lines").into_owned())],
        LineActions::None => Vec::new(),
    }
}

/// The element id of a hunk header's link.
fn hunk_link_id(action: PatchAction) -> &'static str {
    match action {
        PatchAction::Stage => "stage-hunk",
        PatchAction::Discard => "discard-hunk",
        PatchAction::Unstage => "unstage-hunk",
    }
}

/// The next (`forward`) or previous hunk header's row from `from` (no cursor: the first
/// hunk, or the last going back); `None` past the last or first.
fn hunk_row(rows: &[Row], from: Option<usize>, forward: bool) -> Option<usize> {
    let is_hunk = |ix: &usize| matches!(rows[*ix], Row::Hunk(_));
    match (from, forward) {
        (None, true) => (0..rows.len()).find(is_hunk),
        (None, false) => (0..rows.len()).rev().find(is_hunk),
        (Some(at), true) => (at + 1..rows.len()).find(is_hunk),
        (Some(at), false) => (0..at.min(rows.len())).rev().find(is_hunk),
    }
}

/// Unchanged lines between the previous hunk (or the file start) and each hunk.
fn hidden_before(diff: &FileDiff) -> Vec<u32> {
    let mut next_unshown = 1;
    diff.hunks
        .iter()
        .map(|hunk| {
            let hidden = hunk.old_start.saturating_sub(next_unshown);
            next_unshown = hunk.old_start + hunk.old_len;
            hidden
        })
        .collect()
}

/// A line's selection marker and its +/- sign column, in design pixels.
const MARKER_WIDTH: f32 = 3.;
const SIGN_WIDTH: f32 = 16.;

/// Colors and sizes of the rows, from the palette and metrics, once per frame.
#[derive(Clone, Copy)]
struct RowStyle {
    /// Behind find's matches (#43).
    find_bg: Hsla,
    muted: Hsla,
    foreground: Hsla,
    border: Hsla,
    added_bg: Hsla,
    removed_bg: Hsla,
    /// Changed words (and selected lines) get a stronger tint than their line.
    added_strong: Hsla,
    removed_strong: Hsla,
    /// Moved lines, in git's colours (magenta where they left, cyan where they arrived);
    /// every other move is a shade stronger, so neighbouring moves stand apart.
    moved_old: [Hsla; 2],
    moved_new: [Hsla; 2],
    /// Whether moved lines get their tint (the Diff setting, #35).
    tint_moved: bool,
    hunk_bg: Hsla,
    marker: Hsla,
    /// The empty side of a split row.
    blank: Hsla,
    /// Hunk header text.
    hunk_fg: Hsla,
    line_height: Pixels,
    number_width: Pixels,
    marker_width: Pixels,
    sign_width: Pixels,
    /// Long lines wrap (the rows grow) rather than scroll sideways.
    wrap: bool,
}

impl RowStyle {
    fn new(cx: &App) -> Self {
        let (p, m) = (cx.palette(), cx.metrics());
        let stronger = |color: Hsla| Hsla {
            a: (color.a * 1.9).min(1.),
            ..color
        };
        Self {
            muted: p.fg3,
            foreground: p.fg,
            border: p.line,
            added_bg: p.add_bg,
            removed_bg: p.del_bg,
            added_strong: p.add.opacity(0.35),
            removed_strong: p.del.opacity(0.35),
            moved_old: [p.moved_from_bg, stronger(p.moved_from_bg)],
            moved_new: [p.moved_to_bg, stronger(p.moved_to_bg)],
            tint_moved: !gitweft_ui::SettingsStore::settings(cx)
                .diff
                .plain_moved_lines,
            hunk_bg: p.panel2,
            marker: p.acc,
            blank: p.panel2,
            hunk_fg: p.fg2,
            find_bg: p.find_bg,
            line_height: m.code_line,
            marker_width: m.dp(MARKER_WIDTH),
            sign_width: m.dp(SIGN_WIDTH),
            number_width: px(48. * m.zoom),
            wrap: false,
        }
    }
}

pub struct DiffView {
    title: String,
    subtitle: String,
    source: DiffSource,
    options: DiffOptions,
    layout: DiffLayout,
    content: Content,
    rows: Vec<Row>,
    scroll: UniformListScrollHandle,
    /// The rows when long lines wrap (their heights differ).
    list: ListState,
    /// Whether the rows were drawn wrapped last.
    wrapped: bool,
    hscroll: HScroll,
    /// The longest line, in columns (with its "no newline" note).
    longest: usize,
    /// The largest line number, and the number columns' width for it (from the last frame).
    largest_number: usize,
    number_width: Pixels,
    focus: FocusHandle,
    /// Take keyboard focus on the first frame, so Esc works right away (not when opened
    /// from the staging list, which keeps the keys: [`Self::set_focus_on_open`]).
    focus_on_render: bool,
    /// Whether it had the keyboard focus when last drawn.
    focused: bool,
    /// Selected changed lines, as `(hunk, line)`.
    selection: LineSelection,
    /// Where a Shift-click selection range starts.
    anchor: Option<(usize, usize)>,
    /// The row the keys act on (#28); a click on a line puts it there too.
    cursor: Option<usize>,
    /// Ctrl+F's bar (#43). Its matches are keyed by [`line_key`]: a split row holds two lines.
    find: FindSlot,
    /// Where Shift with the arrows started selecting, as a row.
    row_anchor: Option<usize>,
    /// Both versions of a binary image, shown as pictures instead of "binary file".
    images: Option<ImagePair>,
    /// A file stored with Git LFS: the pointer on each side, shown instead of its lines.
    lfs: Option<(Option<LfsPointer>, Option<LfsPointer>)>,
    /// Review diffs: the file's comments, shown under their lines. `None` outside a
    /// review, where lines can't be commented on.
    comments: Option<Vec<Comment>>,
    /// Review diffs: the line picked to comment on, as `(hunk, line)`.
    comment_line: Option<(usize, usize)>,
    /// A diff since the last review: only new-side lines take comments (its old side is
    /// the version reviewed last, which comments aren't anchored to).
    comment_new_only: bool,
    /// It failed because a partial clone lacks the file's contents (#82): offer to fetch.
    missing_objects: bool,
}

impl DiffView {
    pub fn new(
        title: String,
        subtitle: String,
        source: DiffSource,
        options: DiffOptions,
        layout: DiffLayout,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            title,
            subtitle,
            source,
            options,
            layout,
            content: Content::Loading,
            rows: Vec::new(),
            scroll: UniformListScrollHandle::new(),
            list: code_scroll::list_state(0),
            wrapped: code_scroll::wrapping(cx),
            hscroll: HScroll::default(),
            longest: 0,
            largest_number: 0,
            number_width: cx.metrics().dp(48.),
            focus: cx.focus_handle(),
            focus_on_render: true,
            focused: false,
            selection: LineSelection::new(),
            anchor: None,
            cursor: None,
            find: FindSlot::default(),
            row_anchor: None,
            images: None,
            lfs: None,
            comments: None,
            comment_line: None,
            comment_new_only: false,
            missing_objects: false,
        }
    }

    /// Whether the diff takes the keys when it's first drawn: not when opened from the
    /// staging list (the list keeps them), and only when the diff it replaces had them
    /// when it follows a file to its other side.
    pub(crate) fn set_focus_on_open(&mut self, focus: bool) {
        self.focus_on_render = focus;
    }

    /// What the find bar holds, while it's open (the headless UI tests).
    #[cfg(test)]
    pub(crate) fn find_query(&self, cx: &App) -> Option<String> {
        self.find.query(cx).map(|(query, _)| query)
    }

    /// Whether it had the keyboard focus when last drawn.
    pub(crate) fn has_focus(&self) -> bool {
        self.focused
    }

    /// Makes this a review diff showing `comments` (or a plain one with `None`); with
    /// `new_only`, only the new side's lines take comments and show them.
    pub fn set_comments(
        &mut self,
        comments: Option<Vec<Comment>>,
        new_only: bool,
        cx: &mut Context<Self>,
    ) {
        self.comment_new_only = new_only;
        self.comments = comments.map(|all| {
            all.into_iter()
                .filter(|c| !new_only || c.side == Side::New)
                .collect()
        });
        if self.comments.is_none() {
            self.comment_line = None;
        }
        self.rebuild_rows();
        cx.notify();
    }

    fn pick_comment_line(&mut self, h: usize, l: usize, cx: &mut Context<Self>) {
        self.comment_line = if self.comment_line == Some((h, l)) {
            None
        } else {
            Some((h, l))
        };
        cx.notify();
    }

    /// Whether `line` takes a comment: any line in a review, but a diff since the last
    /// review takes them on its new side only.
    fn takes_comment(&self, line: &gitweft_git::read::DiffLine) -> bool {
        self.comments.is_some() && !(self.comment_new_only && line.kind == LineKind::Removed)
    }

    /// The line a comment would go on, as `(hunk, line)`: the one picked by a click, else
    /// the keys' cursor's (a split row's new side when it has one).
    fn line_to_comment(&self) -> Option<(usize, usize)> {
        if self.comment_line.is_some() {
            return self.comment_line;
        }
        let Content::Loaded(diff, _) = &self.content else {
            return None;
        };
        let [first, second] = row_lines(*self.rows.get(self.cursor?)?);
        second
            .or(first)
            .filter(|(h, l)| self.takes_comment(&diff.hunks[*h].lines[*l]))
    }

    /// C: comments on the cursor's line (or the picked one), as "Comment…" does (#85).
    fn comment_on_cursor(&mut self, _: &CommentOnLine, _: &mut Window, cx: &mut Context<Self>) {
        match self.line_to_comment() {
            Some(line) => self.comment_on(line, cx),
            None => cx.propagate(),
        }
    }

    fn comment_on_picked(&mut self, cx: &mut Context<Self>) {
        if let Some(line) = self.comment_line {
            self.comment_on(line, cx);
        }
    }

    /// Asks to comment on line `l` of hunk `h`. Its anchor comes from the hunk's lines on
    /// the same side, which are consecutive lines of the file.
    fn comment_on(&mut self, (h, l): (usize, usize), cx: &mut Context<Self>) {
        let Content::Loaded(diff, _) = &self.content else {
            return;
        };
        let hunk = &diff.hunks[h];
        let Some((side, line)) = comment_place(&hunk.lines[l]) else {
            return;
        };
        let on_side: Vec<(usize, String)> = hunk
            .lines
            .iter()
            .enumerate()
            .filter(|(_, other)| match side {
                Side::Old => other.kind != LineKind::Added,
                Side::New => other.kind != LineKind::Removed,
            })
            .map(|(ix, other)| {
                let text = String::from_utf8_lossy(&other.text);
                (ix, text.trim_end_matches('\r').to_owned())
            })
            .collect();
        let Some(position) = on_side.iter().position(|(ix, _)| *ix == l) else {
            return;
        };
        let texts: Vec<&str> = on_side.iter().map(|(_, text)| text.as_str()).collect();
        let anchor = Anchor::at(&texts, position);
        self.comment_line = None;
        cx.emit(DiffEvent::Comment { side, line, anchor });
        cx.notify();
    }

    /// The diff couldn't be read because this partial clone lacks the file's contents, and
    /// reads don't download them by themselves (#82): says so, with "Fetch missing
    /// contents".
    pub fn set_missing(&mut self, cx: &mut Context<Self>) {
        self.set_diff(Err(t!("diff.missing_objects").into_owned()), cx);
        self.missing_objects = true;
        cx.notify();
    }

    pub fn set_diff(
        &mut self,
        result: Result<(FileDiff, Decorations), String>,
        cx: &mut Context<Self>,
    ) {
        // A reload triggered by some other file changing keeps the selected lines.
        let unchanged = matches!(
            (&self.content, &result),
            (Content::Loaded(old, _), Ok((new, _))) if old == new
        );
        if !unchanged {
            self.selection.clear();
            self.anchor = None;
            self.comment_line = None;
        }
        self.images = None;
        self.lfs = None;
        self.missing_objects = false;
        self.content = match result {
            Ok((diff, mut decorations)) => {
                // The pictures are made once, here: a new `Image` in every frame would be
                // decoded again each time.
                self.images = decorations.images.take().and_then(image_pair);
                self.lfs = pointer_sides(&diff);
                self.longest = diff
                    .hunks
                    .iter()
                    .flat_map(|hunk| &hunk.lines)
                    .map(|line| {
                        let note = if line.no_newline { 24 } else { 0 };
                        columns(&line.display_text(decorations.tab_width)) + note
                    })
                    .max()
                    .unwrap_or(0);
                self.largest_number = diff
                    .hunks
                    .iter()
                    .flat_map(|hunk| [hunk.old_start + hunk.old_len, hunk.new_start + hunk.new_len])
                    .max()
                    .unwrap_or(0) as usize;
                // Debug builds: preselect the first hunk so screenshot-driven checks can
                // see the selection UI without mouse input.
                #[cfg(debug_assertions)]
                if std::env::var_os("GITWEFT_DEBUG_SELECT_HUNK").is_some() {
                    self.selection = select_hunks(&diff, [0]);
                }
                Content::Loaded(diff, Box::new(decorations))
            }
            Err(message) => Content::Failed(message),
        };
        self.rebuild_rows();
        // A new diff: an open find bar's matches are found in it again.
        self.refind(cx);
        cx.notify();
    }

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        let Content::Loaded(diff, _) = &self.content else {
            self.list.reset(0);
            return;
        };
        let show_gaps = self.source != DiffSource::Untracked && self.options.context < FULL_CONTEXT;
        // The comments on a line, as indexes into the file's comments.
        let comments = self.comments.as_deref().unwrap_or_default();
        let comments_on = |h: usize, l: usize| -> Vec<Row> {
            let place = comment_place(&diff.hunks[h].lines[l]);
            comments
                .iter()
                .enumerate()
                .filter(|(_, comment)| place == Some((comment.side, comment.line)))
                .map(|(ix, _)| Row::Comment(ix))
                .collect()
        };
        for ((h, hunk), hidden) in diff.hunks.iter().enumerate().zip(hidden_before(diff)) {
            if show_gaps && hidden > 0 {
                self.rows.push(Row::Gap { hidden });
            }
            self.rows.push(Row::Hunk(h));
            match self.layout {
                DiffLayout::Unified => {
                    for l in 0..hunk.lines.len() {
                        self.rows.push(Row::Line(h, l));
                        self.rows.extend(comments_on(h, l));
                    }
                }
                DiffLayout::Split if !comments.is_empty() => {
                    let sides: Vec<LineSide> = hunk
                        .lines
                        .iter()
                        .map(|line| match line.kind {
                            LineKind::Context => LineSide::Context,
                            LineKind::Removed => LineSide::Removed,
                            LineKind::Added => LineSide::Added,
                        })
                        .collect();
                    for (old, new) in split_rows(&sides) {
                        self.rows.push(Row::Split(h, old, new));
                        for l in [old, new.filter(|n| Some(*n) != old)].into_iter().flatten() {
                            self.rows.extend(comments_on(h, l));
                        }
                    }
                }
                DiffLayout::Split => {
                    let sides: Vec<LineSide> = hunk
                        .lines
                        .iter()
                        .map(|line| match line.kind {
                            LineKind::Context => LineSide::Context,
                            LineKind::Removed => LineSide::Removed,
                            LineKind::Added => LineSide::Added,
                        })
                        .collect();
                    self.rows.extend(
                        split_rows(&sides)
                            .into_iter()
                            .map(|(old, new)| Row::Split(h, old, new)),
                    );
                }
            }
        }
        // Comments whose line is in no hunk shown (unchanged now, and outside the context)
        // come first, so they can still be seen and opened (#35); "Full file" places them.
        let placed: std::collections::HashSet<usize> = self
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Comment(ix) => Some(*ix),
                _ => None,
            })
            .collect();
        let unplaced: Vec<Row> = (0..comments.len())
            .filter(|ix| !placed.contains(ix))
            .map(Row::Comment)
            .collect();
        self.rows.splice(0..0, unplaced);
        self.list.reset(self.rows.len());
        // The rows changed (a reload after staging lines, the layout): the cursor stays near
        // where it was.
        self.cursor = self
            .cursor
            .filter(|_| !self.rows.is_empty())
            .map(|ix| ix.min(self.rows.len() - 1));
        self.row_anchor = None;
    }

    fn line_actions(&self) -> LineActions {
        let Content::Loaded(diff, _) = &self.content else {
            return LineActions::None;
        };
        // A whitespace-blind diff doesn't match the file, so no patch can come from it.
        // (Untracked files are shown whole; the option doesn't apply to them.)
        let whitespace_blind =
            self.options.ignore_whitespace && self.source != DiffSource::Untracked;
        if whitespace_blind || !supports_partial(diff) {
            return LineActions::None;
        }
        match self.source {
            DiffSource::Unstaged | DiffSource::Untracked => LineActions::StageOrDiscard,
            DiffSource::Staged => LineActions::Unstage,
            DiffSource::Commit { .. } => LineActions::None,
        }
    }

    fn set_options(&mut self, options: DiffOptions, cx: &mut Context<Self>) {
        if options == self.options {
            return;
        }
        self.options = options;
        cx.emit(DiffEvent::Reload(options));
        cx.notify();
    }

    fn more_context(&mut self, cx: &mut Context<Self>) {
        let context = (self.options.context + CONTEXT_STEP).min(FULL_CONTEXT);
        self.set_options(
            DiffOptions {
                context,
                ..self.options
            },
            cx,
        );
    }

    fn set_layout(&mut self, layout: DiffLayout, cx: &mut Context<Self>) {
        if layout == self.layout {
            return;
        }
        self.layout = layout;
        self.rebuild_rows();
        cx.emit(DiffEvent::LayoutChanged(layout));
        cx.notify();
    }

    /// Takes the keys again once a view shown on top of it closes.
    pub fn refocus(&mut self, cx: &mut Context<Self>) {
        self.focus_on_render = true;
        cx.notify();
    }

    /// Esc clears the selection first; with nothing selected it closes the diff.
    fn close(&mut self, _: &CloseDiff, _: &mut Window, cx: &mut Context<Self>) {
        if self.selection.is_empty() {
            cx.emit(DiffEvent::Close);
        } else {
            self.clear_selection(cx);
        }
    }

    fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.selection.clear();
        self.anchor = None;
        self.row_anchor = None;
        cx.notify();
    }

    fn click_line(&mut self, h: usize, l: usize, modifiers: Modifiers, cx: &mut Context<Self>) {
        let line = (h, l);
        self.cursor = self.row_of(line);
        self.row_anchor = None;
        if modifiers.shift
            && let Some(anchor) = self.anchor
        {
            self.selection = self.changed_lines_between(anchor, line);
        } else if modifiers.control {
            if !self.selection.remove(&line) {
                self.selection.insert(line);
            }
            self.anchor = Some(line);
        } else if self.selection.len() == 1 && self.selection.contains(&line) {
            // Clicking the only selected line again deselects it.
            self.selection.clear();
            self.anchor = None;
        } else {
            self.selection = LineSelection::from([line]);
            self.anchor = Some(line);
        }
        cx.notify();
    }

    /// All changed lines between two lines (inclusive), in display order.
    fn changed_lines_between(&self, a: (usize, usize), b: (usize, usize)) -> LineSelection {
        match (self.row_of(a), self.row_of(b)) {
            (Some(start), Some(end)) => self.changed_lines_in(start, end),
            _ => LineSelection::new(),
        }
    }

    /// The row showing `line`.
    fn row_of(&self, line: (usize, usize)) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| row_lines(*row).contains(&Some(line)))
    }

    /// The changed lines of the rows from `a` to `b` (inclusive, either way round).
    fn changed_lines_in(&self, a: usize, b: usize) -> LineSelection {
        let Content::Loaded(diff, _) = &self.content else {
            return LineSelection::new();
        };
        let end = a.max(b).min(self.rows.len().saturating_sub(1));
        let Some(rows) = self.rows.get(a.min(b)..=end) else {
            return LineSelection::new();
        };
        rows.iter()
            .flat_map(|row| row_lines(*row))
            .flatten()
            .filter(|&(h, l)| diff.hunks[h].lines[l].kind != LineKind::Context)
            .collect()
    }

    /// Ctrl+F: the find bar (#43).
    fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.find.open(window, cx, Self::on_find);
    }

    fn on_find(&mut self, event: &FindEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            FindEvent::Changed => self.refind(cx),
            FindEvent::Show => {
                let Some(found) = self.find.current(cx) else {
                    return;
                };
                let Content::Loaded(diff, decorations) = &self.content else {
                    return;
                };
                let Some((h, l)) = line_at(diff, found.row) else {
                    return;
                };
                let text = diff.hunks[h].lines[l].display_text(decorations.tab_width);
                if let Some(ix) = self
                    .rows
                    .iter()
                    .position(|row| row_lines(*row).contains(&Some((h, l))))
                {
                    self.put_cursor(ix, cx);
                }
                if !self.wrapped {
                    let width = code_scroll::char_width(window, cx.metrics().text.mono);
                    let at = |byte: usize| width * columns(&text[..byte]) as f32;
                    self.hscroll
                        .reveal(at(found.range.start), at(found.range.end));
                }
            }
            FindEvent::Closed => {
                self.focus.focus(window, cx);
                cx.notify();
            }
        }
    }

    /// Finds the bar's query in the diff's lines again: it changed, or the diff did.
    fn refind(&mut self, cx: &mut Context<Self>) {
        let (matches, from) = match (self.find.query(cx), &self.content) {
            (Some((query, case)), Content::Loaded(diff, decorations)) => {
                let texts: Vec<String> = diff
                    .hunks
                    .iter()
                    .flat_map(|hunk| &hunk.lines)
                    .map(|line| line.display_text(decorations.tab_width))
                    .collect();
                let matches =
                    find_matches(texts.iter().map(String::as_str).enumerate(), &query, case);
                // From the cursor's line on, else from the top.
                let from = self
                    .cursor
                    .and_then(|ix| self.rows.get(ix))
                    .and_then(|row| row_lines(*row).into_iter().flatten().next())
                    .map_or(0, |(h, l)| line_key(diff, h, l));
                (matches, from)
            }
            _ => (Vec::new(), 0),
        };
        self.find.set_matches(matches, from, cx);
        cx.notify();
    }

    /// Puts the cursor on row `ix` and scrolls by the least that shows it.
    fn put_cursor(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.cursor = Some(ix);
        code_keys::reveal(self.wrapped, &self.scroll, &self.list, ix);
        cx.notify();
    }

    /// The arrows, Home and End move the cursor through the rows (#28); the selection
    /// stays, and the next Shift+arrow starts a new range at the cursor.
    fn move_cursor(&mut self, to: Step, cx: &mut Context<Self>) {
        let Some(ix) = list_keys::step(self.rows.len(), self.cursor, to) else {
            return;
        };
        self.anchor = None;
        self.row_anchor = None;
        self.put_cursor(ix, cx);
    }

    /// Shift with the arrows, Home and End: the cursor moves and the selection becomes the
    /// changed lines from where the range started (a Shift-click's line too) to it.
    fn extend(&mut self, to: Step, cx: &mut Context<Self>) {
        let Some(ix) = list_keys::step(self.rows.len(), self.cursor, to) else {
            return;
        };
        let start = self
            .row_anchor
            .or_else(|| self.anchor.and_then(|line| self.row_of(line)))
            .or(self.cursor)
            .unwrap_or(ix);
        self.row_anchor = Some(start);
        self.selection = self.changed_lines_in(start, ix);
        self.put_cursor(ix, cx);
    }

    /// Page Down / Page Up: a view's worth of rows (less one).
    fn page(&mut self, down: bool, cx: &mut Context<Self>) {
        let height = code_keys::view_height(self.wrapped, &self.scroll, &self.list);
        let rows = code_keys::page_rows(height, cx.metrics().code_line);
        let Some(ix) = code_keys::page_target(self.rows.len(), self.cursor, down, rows) else {
            return;
        };
        self.anchor = None;
        self.row_anchor = None;
        self.put_cursor(ix, cx);
    }

    /// Alt+Down / Alt+Up: the cursor to the next or previous hunk's header, shown at the top.
    fn jump_hunk(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(ix) = hunk_row(&self.rows, self.cursor, forward) else {
            return;
        };
        self.cursor = Some(ix);
        self.anchor = None;
        self.row_anchor = None;
        code_scroll::scroll_to_top(self.wrapped, &self.scroll, &self.list, ix);
        cx.notify();
    }

    /// Left / Right: the lines scrolled sideways by four characters.
    fn scroll_sideways(&mut self, left: bool, window: &mut Window, cx: &mut Context<Self>) {
        let step = code_keys::sideways_step(window, cx);
        if self.hscroll.scroll_by(if left { step } else { -step }) {
            cx.notify();
        }
    }

    /// Builds the patch for `selection` and emits it (asking first for discards).
    fn apply(
        &mut self,
        action: PatchAction,
        selection: LineSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Content::Loaded(diff, _) = &self.content else {
            return;
        };
        let target = match action {
            PatchAction::Stage => PatchTarget::Forward,
            PatchAction::Unstage | PatchAction::Discard => PatchTarget::Reverse,
        };
        let path = diff.path.clone();
        let whole_file = action != PatchAction::Stage
            && is_creation(diff)
            && selection == select_hunks(diff, 0..diff.hunks.len());
        let event = if whole_file {
            DiffEvent::WholeFile { action, path }
        } else {
            let Some(patch) = build_patch(diff, &selection, target) else {
                return;
            };
            DiffEvent::Apply {
                action,
                patch,
                path,
            }
        };
        if action != PatchAction::Discard {
            cx.emit(event);
            return;
        }
        let message = tn!("diff.discard_lines", count = selection.len()).into_owned();
        let detail = t!("diff.discard_lines_detail").into_owned();
        let answer = window.prompt(
            PromptLevel::Warning,
            &message,
            Some(&detail),
            &[
                PromptButton::ok(t!("staging.discard").into_owned()),
                PromptButton::cancel(t!("staging.cancel").into_owned()),
            ],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await == Ok(0) {
                this.update(cx, |_, cx| cx.emit(event)).ok();
            }
        })
        .detach();
    }

    fn apply_hunk(
        &mut self,
        action: PatchAction,
        h: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Content::Loaded(diff, _) = &self.content else {
            return;
        };
        let selection = select_hunks(diff, [h]);
        self.apply(action, selection, window, cx);
    }

    fn apply_selection(
        &mut self,
        action: PatchAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let selection = self.selection.clone();
        self.apply(action, selection, window, cx);
    }

    /// The diff's buttons as palette commands: each hunk's links (the file and the hunk as
    /// the detail), the selected lines' actions, the layout and options, and Close.
    pub(crate) fn palette_commands(&self, this: &WeakEntity<Self>) -> Vec<PaletteCommand> {
        let group: SharedString = t!("palette.group.diff").into_owned().into();
        let mut commands = Vec::new();
        let actions = self.line_actions();
        if let Content::Loaded(diff, _) = &self.content {
            let path = diff.path.to_string();
            for (h, hunk) in diff.hunks.iter().enumerate() {
                let detail = format!("{path} {}", hunk.header());
                for (action, label) in hunk_actions(actions) {
                    let command = view_command(this, &group, label, move |this, window, cx| {
                        this.apply_hunk(action, h, window, cx)
                    });
                    commands.push(command.detail(detail.clone()));
                }
            }
            if !self.selection.is_empty() {
                let detail = tn!("diff.lines_selected", count = self.selection.len()).into_owned();
                for (action, label) in selection_actions(actions) {
                    let command = view_command(this, &group, label, move |this, window, cx| {
                        this.apply_selection(action, window, cx)
                    });
                    commands.push(command.detail(detail.clone()));
                }
                let label = t!("palette.diff_clear_selection");
                commands.push(view_command(this, &group, label, |this, _, cx| {
                    this.clear_selection(cx)
                }));
            }
        }
        let (layout, label) = match self.layout {
            DiffLayout::Unified => (DiffLayout::Split, t!("palette.diff_split")),
            DiffLayout::Split => (DiffLayout::Unified, t!("palette.diff_unified")),
        };
        commands.push(view_command(this, &group, label, move |this, _, cx| {
            this.set_layout(layout, cx)
        }));
        // Untracked files are shown whole; options don't apply to them.
        if self.source != DiffSource::Untracked {
            let ignore = !self.options.ignore_whitespace;
            let label = if ignore {
                t!("palette.diff_ignore_whitespace")
            } else {
                t!("palette.diff_show_whitespace")
            };
            commands.push(view_command(this, &group, label, move |this, _, cx| {
                let options = DiffOptions {
                    ignore_whitespace: ignore,
                    ..this.options
                };
                this.set_options(options, cx)
            }));
            let full = self.options.context < FULL_CONTEXT;
            let label = if full {
                t!("palette.diff_full_file")
            } else {
                t!("palette.diff_hunks_only")
            };
            commands.push(view_command(this, &group, label, move |this, _, cx| {
                let context = if full {
                    FULL_CONTEXT
                } else {
                    DiffOptions::default().context
                };
                let options = DiffOptions {
                    context,
                    ..this.options
                };
                this.set_options(options, cx)
            }));
        }
        let comment = self
            .line_to_comment()
            .and_then(|(h, l)| match &self.content {
                Content::Loaded(diff, _) => {
                    Some((h, l, diff.path.to_string(), &diff.hunks[h].lines[l]))
                }
                _ => None,
            });
        if let Some((h, l, path, line)) = comment
            && let Some((_, number)) = comment_place(line)
        {
            let label = t!("palette.diff_comment_line", line = number);
            let command = view_command(this, &group, label, move |this, _, cx| {
                this.comment_on((h, l), cx)
            });
            commands.push(command.detail(path).key(CommentOnLine, Some(CONTEXT)));
        }
        if self.missing_objects {
            let label = t!("diff.fetch_missing");
            commands.push(view_command(this, &group, label, |_, _, cx| {
                cx.emit(DiffEvent::FetchMissing)
            }));
        }
        let label = t!("palette.diff_close");
        commands.push(view_command(this, &group, label, |_, _, cx| {
            cx.emit(DiffEvent::Close)
        }));
        commands
    }

    fn render_rows(
        &mut self,
        range: std::ops::Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        range.map(|ix| self.render_row(ix, false, cx)).collect()
    }

    /// Row `ix`, with the keys' cursor outlined while the diff has the focus.
    fn render_row(&self, ix: usize, wrap: bool, cx: &mut Context<Self>) -> AnyElement {
        let row = self.render_row_content(ix, wrap, cx);
        // How the headless UI tests find the row; test builds only, so the app's layout
        // doesn't get the wrapper.
        #[cfg(test)]
        let row = div()
            .w_full()
            .debug_selector(move || format!("diff-row-{ix}"))
            .child(row)
            .into_any_element();
        if self.focused && self.cursor == Some(ix) {
            code_keys::cursor_ring(row, cx)
        } else {
            row
        }
    }

    /// Row `ix`; `wrap`: its lines as high as their wrapped text.
    fn render_row_content(&self, ix: usize, wrap: bool, cx: &mut Context<Self>) -> AnyElement {
        let actions = self.line_actions();
        let (Content::Loaded(diff, _), Some(&row)) = (&self.content, self.rows.get(ix)) else {
            return div().into_any_element();
        };
        let palette = RowStyle {
            wrap,
            number_width: self.number_width,
            ..RowStyle::new(cx)
        };
        match row {
            Row::Gap { hidden } => self.render_gap(ix, hidden, palette, cx),
            Row::Hunk(h) => self.render_hunk_header(h, actions, palette, cx),
            Row::Comment(c) => self.render_comment(c, palette, cx),
            Row::Line(h, l) => {
                let line = &diff.hunks[h].lines[l];
                let numbers = [line.old_line, line.new_line];
                self.render_line(
                    ("diff-line", ix).into(),
                    (h, l),
                    &numbers,
                    actions,
                    palette,
                    cx,
                )
                .w_full()
                .into_any_element()
            }
            Row::Split(h, old, new) => {
                let mut side = |line: Option<usize>, old_side: bool| match line {
                    Some(l) => {
                        let line = &diff.hunks[h].lines[l];
                        let number = if old_side {
                            line.old_line
                        } else {
                            line.new_line
                        };
                        let id = if old_side {
                            "diff-line-old"
                        } else {
                            "diff-line-new"
                        };
                        self.render_line((id, ix).into(), (h, l), &[number], actions, palette, cx)
                            .flex_1()
                            .into_any_element()
                    }
                    None => div()
                        .flex_1()
                        .when(!wrap, |this| this.h(palette.line_height))
                        .bg(palette.blank)
                        .into_any_element(),
                };
                let (left, right) = (side(old, true), side(new, false));
                // Wrapped, the taller side sets the row's height and the other stretches.
                h_flex()
                    .w_full()
                    .when(wrap, |this| this.items_stretch())
                    .child(left)
                    .child(
                        div()
                            .w(px(1.))
                            .flex_shrink_0()
                            .when(!wrap, |this| this.h(palette.line_height))
                            .bg(palette.border),
                    )
                    .child(right)
                    .into_any_element()
            }
        }
    }

    /// A comment under its line: the first line of the text; a click opens it.
    fn render_comment(
        &self,
        index: usize,
        palette: RowStyle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(comment) = self.comments.as_ref().and_then(|all| all.get(index)) else {
            return div().h(palette.line_height).into_any_element();
        };
        let id = comment.id;
        // Its first line scrolls sideways with the code (or wraps).
        code_scroll::code_row(palette.wrap, palette.line_height)
            .id(("diff-comment", index))
            .w_full()
            .pl(palette.number_width)
            .pr_2()
            .gap_2()
            .bg(palette.hunk_bg)
            .cursor_pointer()
            .font_family(gitweft_ui::current::ui_font())
            .text_color(if comment.resolved {
                palette.muted
            } else {
                palette.foreground
            })
            .child(
                div()
                    .w(palette.marker_width)
                    .flex_shrink_0()
                    .bg(palette.marker),
            )
            .child(code_scroll::code_text(
                first_line(&comment.body),
                palette.wrap,
                self.hscroll.offset(),
            ))
            .when(comment.resolved, |this| {
                this.child(
                    div()
                        .flex_shrink_0()
                        .text_color(palette.muted)
                        .child(t!("review.resolved").into_owned()),
                )
            })
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(DiffEvent::OpenComment(id))))
            .into_any_element()
    }

    fn render_gap(
        &self,
        ix: usize,
        hidden: u32,
        palette: RowStyle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        h_flex()
            .id(("diff-gap", ix))
            .h(palette.line_height)
            .w_full()
            .px_2()
            .cursor_pointer()
            // The label is the tab stop; the whole row still takes a click.
            .child(
                gitweft_ui::widgets::LinkButton::new(
                    ("diff-gap-link", ix),
                    tn!("diff.hidden_lines", count = hidden).into_owned(),
                    palette.muted,
                )
                .hover_color(palette.foreground)
                .on_click(cx.listener(|this, _, _, cx| this.more_context(cx))),
            )
            .on_click(cx.listener(|this, _, _, cx| this.more_context(cx)))
            .into_any_element()
    }

    fn render_hunk_header(
        &self,
        h: usize,
        actions: LineActions,
        palette: RowStyle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Content::Loaded(diff, _) = &self.content else {
            return div().into_any_element();
        };
        let link = |id: &'static str, label: String, action: PatchAction| {
            div().px_1().child(
                gitweft_ui::widgets::LinkButton::new((id, h), label, palette.muted)
                    .hover_color(palette.foreground)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.apply_hunk(action, h, window, cx)
                    })),
            )
        };
        let links: Vec<_> = hunk_actions(actions)
            .into_iter()
            .map(|(action, label)| link(hunk_link_id(action), label, action))
            .collect();
        // The header scrolls sideways with the code (or wraps); its links stay at the right.
        code_scroll::code_row(palette.wrap, palette.line_height)
            .w_full()
            .px_2()
            .gap_2()
            .bg(palette.hunk_bg)
            .child(
                code_scroll::code_text(diff.hunks[h].header(), palette.wrap, self.hscroll.offset())
                    .text_color(palette.hunk_fg),
            )
            .children(links)
            .into_any_element()
    }

    /// One diff line: selection marker, line number(s), sign and highlighted text.
    fn render_line(
        &self,
        id: ElementId,
        at: (usize, usize),
        numbers: &[Option<u32>],
        actions: LineActions,
        palette: RowStyle,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let (h, l) = at;
        let wrap = palette.wrap;
        let Content::Loaded(diff, decorations) = &self.content else {
            return h_flex().id(id);
        };
        let line = &diff.hunks[h].lines[l];
        // A diff since the last review takes comments on its new side only.
        let reviewing = self.takes_comment(line);
        let selected = self.selection.contains(&(h, l)) || self.comment_line == Some((h, l));
        // Without the moved tint (the Diff setting) a moved line is a plain added or removed one.
        let moved = decorations
            .moved
            .get(h)
            .and_then(|lines| lines.get(l).copied())
            .flatten()
            .filter(|_| palette.tint_moved);
        // A move whose other end is in another file of the same diff says so (#69).
        let across = moved
            .and_then(|id| decorations.moved_across.get(id).cloned().flatten())
            .map(|path| {
                let file = path.to_str_lossy().into_owned();
                match line.kind {
                    LineKind::Removed => t!("diff.moved_to", file = file),
                    _ => t!("diff.moved_from", file = file),
                }
                .into_owned()
            });
        let (sign, bg, strong) = match (line.kind, moved) {
            (LineKind::Added, Some(id)) => {
                ("+", Some(palette.moved_new[id % 2]), palette.added_strong)
            }
            (LineKind::Removed, Some(id)) => {
                ("-", Some(palette.moved_old[id % 2]), palette.removed_strong)
            }
            (LineKind::Added, None) => ("+", Some(palette.added_bg), palette.added_strong),
            (LineKind::Removed, None) => ("-", Some(palette.removed_bg), palette.removed_strong),
            (LineKind::Context, _) => (" ", None, palette.added_strong),
        };
        let bg = if selected { Some(strong) } else { bg };
        // On a selected line the line tint is already "strong"; changed words need an
        // even stronger one to stay visible.
        let word_bg = if selected {
            Hsla { a: 0.6, ..strong }
        } else {
            strong
        };
        let selectable = actions != LineActions::None && line.kind != LineKind::Context;

        let syntax = decorations.syntax[h][l].iter().map(|(range, color)| {
            let style = HighlightStyle {
                color: Some(rgb(color).into()),
                ..HighlightStyle::default()
            };
            (range.clone(), style)
        });
        let words = decorations.words[h][l].iter().map(|range| {
            let style = HighlightStyle {
                background_color: Some(word_bg),
                ..HighlightStyle::default()
            };
            (range.clone(), style)
        });
        let found = self
            .find
            .highlights(line_key(diff, h, l), palette.find_bg, cx);
        let highlights: Vec<_> =
            combine_highlights(combine_highlights(syntax, words), found).collect();
        let text = line.display_text(decorations.tab_width);
        let styled = StyledText::new(text).with_highlights(highlights);
        let note = line.no_newline.then(|| {
            div()
                .pl_2()
                .text_color(palette.muted)
                .child(t!("diff.no_newline").into_owned())
        });
        // Wrapped, the note goes under the text; otherwise after it.
        let text = if wrap {
            div().child(styled).children(note).into_any_element()
        } else {
            h_flex().child(styled).children(note).into_any_element()
        };

        code_scroll::code_row(wrap, palette.line_height)
            .id(id)
            .when_some(bg, |this, bg| this.bg(bg))
            .when_some(across, |this, tip| {
                this.tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
            })
            .when(selectable, |this| {
                this.cursor_pointer().on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                        this.click_line(h, l, event.modifiers, cx)
                    }),
                )
            })
            // In a review, any line can be picked to comment on.
            .when(reviewing, |this| {
                this.cursor_pointer().on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                        this.pick_comment_line(h, l, cx)
                    }),
                )
            })
            // Selection marker in the gutter.
            .child(
                div()
                    .w(palette.marker_width)
                    // Wrapped rows stretch it instead: a set height would stop that.
                    .when(!wrap, |this| this.h_full())
                    .flex_shrink_0()
                    .when(selected, |this| this.bg(palette.marker)),
            )
            .children(numbers.iter().map(|n| {
                div()
                    .w(palette.number_width)
                    .flex_shrink_0()
                    .pr_2()
                    .text_right()
                    .text_color(palette.muted)
                    .child(n.map(|n| n.to_string()).unwrap_or_default())
            }))
            .child(
                div()
                    .w(palette.sign_width)
                    .flex_shrink_0()
                    .text_color(palette.muted)
                    .child(sign),
            )
            .child(code_scroll::code_text(text, wrap, self.hscroll.offset()))
    }

    /// The bar with actions for the selected lines.
    fn render_selection_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if let Some((h, l)) = self.comment_line {
            return self.render_comment_bar(h, l, cx);
        }
        if self.selection.is_empty() {
            return None;
        }
        let actions = self.line_actions();
        let (p, m) = (*cx.palette(), *cx.metrics());
        let button = |id: &'static str, label: String, action: PatchAction| {
            Button::new(id).xsmall().label(label).on_click(
                cx.listener(move |this, _, window, cx| this.apply_selection(action, window, cx)),
            )
        };
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
                    div().flex_1().child(
                        tn!("diff.lines_selected", count = self.selection.len()).into_owned(),
                    ),
                )
                .when(actions == LineActions::StageOrDiscard, |this| {
                    this.child(button(
                        "stage-lines",
                        t!("diff.stage_lines").into_owned(),
                        PatchAction::Stage,
                    ))
                    .child(button(
                        "discard-lines",
                        t!("diff.discard_lines_button").into_owned(),
                        PatchAction::Discard,
                    ))
                })
                .when(actions == LineActions::Unstage, |this| {
                    this.child(button(
                        "unstage-lines",
                        t!("diff.unstage_lines").into_owned(),
                        PatchAction::Unstage,
                    ))
                })
                .child(
                    Button::new("clear-selection")
                        .xsmall()
                        .ghost()
                        .label(t!("diff.clear_selection").into_owned())
                        .on_click(cx.listener(|this, _, _, cx| this.clear_selection(cx))),
                )
                .into_any_element(),
        )
    }

    /// Review diffs: the bar for the picked line, with "Comment…".
    fn render_comment_bar(&self, h: usize, l: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let Content::Loaded(diff, _) = &self.content else {
            return None;
        };
        let (_, line) = comment_place(diff.hunks.get(h)?.lines.get(l)?)?;
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
                        .child(t!("review.picked_line", line = line).into_owned()),
                )
                .child(
                    Button::new("comment-line")
                        .debug_selector(|| "comment-line".into())
                        .xsmall()
                        .primary()
                        .label(t!("review.comment_button").into_owned())
                        .on_click(cx.listener(|this, _, _, cx| this.comment_on_picked(cx))),
                )
                .child(
                    Button::new("clear-comment-line")
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

    /// Layout switch and diff options.
    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let layout_button = |id: &'static str, label: String, layout: DiffLayout| {
            Button::new(id)
                .xsmall()
                .ghost()
                .selected(self.layout == layout)
                .label(label)
                .on_click(cx.listener(move |this, _, _, cx| this.set_layout(layout, cx)))
        };
        let unified = layout_button(
            "layout-unified",
            t!("diff.unified").into_owned(),
            DiffLayout::Unified,
        );
        let split = layout_button(
            "layout-split",
            t!("diff.split").into_owned(),
            DiffLayout::Split,
        );
        // Untracked files are shown whole; options don't apply to them.
        let options = (self.source != DiffSource::Untracked).then(|| {
            h_flex()
                .gap_3()
                .child(
                    Checkbox::new("ignore-whitespace")
                        .small()
                        .label(t!("diff.ignore_whitespace").into_owned())
                        .checked(self.options.ignore_whitespace)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            let options = DiffOptions {
                                ignore_whitespace: *checked,
                                ..this.options
                            };
                            this.set_options(options, cx);
                        })),
                )
                .child(
                    Checkbox::new("full-file")
                        .small()
                        .label(t!("diff.full_file").into_owned())
                        .checked(self.options.context >= FULL_CONTEXT)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            let context = if *checked {
                                FULL_CONTEXT
                            } else {
                                DiffOptions::default().context
                            };
                            this.set_options(
                                DiffOptions {
                                    context,
                                    ..this.options
                                },
                                cx,
                            );
                        })),
                )
        });
        h_flex()
            .gap_3()
            .children(options)
            .child(h_flex().gap_0p5().child(unified).child(split))
            .child(code_scroll::wrap_button(code_scroll::wrapping(cx)))
    }
}

impl Render for DiffView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.focus_on_render) {
            self.focus.focus(window, cx);
        }
        self.focused = self.focus.contains_focused(window, cx);
        let (p, m) = (*cx.palette(), *cx.metrics());
        let (muted, border) = (p.fg3, p.line);

        let header = centre_header(self.title.clone(), self.subtitle.clone(), &p, &m)
            .child(self.render_toolbar(cx))
            .child(
                Button::new("close-diff")
                    .small()
                    .ghost()
                    .label(t!("diff.close").into_owned())
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DiffEvent::Close))),
            );

        let placeholder = |text: String| {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_color(muted)
                .child(text)
                .into_any_element()
        };
        let wrap = code_scroll::wrapping(cx);
        if wrap != self.wrapped {
            let rows = self.rows.len();
            code_scroll::switch_mode(wrap, &self.scroll, &self.list, rows, m.code_line);
            self.wrapped = wrap;
        }
        // Wide enough for the largest number, with the column's `pr_2` (#42).
        self.number_width = code_scroll::number_width(
            window,
            m.text.mono,
            self.largest_number,
            code_scroll::pr_2(&m),
            px(48. * m.zoom),
        );
        if !wrap {
            // The text's room: the list less the marker, numbers and sign (per side).
            let number = self.number_width;
            let viewport = self.hscroll.viewport();
            let gutter = m.dp(MARKER_WIDTH + SIGN_WIDTH);
            let visible = match self.layout {
                DiffLayout::Unified => viewport - (gutter + number * 2.),
                DiffLayout::Split => (viewport - px(1.)) / 2. - (gutter + number),
            };
            let content = code_scroll::text_width(window, m.text.mono, self.longest);
            self.hscroll.update(content, visible);
        }
        let whitespace_note = self.options.ignore_whitespace
            && matches!(self.source, DiffSource::Unstaged | DiffSource::Staged);
        let body = match &self.content {
            Content::Loading => placeholder(t!("repo.loading").into_owned()),
            Content::Failed(message) if self.missing_objects => v_flex()
                .flex_1()
                .gap_3()
                .items_center()
                .justify_center()
                .text_color(muted)
                .child(message.clone())
                .child(
                    Button::new("fetch-missing")
                        .debug_selector(|| "fetch-missing".into())
                        .small()
                        .primary()
                        .label(t!("diff.fetch_missing").into_owned())
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(DiffEvent::FetchMissing))),
                )
                .into_any_element(),
            Content::Failed(message) => placeholder(message.clone()),
            // Pictures: a binary image's, or an LFS image's from the local store (#36); an LFS
            // file without them shows its pointers.
            Content::Loaded(..) if self.images.is_some() => self.render_images(cx),
            Content::Loaded(..) if self.lfs.is_some() => self.render_lfs(cx),
            Content::Loaded(diff, _) if diff.binary => placeholder(t!("diff.binary").into_owned()),
            Content::Loaded(diff, _) if diff.hunks.is_empty() && diff.truncated => {
                placeholder(t!("diff.too_large").into_owned())
            }
            // A chmod alone: before, "No changes" (#16).
            Content::Loaded(diff, _) if diff.hunks.is_empty() && diff.mode_change.is_some() => {
                let (old, new) = diff.mode_change.clone().unwrap_or_default();
                placeholder(t!("diff.mode_changed", old = old, new = new).into_owned())
            }
            Content::Loaded(diff, _) if diff.hunks.is_empty() && self.options.ignore_whitespace => {
                placeholder(t!("diff.whitespace_only").into_owned())
            }
            Content::Loaded(diff, _) if diff.hunks.is_empty() => {
                placeholder(t!("diff.no_changes").into_owned())
            }
            Content::Loaded(diff, _) => {
                let truncated = diff.truncated;
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .when(whitespace_note, |this| {
                        this.child(
                            div()
                                .px_3()
                                .py_1()
                                .text_size(m.text.xs)
                                .text_color(muted)
                                .border_b_1()
                                .border_color(border)
                                .child(t!("diff.whitespace_no_staging").into_owned()),
                        )
                    })
                    // A chmod along with the edits (#16).
                    .when_some(diff.mode_change.clone(), |this, (old, new)| {
                        this.child(
                            div()
                                .px_3()
                                .py_1()
                                .text_size(m.text.xs)
                                .text_color(muted)
                                .border_b_1()
                                .border_color(border)
                                .child(t!("diff.mode_note", old = old, new = new).into_owned()),
                        )
                    })
                    .children(self.render_selection_bar(cx))
                    .child({
                        let lines = if wrap {
                            list(
                                self.list.clone(),
                                cx.processor(|this, ix, _, cx| this.render_row(ix, true, cx)),
                            )
                            .size_full()
                            .into_any_element()
                        } else {
                            uniform_list(
                                "diff-lines",
                                self.rows.len(),
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
                    })
                    .when(truncated, |this| {
                        this.child(
                            div()
                                .px_3()
                                .py_1()
                                .text_size(m.text.xs)
                                .text_color(muted)
                                .border_t_1()
                                .border_color(border)
                                .child(t!("diff.truncated").into_owned()),
                        )
                    })
                    .into_any_element()
            }
        };

        v_flex()
            .size_full()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::close))
            .on_action(cx.listener(Self::comment_on_cursor))
            .on_action(cx.listener(|this, _: &OpenFind, window, cx| this.open_find(window, cx)))
            .on_action(cx.listener(|this, _: &NextRow, _, cx| this.move_cursor(Step::Next, cx)))
            .on_action(
                cx.listener(|this, _: &PreviousRow, _, cx| this.move_cursor(Step::Previous, cx)),
            )
            .on_action(cx.listener(|this, _: &FirstRow, _, cx| this.move_cursor(Step::First, cx)))
            .on_action(cx.listener(|this, _: &LastRow, _, cx| this.move_cursor(Step::Last, cx)))
            .on_action(cx.listener(|this, _: &ExtendToNextRow, _, cx| this.extend(Step::Next, cx)))
            .on_action(
                cx.listener(|this, _: &ExtendToPreviousRow, _, cx| this.extend(Step::Previous, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ExtendToFirstRow, _, cx| this.extend(Step::First, cx)),
            )
            .on_action(cx.listener(|this, _: &ExtendToLastRow, _, cx| this.extend(Step::Last, cx)))
            .on_action(cx.listener(|this, _: &NextPage, _, cx| this.page(true, cx)))
            .on_action(cx.listener(|this, _: &PreviousPage, _, cx| this.page(false, cx)))
            .on_action(cx.listener(|this, _: &ScrollLeft, window, cx| {
                this.scroll_sideways(true, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ScrollRight, window, cx| {
                this.scroll_sideways(false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &NextHunk, _, cx| this.jump_hunk(true, cx)))
            .on_action(cx.listener(|this, _: &PreviousHunk, _, cx| this.jump_hunk(false, cx)))
            .child(header)
            .children(self.find.element(cx))
            .child(body)
    }
}

/// Image versions larger than this are not shown as pictures.
pub(crate) const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

/// The format of an image file, by its extension. SVG is left out: git diffs it as text.
pub(crate) fn image_format(path: &[u8]) -> Option<ImageFormat> {
    let name = path.rsplit(|b| *b == b'/').next().unwrap_or(path);
    let dot = name.iter().rposition(|b| *b == b'.')?;
    let extension = std::str::from_utf8(&name[dot + 1..])
        .ok()?
        .to_ascii_lowercase();
    Some(match extension.as_str() {
        "png" => ImageFormat::Png,
        "jpg" | "jpeg" => ImageFormat::Jpeg,
        "gif" => ImageFormat::Gif,
        "webp" => ImageFormat::Webp,
        "bmp" => ImageFormat::Bmp,
        "tif" | "tiff" => ImageFormat::Tiff,
        "ico" => ImageFormat::Ico,
        _ => return None,
    })
}

/// A picture of each version of an image (decoded when first drawn), with its size.
struct ImagePair {
    old: Option<(Arc<Image>, usize)>,
    new: Option<(Arc<Image>, usize)>,
}

fn image_pair(sides: ImageSides) -> Option<ImagePair> {
    let format = image_format(&sides.path)?;
    let picture = |bytes: Vec<u8>| {
        let len = bytes.len();
        (Arc::new(Image::from_bytes(format, bytes)), len)
    };
    Some(ImagePair {
        old: sides.old.map(&picture),
        new: sides.new.map(&picture),
    })
}

pub(crate) fn human_size(bytes: usize) -> String {
    const KB: f64 = 1024.;
    let n = bytes as f64;
    if n < KB {
        t!("diff.size_bytes", n = bytes)
    } else if n < KB * KB {
        t!("diff.size_kb", n = format!("{:.1}", n / KB))
    } else {
        t!("diff.size_mb", n = format!("{:.1}", n / KB / KB))
    }
    .into_owned()
}

impl DiffView {
    /// Both versions of an image side by side, each scaled to fit its pane.
    fn render_images(&self, cx: &App) -> AnyElement {
        let Some(images) = &self.images else {
            return div().into_any_element();
        };
        let (p, m) = (*cx.palette(), *cx.metrics());
        let (muted, backdrop) = (p.fg3, p.panel2);
        let side = |label: String, image: &Option<(Arc<Image>, usize)>| {
            let caption = match image {
                Some((_, len)) => t!(
                    "common.middle_dot",
                    first = label,
                    second = human_size(*len)
                )
                .into_owned(),
                None => label,
            };
            let body = match image {
                Some((picture, _)) => div()
                    .flex_1()
                    .min_h_0()
                    .rounded_md()
                    .bg(backdrop)
                    .p_2()
                    .child(
                        img(picture.clone())
                            .size_full()
                            // Large images shrink to fit; small ones keep their size.
                            .object_fit(ObjectFit::ScaleDown),
                    )
                    .into_any_element(),
                None => v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .rounded_md()
                    .bg(backdrop)
                    .text_color(muted)
                    .child(t!("diff.image_none").into_owned())
                    .into_any_element(),
            };
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_2()
                .p_3()
                .child(div().text_size(m.text.xs).text_color(muted).child(caption))
                .child(body)
        };
        h_flex()
            .flex_1()
            .min_h_0()
            .items_stretch()
            .child(side(t!("diff.image_before").into_owned(), &images.old))
            .child(side(t!("diff.image_after").into_owned(), &images.new))
            .into_any_element()
    }

    /// A file stored with Git LFS: git diffs the pointer files, so show what each side
    /// points at instead of their three lines.
    fn render_lfs(&self, cx: &App) -> AnyElement {
        let Some((old, new)) = &self.lfs else {
            return div().into_any_element();
        };
        let (p, m) = (*cx.palette(), *cx.metrics());
        let (muted, backdrop) = (p.fg3, p.panel2);
        let side = |label: String, pointer: &Option<LfsPointer>| {
            let body = match pointer {
                Some(pointer) => {
                    let size = human_size(usize::try_from(pointer.size).unwrap_or(usize::MAX));
                    v_flex()
                        .gap_1()
                        .child(t!("diff.lfs_object", size = size).into_owned())
                        .child(
                            div()
                                .text_size(m.text.xs)
                                .text_color(muted)
                                .font_family(mono_font())
                                .child(
                                    t!("diff.lfs_oid", oid = pointer.oid.to_string()).into_owned(),
                                ),
                        )
                        .into_any_element()
                }
                None => div()
                    .text_color(muted)
                    .child(t!("diff.image_none").into_owned())
                    .into_any_element(),
            };
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_2()
                .p_3()
                .child(div().text_size(m.text.xs).text_color(muted).child(label))
                .child(div().rounded_md().bg(backdrop).p_3().child(body))
        };
        v_flex()
            .flex_1()
            .min_h_0()
            .child(
                h_flex()
                    .items_start()
                    .child(side(t!("diff.image_before").into_owned(), old))
                    .child(side(t!("diff.image_after").into_owned(), new)),
            )
            .child(
                div()
                    .px_3()
                    .text_size(m.text.xs)
                    .text_color(muted)
                    .child(t!("diff.lfs_note").into_owned()),
            )
            .into_any_element()
    }
}

pub(crate) fn rgb(color: &gitweft_syntax::Rgb) -> Rgba {
    Rgba {
        r: f32::from(color.0) / 255.,
        g: f32::from(color.1) / 255.,
        b: f32::from(color.2) / 255.,
        a: 1.,
    }
}

#[cfg(test)]
mod tests {
    use super::{Row, hidden_before, row_lines};
    use gitweft_git::read::{FileDiff, Hunk};

    fn hunk(old_start: u32, old_len: u32) -> Hunk {
        Hunk {
            old_start,
            old_len,
            new_start: old_start,
            new_len: old_len,
            section: String::new(),
            lines: Vec::new(),
        }
    }

    #[test]
    fn hunks_and_lines_offer_what_the_diff_allows() {
        use super::{LineActions, hunk_actions, hunk_link_id, selection_actions};
        let ids = |list: Vec<(super::PatchAction, String)>| -> Vec<&'static str> {
            list.into_iter()
                .map(|(action, _)| hunk_link_id(action))
                .collect()
        };
        assert_eq!(
            ids(hunk_actions(LineActions::StageOrDiscard)),
            ["stage-hunk", "discard-hunk"]
        );
        assert_eq!(ids(hunk_actions(LineActions::Unstage)), ["unstage-hunk"]);
        assert!(hunk_actions(LineActions::None).is_empty());
        assert_eq!(
            ids(selection_actions(LineActions::StageOrDiscard)),
            ["stage-hunk", "discard-hunk"]
        );
        assert_eq!(
            ids(selection_actions(LineActions::Unstage)),
            ["unstage-hunk"]
        );
        assert!(selection_actions(LineActions::None).is_empty());
    }

    #[test]
    fn image_formats_by_extension() {
        use super::image_format;
        assert!(image_format(b"assets/Logo.PNG").is_some());
        assert!(image_format(b"photo.jpeg").is_some());
        // SVG is text to git; a folder named like an image isn't one.
        assert!(image_format(b"icon.svg").is_none());
        assert!(image_format(b"dir.png/readme").is_none());
        assert!(image_format(b"Makefile").is_none());
    }

    #[test]
    fn alt_arrows_go_from_hunk_to_hunk() {
        use super::{Row, hunk_row};
        let rows = [
            Row::Gap { hidden: 3 },
            Row::Hunk(0),
            Row::Line(0, 0),
            Row::Line(0, 1),
            Row::Hunk(1),
            Row::Line(1, 0),
        ];
        assert_eq!(hunk_row(&rows, None, true), Some(1));
        assert_eq!(hunk_row(&rows, None, false), Some(4));
        assert_eq!(hunk_row(&rows, Some(2), true), Some(4));
        assert_eq!(hunk_row(&rows, Some(5), false), Some(4));
        // From a header, the next one, not itself.
        assert_eq!(hunk_row(&rows, Some(4), false), Some(1));
        assert_eq!(hunk_row(&rows, Some(4), true), None);
        assert_eq!(hunk_row(&rows, Some(1), false), None);
    }

    #[test]
    fn hidden_lines_before_each_hunk() {
        let diff = FileDiff {
            hunks: vec![hunk(1, 7), hunk(12, 6), hunk(40, 3)],
            ..FileDiff::default()
        };
        // Lines 1–7 shown, 8–11 hidden, 12–17 shown, 18–39 hidden.
        assert_eq!(hidden_before(&diff), [0, 4, 22]);
        // A new file's hunk starts at old line 0: nothing hidden.
        let new_file = FileDiff {
            hunks: vec![hunk(0, 0)],
            ..FileDiff::default()
        };
        assert_eq!(hidden_before(&new_file), [0]);
    }

    #[test]
    fn split_rows_expose_both_sides_but_context_once() {
        assert_eq!(
            row_lines(Row::Split(2, Some(4), Some(5))),
            [Some((2, 4)), Some((2, 5))]
        );
        assert_eq!(
            row_lines(Row::Split(2, Some(3), Some(3))),
            [Some((2, 3)), None]
        );
        assert_eq!(
            row_lines(Row::Split(0, None, Some(1))),
            [None, Some((0, 1))]
        );
        assert_eq!(row_lines(Row::Hunk(0)), [None, None]);
    }
}
