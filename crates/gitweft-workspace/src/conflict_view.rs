//! The conflict tool (centre panel): a conflicted file's conflicts one at a time, "ours"
//! next to "theirs" with a few clean lines around them, and the whole resolved file below
//! in an editable box. Saving writes the file; "Save & mark resolved" also stages it, and
//! is only offered once no conflict markers are left. The header above it (which
//! operation, which branches) is the workspace's, which knows the operation.

use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

use gitweft_diff::conflict::{
    Conflict, Origin, Region, Resolution, conflict_count, resolve, resolve_lines,
};
use gitweft_git::conflict_files::{ConflictSides, Side, StageInfo};
use gitweft_git::display::{columns, expand_tabs};
use gitweft_git::gix::bstr::BString;
use gitweft_syntax::{Spans, highlight_lines, syntax_for_path};
use gitweft_ui::current::mono_font;
use gitweft_ui::truncated::truncated;
use gitweft_ui::{AppIcon, LookExt, Metrics, Palette};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::{Disableable, Icon, Sizable, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::code_keys::{self, NextHunk, PreviousHunk};
use crate::code_scroll::{self, HScroll};
use crate::find::{FindEvent, FindSlot, OpenFind, find_matches};
use crate::list_keys::{self, FirstRow, LastRow, NextRow, OpenRow, PreviousRow, Step, ToggleRow};
use crate::palette::{PaletteCommand, view_command};

/// The preview beside the text box follows the typing this long after it stops.
const TYPING_DELAY: Duration = Duration::from_millis(150);
/// Clean lines shown above and below a conflict in the two panes.
const CONTEXT_LINES: usize = 3;
/// The line number column, in design pixels.
const NUMBER_WIDTH: f32 = 44.;
/// Files longer than this are shown without syntax colors.
const MAX_HIGHLIGHT_LINES: usize = 5_000;
pub(crate) const CONTEXT: &str = "ConflictView";
/// The two panes' own context (#28): their line cursor's keys, Space among them, which the
/// result box outside it keeps for typing.
pub(crate) const PANES: &str = "ConflictPanes";

actions!(conflict, [CloseConflict, OtherPane]);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", CloseConflict, Some(CONTEXT)),
        KeyBinding::new("ctrl-f", OpenFind, Some(CONTEXT)),
        KeyBinding::new("left", OtherPane, Some(PANES)),
        KeyBinding::new("right", OtherPane, Some(PANES)),
    ]);
}

pub enum ConflictEvent {
    Close,
    /// Write `content` to the file at `path`; with `mark_resolved`, also stage it.
    Save {
        path: BString,
        content: String,
        mark_resolved: bool,
    },
    /// Resolve the file with one side's version whole (`deletes`: that side has no file).
    TakeSide {
        path: BString,
        side: Side,
        deletes: bool,
    },
    /// Open the file in git's merge tool.
    MergeTool(BString),
}

impl EventEmitter<ConflictEvent> for ConflictView {}

impl Focusable for ConflictView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

pub struct ConflictView {
    path: BString,
    regions: Vec<Region>,
    /// One entry per conflict; `None` = still unresolved.
    resolutions: Vec<Option<Resolution>>,
    /// The conflict shown in the two panes.
    current: usize,
    result: Entity<TextareaState>,
    /// The text last generated from the picks; anything else in the box was typed.
    generated: String,
    /// Replaces the result text on the next render (setting it needs the window).
    pending_result: Option<String>,
    /// Whether the box's text was typed (isn't the picks' text) and how many conflicts it
    /// still has; read again only after the text or the picks change (#24), not per frame.
    text_state: Option<(bool, usize)>,
    _result_events: Subscription,
    /// Debug builds: save as resolved on the first frame (`GITWEFT_DEBUG_CONFLICT_PICK`).
    /// Not in `new`: the workspace subscribes to our events only after creating us.
    save_on_first_frame: bool,
    /// A conflict of the whole file (a side deleted it, or it's binary): what each side
    /// has, shown instead of the line-by-line panes.
    whole: Option<ConflictSides>,
    /// The incoming side's name for a whole-file conflict (a branch, or a short SHA).
    whole_theirs: Option<String>,
    /// The Result shows the text box for typing instead of the colored preview.
    editing: bool,
    /// The Result as colored, read-only lines, each with where it came from.
    preview: Vec<PreviewLine>,
    preview_spans: Vec<Spans>,
    preview_scroll: UniformListScrollHandle,
    /// The preview's rows when long lines wrap (their heights differ).
    preview_list: ListState,
    /// The longest preview line, in columns.
    preview_longest: usize,
    /// The number columns' width, for the largest number (from the last frame, #42).
    number_width: Pixels,
    /// How far the preview's text is scrolled sideways.
    preview_hscroll: HScroll,
    /// How far the Ours and Theirs panes' text is scrolled sideways (both together).
    pane_hscroll: HScroll,
    /// Whether long lines were drawn wrapped last.
    wrapped: bool,
    /// Colors the preview beside the text box once typing pauses.
    _typing_task: Option<Task<()>>,
    /// Syntax colors of ours' and theirs' whole versions of the file, by line (the panes
    /// number their lines that way).
    side_spans: [Vec<Spans>; 2],
    /// Whether the colors were computed for the dark palette.
    dark: bool,
    /// The tab width its lines are shown and colored with (#80).
    tab_width: usize,
    /// Ctrl+F's bar (#43): it finds in the preview, the file as it resolves.
    find: FindSlot,
    _preview_task: Option<Task<()>>,
    _sides_task: Option<Task<()>>,
    /// The line the panes' keys are on: a side and a line of the current conflict (#28).
    cursor: Option<(PaneSide, usize)>,
    /// Each pane's scroll, and the next frame's order to scroll the cursor's line into view.
    pane_scroll: [ScrollHandle; 2],
    reveal: Rc<Cell<bool>>,
    /// Take keyboard focus on the first frame, so the keys work right away.
    focus_on_render: bool,
    /// Whether the panes had the focus when last drawn (the cursor shows then).
    focused: bool,
    focus: FocusHandle,
    /// The size of the file's conflict markers (`conflict-marker-size`, else 7; #16).
    marker_size: usize,
}

/// A line of the Result preview: its text (tabs expanded), where it came from, and the
/// conflict it belongs to.
struct PreviewLine {
    text: SharedString,
    origin: Origin,
    conflict: Option<usize>,
}

/// How a line of the current conflict shows in its pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineState {
    /// No lines picked: the side as a whole.
    Normal,
    Picked,
    /// Lines of this conflict are picked, not this one.
    Dropped,
}

impl ConflictView {
    /// `marker_size`: the file's `conflict-marker-size` (git writes markers that long).
    pub fn new(
        path: BString,
        text: String,
        marker_size: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let regions = gitweft_diff::conflict::parse_conflicts_sized(&text, marker_size);
        let count = conflict_count(&regions);
        let result = cx.new(|cx| TextareaState::new(window, cx).auto_grow(8, 400));
        result.update(cx, |state, cx| state.set_value(text.clone(), window, cx));
        // Typing redraws (the "edited by hand" note and the save buttons follow the text)
        // and, once it pauses, colors the preview beside the box.
        let events = cx.subscribe(&result, |this: &mut Self, _, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                this.text_state = None;
                if this.editing {
                    this.preview_typing(cx);
                }
                cx.notify();
            }
        });
        // Only the debug-build pick below changes it.
        #[cfg_attr(not(debug_assertions), allow(unused_mut))]
        let mut this = Self {
            path,
            regions,
            resolutions: vec![None; count],
            current: 0,
            result,
            generated: text,
            pending_result: None,
            text_state: None,
            _result_events: events,
            save_on_first_frame: false,
            whole: None,
            whole_theirs: None,
            editing: false,
            preview: Vec::new(),
            preview_spans: Vec::new(),
            preview_scroll: UniformListScrollHandle::new(),
            preview_list: code_scroll::list_state(0),
            preview_longest: 0,
            number_width: px(NUMBER_WIDTH),
            preview_hscroll: HScroll::default(),
            pane_hscroll: HScroll::default(),
            wrapped: code_scroll::wrapping(cx),
            _typing_task: None,
            side_spans: [Vec::new(), Vec::new()],
            dark: cx.palette().dark,
            tab_width: code_scroll::tab_width(cx),
            find: FindSlot::default(),
            _preview_task: None,
            _sides_task: None,
            cursor: None,
            pane_scroll: [ScrollHandle::new(), ScrollHandle::new()],
            reveal: Rc::new(Cell::new(false)),
            focus_on_render: true,
            focused: false,
            focus: cx.focus_handle(),
            marker_size,
        };
        if count > 0 {
            this.highlight_sides(cx);
            this.refresh_preview(None, cx);
        }
        // Debug builds: `GITWEFT_DEBUG_CONFLICT_PICK=ours|theirs` resolves the first
        // conflict right away, so screenshot checks can see a pick without clicking.
        #[cfg(debug_assertions)]
        if let Ok(side) = std::env::var("GITWEFT_DEBUG_CONFLICT_PICK") {
            match side.as_str() {
                "ours" => this.pick(Resolution::Ours, cx),
                "theirs" => this.pick(Resolution::Theirs, cx),
                "theirs-save" => {
                    this.pick(Resolution::Theirs, cx);
                    this.save_on_first_frame = true;
                }
                _ => {}
            }
        }
        this
    }

    /// A conflicted file the line-by-line tool can't show (a side deleted it, it's binary,
    /// or only one side has it): what each side has, and taking one side's version whole.
    /// `theirs`: what to call the incoming side (the file has no markers to name it).
    pub fn whole_file(
        path: BString,
        sides: ConflictSides,
        theirs: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let size = gitweft_diff::conflict::DEFAULT_MARKER_SIZE;
        let mut this = Self::new(path, String::new(), size, window, cx);
        this.whole = Some(sides);
        this.whole_theirs = theirs;
        this
    }

    /// The file the tool shows.
    pub fn path(&self) -> &BString {
        &self.path
    }

    /// The first conflict's marker labels (ours, theirs), e.g. `HEAD` and `topic`; for a
    /// whole-file conflict, `HEAD` and the incoming side's name.
    pub fn labels(&self) -> Option<(String, String)> {
        if self.whole.is_some() {
            return Some(("HEAD".to_owned(), self.whole_theirs.clone()?));
        }
        let conflict = self.conflicts().next()?;
        Some((
            conflict.ours_label().to_owned(),
            conflict.theirs_label().to_owned(),
        ))
    }

    fn conflicts(&self) -> impl Iterator<Item = &Conflict> {
        self.regions.iter().filter_map(|region| match region {
            Region::Conflict(conflict) => Some(conflict),
            Region::Clean(_) => None,
        })
    }

    fn current_conflict(&self) -> Option<&Conflict> {
        self.conflicts().nth(self.current)
    }

    /// Whether the box was typed in, and the conflicts left in it (cached, see
    /// `text_state`).
    fn text_state(&mut self, cx: &App) -> (bool, usize) {
        if let Some(state) = self.text_state {
            return state;
        }
        let text = self.result.read(cx).value();
        let edited = text.as_ref() != self.generated;
        let left = conflict_count(&gitweft_diff::conflict::parse_conflicts_sized(
            text.as_ref(),
            self.marker_size,
        ));
        self.text_state = Some((edited, left));
        (edited, left)
    }

    /// Resolves the current conflict and moves on to the next unresolved one.
    fn pick(&mut self, resolution: Resolution, cx: &mut Context<Self>) {
        let Some(slot) = self.resolutions.get_mut(self.current) else {
            return;
        };
        *slot = Some(resolution);
        self.regenerate(cx);
        if let Some(next) = (self.current + 1..self.resolutions.len())
            .chain(0..self.current)
            .find(|&ix| self.resolutions[ix].is_none())
        {
            self.current = next;
        }
        self.cursor_to_start();
        self.reveal_current();
        cx.notify();
    }

    /// Takes a line of the current conflict into the result, or leaves it out again. The
    /// first click starts from what the conflict was resolved to (all of ours after "Take
    /// ours"), and the tool stays on this conflict.
    fn toggle_line(&mut self, side: PaneSide, line: usize, cx: &mut Context<Self>) {
        let Some(conflict) = self.current_conflict() else {
            return;
        };
        let sizes = (conflict.ours.len(), conflict.theirs.len());
        let Some(slot) = self.resolutions.get_mut(self.current) else {
            return;
        };
        let (mut ours, mut theirs) = picks_from(slot.as_ref(), sizes);
        let list = match side {
            PaneSide::Ours => &mut ours,
            PaneSide::Theirs => &mut theirs,
        };
        toggle(list, line);
        *slot = Some(Resolution::Picked { ours, theirs });
        self.regenerate(cx);
        cx.notify();
    }

    /// How line `line` of the current conflict's `side` shows.
    fn line_state(&self, side: PaneSide, line: usize) -> LineState {
        match self.resolutions.get(self.current).and_then(Option::as_ref) {
            Some(Resolution::Picked { ours, theirs }) => {
                let list = match side {
                    PaneSide::Ours => ours,
                    PaneSide::Theirs => theirs,
                };
                if list.contains(&line) {
                    LineState::Picked
                } else {
                    LineState::Dropped
                }
            }
            _ => LineState::Normal,
        }
    }

    /// The result after a pick: the text for the box and the preview.
    fn regenerate(&mut self, cx: &mut Context<Self>) {
        let text = resolve(&self.regions, &self.resolutions);
        self.generated = text.clone();
        self.pending_result = Some(text);
        self.text_state = None;
        self.refresh_preview(None, cx);
    }

    /// Rebuilds the preview: from the picks, or from `typed` text (whose conflicts, if
    /// markers are left, show as unresolved), then colors it in the background.
    fn refresh_preview(&mut self, typed: Option<String>, cx: &mut Context<Self>) {
        let lines = match &typed {
            None => resolve_lines(&self.regions, &self.resolutions),
            Some(text) => {
                let regions = gitweft_diff::conflict::parse_conflicts_sized(text, self.marker_size);
                resolve_lines(&regions, &[])
            }
        };
        self.preview = lines
            .into_iter()
            .map(|line| PreviewLine {
                text: display(&line.text, self.tab_width).into(),
                origin: line.origin,
                // Typed text's conflicts aren't the tool's.
                conflict: line.conflict.filter(|_| typed.is_none()),
            })
            .collect();
        self.preview_longest = self
            .preview
            .iter()
            .map(|line| columns(&line.text))
            .max()
            .unwrap_or(0);
        // The wrapped list keeps its place: typing refreshes it all the time.
        let top = self.preview_list.logical_scroll_top();
        self.preview_list.reset(self.preview.len());
        self.preview_list.scroll_to(ListOffset {
            item_ix: top.item_ix.min(self.preview.len().saturating_sub(1)),
            offset_in_item: top.offset_in_item,
        });
        self.highlight_preview(cx);
        // A new preview: an open find bar's matches are found in it again.
        self.refind(cx);
    }

    /// Colors what's typed in the box once typing pauses.
    fn preview_typing(&mut self, cx: &mut Context<Self>) {
        self._typing_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(TYPING_DELAY).await;
            this.update(cx, |this, cx| {
                this.preview_text(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// The preview of the box's text: from the picks while it's what they generated.
    fn preview_text(&mut self, cx: &mut Context<Self>) {
        let text = self.result.read(cx).value().to_string();
        let typed = (text != self.generated).then_some(text);
        self.refresh_preview(typed, cx);
    }

    /// Colors the preview on a background thread (loading the syntaxes the first time
    /// takes a while). Colors of an older preview are dropped meanwhile.
    fn highlight_preview(&mut self, cx: &mut Context<Self>) {
        self.preview_spans.clear();
        let texts: Vec<String> = self
            .preview
            .iter()
            .map(|line| line.text.to_string())
            .collect();
        let (path, dark) = (self.path.to_string(), self.dark);
        let task = cx
            .background_executor()
            .spawn(async move { colors(&path, &texts, dark) });
        self._preview_task = Some(cx.spawn(async move |this, cx| {
            let spans = task.await;
            this.update(cx, |this, cx| {
                this.preview_spans = spans;
                cx.notify();
            })
            .ok();
        }));
    }

    /// Colors ours' and theirs' versions of the file for the panes, in the background.
    fn highlight_sides(&mut self, cx: &mut Context<Self>) {
        let count = self.resolutions.len();
        let version = |resolution: Resolution| -> Vec<String> {
            resolve_lines(&self.regions, &vec![Some(resolution); count])
                .iter()
                .map(|line| display(&line.text, self.tab_width))
                .collect()
        };
        let (ours, theirs) = (version(Resolution::Ours), version(Resolution::Theirs));
        let (path, dark) = (self.path.to_string(), self.dark);
        let task = cx
            .background_executor()
            .spawn(async move { [colors(&path, &ours, dark), colors(&path, &theirs, dark)] });
        self._sides_task = Some(cx.spawn(async move |this, cx| {
            let spans = task.await;
            this.update(cx, |this, cx| {
                this.side_spans = spans;
                cx.notify();
            })
            .ok();
        }));
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
                code_keys::reveal(
                    self.wrapped,
                    &self.preview_scroll,
                    &self.preview_list,
                    found.row,
                );
                if let Some(line) = self.preview.get(found.row)
                    && !self.wrapped
                {
                    let width = code_scroll::char_width(window, cx.metrics().text.mono);
                    let at = |byte: usize| width * columns(&line.text[..byte]) as f32;
                    self.preview_hscroll
                        .reveal(at(found.range.start), at(found.range.end));
                }
                cx.notify();
            }
            FindEvent::Closed => {
                self.focus.focus(window, cx);
                cx.notify();
            }
        }
    }

    /// Finds the bar's query in the preview again: it changed, or the preview did.
    fn refind(&mut self, cx: &mut Context<Self>) {
        let matches = match self.find.query(cx) {
            Some((query, case)) => find_matches(
                self.preview
                    .iter()
                    .map(|line| line.text.as_ref())
                    .enumerate(),
                &query,
                case,
            ),
            None => Vec::new(),
        };
        let from = self
            .preview
            .iter()
            .position(|line| line.conflict == Some(self.current))
            .unwrap_or(0);
        self.find.set_matches(matches, from, cx);
        cx.notify();
    }

    /// Scrolls the preview to the current conflict.
    fn reveal_current(&self) {
        if let Some(ix) = self
            .preview
            .iter()
            .position(|line| line.conflict == Some(self.current))
        {
            if self.wrapped {
                self.preview_list.scroll_to_reveal_item(ix);
            } else {
                self.preview_scroll
                    .scroll_to_item(ix, ScrollStrategy::Center);
            }
        }
    }

    /// The text box (with the preview beside it), or the preview alone. Leaving the box
    /// shows what was typed.
    fn set_editing(&mut self, editing: bool, cx: &mut Context<Self>) {
        self.editing = editing;
        if !editing {
            self._typing_task = None;
            self.preview_text(cx);
        }
        cx.notify();
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.resolutions.len();
        if count == 0 {
            return;
        }
        self.current = (self.current as isize + delta).rem_euclid(count as isize) as usize;
        self.cursor_to_start();
        self.reveal_current();
        cx.notify();
    }

    /// Another conflict is shown: the cursor to its first line, on the same side.
    fn cursor_to_start(&mut self) {
        if let Some((side, _)) = self.cursor {
            self.cursor = Some((side, 0));
            self.reveal.set(true);
        }
    }

    /// The current conflict's line count on `side`.
    fn side_len(&self, side: PaneSide) -> usize {
        self.current_conflict().map_or(0, |conflict| match side {
            PaneSide::Ours => conflict.ours.len(),
            PaneSide::Theirs => conflict.theirs.len(),
        })
    }

    fn put_cursor(&mut self, side: PaneSide, line: usize, cx: &mut Context<Self>) {
        self.cursor = Some((side, line));
        self.reveal.set(true);
        cx.notify();
    }

    /// Up/Down, Home/End: the cursor through the current conflict's lines on its side (Ours
    /// to begin with).
    fn move_cursor(&mut self, to: Step, cx: &mut Context<Self>) {
        let side = self.cursor.map_or(PaneSide::Ours, |(side, _)| side);
        let current = self.cursor.map(|(_, line)| line);
        if let Some(line) = list_keys::step(self.side_len(side), current, to) {
            self.put_cursor(side, line, cx);
        }
    }

    /// Left/Right: the other pane, on the same line (or its last).
    fn other_pane(&mut self, cx: &mut Context<Self>) {
        let (side, line) = self.cursor.unwrap_or((PaneSide::Theirs, 0));
        let other = match side {
            PaneSide::Ours => PaneSide::Theirs,
            PaneSide::Theirs => PaneSide::Ours,
        };
        if let Some(last) = self.side_len(other).checked_sub(1) {
            self.put_cursor(other, line.min(last), cx);
        }
    }

    /// Space: the cursor's line alone into the result, or left out again (a click's pick).
    fn toggle_cursor(&mut self, cx: &mut Context<Self>) {
        if let Some((side, line)) = self.cursor
            && line < self.side_len(side)
        {
            self.toggle_line(side, line, cx);
        }
    }

    /// Enter: the cursor's side taken whole, as its pane's button does (the tool moves on).
    fn take_cursor_side(&mut self, cx: &mut Context<Self>) {
        let resolution = match self.cursor.map_or(PaneSide::Ours, |(side, _)| side) {
            PaneSide::Ours => Resolution::Ours,
            PaneSide::Theirs => Resolution::Theirs,
        };
        self.pick(resolution, cx);
    }

    /// Escape: out of the text box while typing (what was typed stays), else closes.
    fn escape(&mut self, _: &CloseConflict, _: &mut Window, cx: &mut Context<Self>) {
        if self.editing {
            self.set_editing(false, cx);
        } else {
            cx.emit(ConflictEvent::Close);
        }
    }

    /// The preview's rows: the result's line number, a bar in the color of where the line
    /// came from (a tint while unresolved), the colored text. A line of a conflict shows
    /// that conflict in the panes.
    fn render_preview_rows(
        &mut self,
        range: Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        range
            .filter_map(|ix| self.render_preview_row(ix, false, cx))
            .collect()
    }

    fn render_preview_row(
        &self,
        ix: usize,
        wrap: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (p, m) = (*cx.palette(), *cx.metrics());
        let line = self.preview.get(ix)?;
        let bar = match line.origin {
            Origin::Clean => None,
            Origin::Ours => Some(p.lanes[2]),
            Origin::Theirs => Some(p.lanes[3]),
            Origin::Base => Some(p.fg3),
            Origin::Edited => Some(p.acc),
            Origin::Unresolved => Some(p.modified),
        };
        let bg = if line.origin == Origin::Unresolved {
            p.modified.opacity(0.12)
        } else {
            transparent_black()
        };
        let text = StyledText::new(line.text.clone()).with_highlights(combine_highlights(
            styles(&line.text, self.preview_spans.get(ix)),
            self.find.highlights(ix, p.find_bg, cx),
        ));
        let tint = bar.map(|bar| (bg, bar));
        let offset = self.preview_hscroll.offset();
        let row = code_row(
            (Some(ix + 1), self.number_width),
            text,
            tint,
            wrap,
            offset,
            &p,
            &m,
        );
        Some(match line.conflict {
            Some(conflict) => row
                .id(("preview-line", ix))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.current = conflict;
                    cx.notify();
                }))
                .into_any_element(),
            None => row.into_any_element(),
        })
    }

    /// The Result as colored lines: one line each and scrolled sideways, or wrapped (rows
    /// as high as their text, in a `list`).
    fn render_preview(
        &self,
        wrap: bool,
        p: &Palette,
        m: &Metrics,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let z = |value: f32| px(value * m.zoom);
        let lines = if wrap {
            list(
                self.preview_list.clone(),
                cx.processor(|this, ix, _, cx| {
                    this.render_preview_row(ix, true, cx)
                        .unwrap_or_else(|| div().into_any_element())
                }),
            )
            .size_full()
            .into_any_element()
        } else {
            uniform_list(
                "conflict-preview",
                self.preview.len(),
                cx.processor(Self::render_preview_rows),
            )
            .track_scroll(&self.preview_scroll)
            .size_full()
            .into_any_element()
        };
        div()
            .relative()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .bg(p.bg)
            .py(z(4.))
            .font_family(mono_font())
            .text_size(m.text.mono)
            .child(lines)
            .when(!wrap, |this| {
                this.child(code_scroll::overlay(
                    cx.entity().downgrade(),
                    |this: &mut Self| &mut this.preview_hscroll,
                    &self.preview_hscroll,
                    p,
                    m,
                ))
            })
            .into_any_element()
    }

    fn save(&mut self, mark_resolved: bool, cx: &mut Context<Self>) {
        let content = self.result.read(cx).value().to_string();
        cx.emit(ConflictEvent::Save {
            path: self.path.clone(),
            content,
            mark_resolved,
        });
    }

    /// The ways to resolve the current conflict whole, with the labels of the tool's
    /// buttons (base only when the conflict has one), for the palette.
    fn picks(has_base: bool) -> Vec<(Resolution, String)> {
        let mut picks = vec![
            (Resolution::Ours, t!("conflict.take_ours").into_owned()),
            (Resolution::Theirs, t!("conflict.take_theirs").into_owned()),
            (Resolution::Both, t!("conflict.take_both").into_owned()),
            (
                Resolution::BothTheirsFirst,
                t!("conflict.take_both_reversed").into_owned(),
            ),
        ];
        if has_base {
            picks.push((Resolution::Base, t!("conflict.take_base").into_owned()));
        }
        picks
    }

    /// The tool's buttons as palette commands, with the file (and which conflict) as the
    /// detail. A whole-file conflict offers its cards' Take / Delete and the merge tool.
    pub(crate) fn palette_commands(&self, this: &WeakEntity<Self>) -> Vec<PaletteCommand> {
        let group: SharedString = t!("palette.group.conflict").into_owned().into();
        let path = self.path.to_string();
        let mut commands = Vec::new();
        if let Some(sides) = &self.whole {
            let cards = [
                (Side::Ours, sides.ours.is_none(), t!("conflict.label_ours")),
                (
                    Side::Theirs,
                    sides.theirs.is_none(),
                    t!("conflict.label_theirs"),
                ),
            ];
            for (side, deletes, name) in cards {
                let label = match (deletes, side) {
                    (true, _) => t!("conflict.take_deletion"),
                    (false, Side::Ours) => t!("conflict.take_ours"),
                    (false, Side::Theirs) => t!("conflict.take_theirs"),
                };
                let file = self.path.clone();
                let command = view_command(this, &group, label, move |_, _, cx| {
                    cx.emit(ConflictEvent::TakeSide {
                        path: file.clone(),
                        side,
                        deletes,
                    })
                });
                commands.push(
                    command
                        .detail(t!("common.middle_dot", first = path, second = name).into_owned()),
                );
            }
            let file = self.path.clone();
            let label = t!("external.merge_tool");
            let command = view_command(this, &group, label, move |_, _, cx| {
                cx.emit(ConflictEvent::MergeTool(file.clone()))
            });
            commands.push(command.detail(path.clone()));
        } else if !self.resolutions.is_empty() {
            let count = self.resolutions.len();
            let position = t!(
                "conflict.position",
                current = self.current + 1,
                count = count
            );
            let detail = t!("common.middle_dot", first = path, second = position).into_owned();
            let has_base = self.current_conflict().is_some_and(|c| c.base.is_some());
            for (resolution, label) in Self::picks(has_base) {
                let command = view_command(this, &group, label, move |this, _, cx| {
                    this.pick(resolution.clone(), cx)
                });
                commands.push(command.detail(detail.clone()));
            }
            if count > 1 {
                for (delta, label) in [
                    (1, t!("palette.conflict_next")),
                    (-1, t!("palette.conflict_previous")),
                ] {
                    let command =
                        view_command(this, &group, label, move |this, _, cx| this.step(delta, cx));
                    commands.push(command.detail(detail.clone()));
                }
            }
            let label = if self.editing {
                t!("palette.conflict_stop_editing")
            } else {
                t!("conflict.edit_by_hand")
            };
            commands.push(view_command(this, &group, label, |this, _, cx| {
                this.set_editing(!this.editing, cx)
            }));
            for (mark_resolved, label) in [
                (false, t!("conflict.save")),
                (true, t!("conflict.save_resolved")),
            ] {
                let command = view_command(this, &group, label, move |this, _, cx| {
                    this.save(mark_resolved, cx)
                });
                commands.push(command.detail(path.clone()));
            }
        }
        let label = t!("palette.conflict_close");
        commands.push(view_command(this, &group, label, |_, _, cx| {
            cx.emit(ConflictEvent::Close)
        }));
        commands
    }

    /// One side of the current conflict: a 34 px header (its color, name, marker label and
    /// "Take …"), then its lines on the side's tint, with clean lines around them.
    #[allow(clippy::too_many_arguments)]
    fn render_side(
        &self,
        id: &'static str,
        side: PaneSide,
        detail: String,
        take: Resolution,
        take_label: String,
        p: &Palette,
        m: &Metrics,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let z = |value: f32| px(value * m.zoom);
        let side_ix = match side {
            PaneSide::Ours => 0,
            PaneSide::Theirs => 1,
        };
        let (label, color, tint) = match side {
            PaneSide::Ours => (t!("conflict.label_ours"), p.lanes[2], p.ours_bg),
            PaneSide::Theirs => (t!("conflict.label_theirs"), p.lanes[3], p.theirs_bg),
        };
        let chosen = self
            .resolutions
            .get(self.current)
            .is_some_and(|picked| picked.as_ref() == Some(&take));
        let lines = pane_lines(
            &self.regions,
            self.current,
            side,
            CONTEXT_LINES,
            self.tab_width,
        );
        let (wrap, offset) = (self.wrapped, self.pane_hscroll.offset());
        let (spans, row_id) = match side {
            PaneSide::Ours => (&self.side_spans[0], "ours-line"),
            PaneSide::Theirs => (&self.side_spans[1], "theirs-line"),
        };
        let styled = |line: &PaneLine| {
            let spans = line.number.checked_sub(1).and_then(|ix| spans.get(ix));
            StyledText::new(line.text.clone()).with_highlights(styles(&line.text, spans))
        };
        let empty = lines.conflict.is_empty().then(|| {
            code_row(
                (None, self.number_width),
                t!("conflict.empty_side").into_owned(),
                Some((tint, color)),
                wrap,
                Pixels::ZERO,
                p,
                m,
            )
            .text_color(p.fg3)
        });
        v_flex()
            .flex_1()
            .min_w_0()
            .bg(p.bg)
            .child(
                pane_header(p, m)
                    .child(div().size(z(8.)).flex_shrink_0().rounded(z(2.)).bg(color))
                    .child(
                        div()
                            .flex_shrink_0()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.fg)
                            .child(label.into_owned()),
                    )
                    .child(truncated("detail", detail))
                    .child(div().flex_1())
                    .child(
                        Button::new(id)
                            .xsmall()
                            .when(chosen, |button| button.primary())
                            .when(!chosen, |button| button.outline())
                            .label(take_label)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.pick(take.clone(), cx)),
                            ),
                    ),
            )
            .child(
                // Both panes scroll sideways together; the overlay takes the wheel's
                // sideways moves and draws the thumb.
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id((id, 1u32))
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.pane_scroll[side_ix])
                            .py(z(6.))
                            .font_family(mono_font())
                            .text_size(m.text.mono)
                            .children(lines.before.iter().map(|line| {
                                code_row(
                                    (Some(line.number), self.number_width),
                                    styled(line),
                                    None,
                                    wrap,
                                    offset,
                                    p,
                                    m,
                                )
                            }))
                            // A click takes the line alone into the result (or leaves it out).
                            .children(lines.conflict.iter().enumerate().map(|(ix, line)| {
                                let state = self.line_state(side, ix);
                                let tint = (state != LineState::Dropped).then_some((tint, color));
                                code_row(
                                    (Some(line.number), self.number_width),
                                    styled(line),
                                    tint,
                                    wrap,
                                    offset,
                                    p,
                                    m,
                                )
                                .id((row_id, ix))
                                .cursor_pointer()
                                .when(state == LineState::Dropped, |row| row.opacity(p.dim))
                                // The keys' cursor, kept in view (#28): outlined too, since
                                // the conflict's own stripe hides a bar at the edge.
                                .when(self.focused && self.cursor == Some((side, ix)), |row| {
                                    row.relative()
                                        .child(list_keys::revealing_cursor_bar(
                                            p.acc,
                                            m,
                                            &self.pane_scroll[side_ix],
                                            &self.reveal,
                                        ))
                                        .child(
                                            div()
                                                .absolute()
                                                .inset_0()
                                                .border_1()
                                                .border_color(p.acc.opacity(0.8)),
                                        )
                                })
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.cursor = Some((side, ix));
                                        this.toggle_line(side, ix, cx)
                                    },
                                ))
                            }))
                            .children(empty)
                            .children(lines.after.iter().map(|line| {
                                code_row(
                                    (Some(line.number), self.number_width),
                                    styled(line),
                                    None,
                                    wrap,
                                    offset,
                                    p,
                                    m,
                                )
                            })),
                    )
                    .when(!wrap, |pane| {
                        pane.child(code_scroll::overlay(
                            cx.entity().downgrade(),
                            |this: &mut Self| &mut this.pane_hscroll,
                            &self.pane_hscroll,
                            p,
                            m,
                        ))
                    }),
            )
    }
}

impl ConflictView {
    /// The whole-file mode: the file bar, why the file can't be merged by lines (with the
    /// merge tool as the other way out), then a card per side.
    fn render_whole(&self, sides: &ConflictSides, cx: &mut Context<Self>) -> impl IntoElement {
        let (p, m) = (*cx.palette(), *cx.metrics());
        let z = |value: f32| px(value * m.zoom);
        let explain = match (&sides.ours, &sides.theirs) {
            (Some(_), None) => t!("conflict.whole_deleted_theirs"),
            (None, Some(_)) => t!("conflict.whole_deleted_ours"),
            (None, None) => t!("conflict.whole_deleted_both"),
            (Some(ours), Some(theirs)) if ours.is_submodule() || theirs.is_submodule() => {
                t!("conflict.whole_submodule")
            }
            (Some(ours), Some(theirs)) if ours.is_symlink() || theirs.is_symlink() => {
                t!("conflict.whole_symlink")
            }
            (Some(_), Some(_)) if sides.binary_by_attributes => {
                t!("conflict.whole_binary_attributes")
            }
            (Some(_), Some(_)) => t!("conflict.whole_binary"),
        }
        .into_owned();
        let file_bar = h_flex()
            .h(z(34.))
            .flex_shrink_0()
            .px(z(14.))
            .gap(z(8.))
            .border_b_1()
            .border_color(p.line)
            .child(Icon::new(AppIcon::File).text_color(p.fg3))
            .child(
                truncated("path", self.path.to_string())
                    .font_family(mono_font())
                    .text_size(m.text.mono)
                    .text_color(p.fg2),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .font_family(mono_font())
                    .text_size(m.text.mono)
                    .text_color(p.fg3)
                    .child(t!("conflict.whole_file_after").into_owned()),
            )
            .child(div().flex_1())
            .child(
                link(
                    "conflict-close",
                    t!("conflict.close").into_owned(),
                    p.fg3,
                    true,
                    &m,
                )
                .on_click(cx.listener(|_, _, _, cx| cx.emit(ConflictEvent::Close))),
            );
        let path = self.path.clone();
        let note = h_flex()
            .flex_shrink_0()
            .px(z(14.))
            .py(z(10.))
            .gap(z(12.))
            .bg(p.panel2)
            .border_b_1()
            .border_color(p.line)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(m.text.small)
                    .text_color(p.fg2)
                    .whitespace_normal()
                    .child(explain),
            )
            .child(
                Button::new("whole-merge-tool")
                    .xsmall()
                    .outline()
                    .label(t!("external.merge_tool").into_owned())
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(ConflictEvent::MergeTool(path.clone()))
                    })),
            );
        let had_base = sides.base.is_some();
        v_flex()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::escape))
            .size_full()
            .bg(p.bg)
            .child(file_bar)
            .child(note)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .gap(px(1.))
                    .bg(p.line)
                    .child(self.render_card(
                        PaneSide::Ours,
                        sides.ours.as_ref(),
                        had_base,
                        &p,
                        &m,
                        cx,
                    ))
                    .child(self.render_card(
                        PaneSide::Theirs,
                        sides.theirs.as_ref(),
                        had_base,
                        &p,
                        &m,
                        cx,
                    )),
            )
    }

    /// One side of a whole-file conflict: its header with "Take …" ("Delete the file" when
    /// it has none), and what it has: changed, added or deleted, the size, text or binary.
    fn render_card(
        &self,
        side: PaneSide,
        stage: Option<&StageInfo>,
        had_base: bool,
        p: &Palette,
        m: &Metrics,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let z = |value: f32| px(value * m.zoom);
        let (label, color, id, take, git_side) = match side {
            PaneSide::Ours => (
                t!("conflict.label_ours"),
                p.lanes[2],
                "whole-take-ours",
                t!("conflict.take_ours"),
                Side::Ours,
            ),
            PaneSide::Theirs => (
                t!("conflict.label_theirs"),
                p.lanes[3],
                "whole-take-theirs",
                t!("conflict.take_theirs"),
                Side::Theirs,
            ),
        };
        let deletes = stage.is_none();
        let take_label = if deletes {
            t!("conflict.take_deletion")
        } else {
            take
        }
        .into_owned();
        let (state, details) = match stage {
            None => (t!("conflict.side_deleted").into_owned(), Vec::new()),
            Some(stage) => {
                let state = if had_base {
                    t!("conflict.side_changed")
                } else {
                    t!("conflict.side_added")
                };
                (state.into_owned(), stage_details(stage))
            }
        };
        let path = self.path.clone();
        v_flex()
            .flex_1()
            .min_w_0()
            .bg(p.bg)
            .child(
                pane_header(p, m)
                    .child(div().size(z(8.)).flex_shrink_0().rounded(z(2.)).bg(color))
                    .child(
                        div()
                            .flex_shrink_0()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.fg)
                            .child(label.into_owned()),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new(id)
                            .xsmall()
                            .outline()
                            .label(take_label)
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(ConflictEvent::TakeSide {
                                    path: path.clone(),
                                    side: git_side,
                                    deletes,
                                })
                            })),
                    ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap(z(4.))
                    .child(
                        div()
                            .text_size(m.text.ui)
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(if deletes { p.del } else { p.fg })
                            .child(state),
                    )
                    .children(details.into_iter().enumerate().map(|(ix, detail)| {
                        truncated(("detail", ix), detail)
                            .max_w_full()
                            .px(z(16.))
                            .text_size(m.text.small)
                            .text_color(p.fg3)
                    })),
            )
    }
}

impl Render for ConflictView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.focus_on_render) {
            self.focus.focus(window, cx);
        }
        self.focused = self.focus.contains_focused(window, cx);
        if let Some(text) = self.pending_result.take() {
            self.result
                .update(cx, |state, cx| state.set_value(text, window, cx));
            self.text_state = None;
        }
        if std::mem::take(&mut self.save_on_first_frame) {
            self.save(true, cx);
        }
        if let Some(sides) = self.whole.clone() {
            return self.render_whole(&sides, cx).into_any_element();
        }
        // A light/dark switch: colors for the other palette.
        let dark = cx.palette().dark;
        if dark != self.dark && !self.resolutions.is_empty() {
            self.dark = dark;
            self.highlight_sides(cx);
            self.highlight_preview(cx);
        }
        // A Tab width change: the lines again, and their colors (#80).
        let tab = code_scroll::tab_width(cx);
        if tab != self.tab_width && !self.resolutions.is_empty() {
            self.tab_width = tab;
            self.highlight_sides(cx);
            self.preview_text(cx);
        }
        let (p, m) = (*cx.palette(), *cx.metrics());
        let z = |value: f32| px(value * m.zoom);
        let wrap = code_scroll::wrapping(cx);
        if wrap != self.wrapped {
            let rows = self.preview.len();
            code_scroll::switch_mode(
                wrap,
                &self.preview_scroll,
                &self.preview_list,
                rows,
                m.conflict_line,
            );
            self.wrapped = wrap;
        }
        // Wide enough for the largest number, with the column's 12 px after it (#42).
        self.number_width = code_scroll::number_width(
            window,
            m.text.mono,
            largest_number(&self.regions, self.preview.len()),
            z(12.),
            z(NUMBER_WIDTH),
        );
        if !wrap {
            // The bar and the number column stay put.
            let fixed = z(3.) + self.number_width;
            let longest = pane_longest(&self.regions, self.current, self.tab_width);
            let panes = code_scroll::text_width(window, m.text.mono, longest);
            self.pane_hscroll
                .update(panes, self.pane_hscroll.viewport() - fixed);
            let preview = code_scroll::text_width(window, m.text.mono, self.preview_longest);
            self.preview_hscroll
                .update(preview, self.preview_hscroll.viewport() - fixed);
        }
        let count = self.resolutions.len();
        let (edited, markers_left) = self.text_state(cx);
        let position = if count == 0 {
            t!("conflict.none").into_owned()
        } else {
            t!(
                "conflict.position",
                current = self.current + 1,
                count = count
            )
            .into_owned()
        };

        // 34 px: the file, which conflict, and the ways to move and leave.
        let file_bar = h_flex()
            .h(z(34.))
            .flex_shrink_0()
            .px(z(14.))
            .gap(z(8.))
            .border_b_1()
            .border_color(p.line)
            .child(Icon::new(AppIcon::File).text_color(p.fg3))
            .child(
                truncated("path", self.path.to_string())
                    .font_family(mono_font())
                    .text_size(m.text.mono)
                    .text_color(p.fg2),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .font_family(mono_font())
                    .text_size(m.text.mono)
                    .text_color(p.fg3)
                    .child(format!("· {position}")),
            )
            .when(count > 0, |bar| {
                bar.child(
                    truncated("summary", t!("conflict.pick_hint").into_owned())
                        .text_size(m.text.small)
                        .text_color(p.fg3),
                )
            })
            .child(div().flex_1())
            .child(
                link(
                    "conflict-prev",
                    t!("conflict.previous").into_owned(),
                    p.fg3,
                    count > 1,
                    &m,
                )
                .on_click(cx.listener(|this, _, _, cx| this.step(-1, cx))),
            )
            .child(
                link(
                    "conflict-next",
                    t!("conflict.next").into_owned(),
                    p.fg,
                    count > 1,
                    &m,
                )
                .on_click(cx.listener(|this, _, _, cx| this.step(1, cx))),
            )
            .child(code_scroll::wrap_button(wrap))
            .child(
                link(
                    "conflict-close",
                    t!("conflict.close").into_owned(),
                    p.fg3,
                    true,
                    &m,
                )
                .on_click(cx.listener(|_, _, _, cx| cx.emit(ConflictEvent::Close))),
            );

        let current = self.current_conflict().cloned();
        let has_base = current.as_ref().is_some_and(|c| c.base.is_some());
        let sides = current.map(|conflict| {
            h_flex()
                .flex_1()
                .min_h_0()
                .items_stretch()
                .gap(px(1.))
                // The panes hold the keys: the cursor's, Space among them (#28).
                .key_context(PANES)
                .track_focus(&self.focus)
                .on_action(cx.listener(|this, _: &NextRow, _, cx| this.move_cursor(Step::Next, cx)))
                .on_action(
                    cx.listener(|this, _: &PreviousRow, _, cx| {
                        this.move_cursor(Step::Previous, cx)
                    }),
                )
                .on_action(
                    cx.listener(|this, _: &FirstRow, _, cx| this.move_cursor(Step::First, cx)),
                )
                .on_action(cx.listener(|this, _: &LastRow, _, cx| this.move_cursor(Step::Last, cx)))
                .on_action(cx.listener(|this, _: &OtherPane, _, cx| this.other_pane(cx)))
                .on_action(cx.listener(|this, _: &ToggleRow, _, cx| this.toggle_cursor(cx)))
                .on_action(cx.listener(|this, _: &OpenRow, _, cx| this.take_cursor_side(cx)))
                .child(self.render_side(
                    "take-ours",
                    PaneSide::Ours,
                    conflict.ours_label().to_owned(),
                    Resolution::Ours,
                    t!("conflict.take_ours").into_owned(),
                    &p,
                    &m,
                    cx,
                ))
                .child(self.render_side(
                    "take-theirs",
                    PaneSide::Theirs,
                    conflict.theirs_label().to_owned(),
                    Resolution::Theirs,
                    t!("conflict.take_theirs").into_owned(),
                    &p,
                    &m,
                    cx,
                ))
        });

        let has_sides = sides.is_some();
        let (note, note_color) = match (edited, markers_left) {
            (true, 0) => (t!("conflict.edited").into_owned(), p.fg3),
            (false, 0) => (t!("conflict.all_resolved").into_owned(), p.fg3),
            (_, left) => (
                tn!("conflict.markers_left", count = left).into_owned(),
                p.modified,
            ),
        };
        let take = |id: &'static str, label: String, resolution: Resolution| {
            Button::new(id)
                .xsmall()
                .outline()
                .label(label)
                .on_click(cx.listener(move |this, _, _, cx| this.pick(resolution.clone(), cx)))
        };
        let picks = (count > 0).then(|| {
            h_flex()
                .gap(z(6.))
                .child(take(
                    "take-both",
                    t!("conflict.take_both").into_owned(),
                    Resolution::Both,
                ))
                .child(take(
                    "take-both-reversed",
                    t!("conflict.take_both_reversed").into_owned(),
                    Resolution::BothTheirsFirst,
                ))
                .when(has_base, |row| {
                    row.child(take(
                        "take-base",
                        t!("conflict.take_base").into_owned(),
                        Resolution::Base,
                    ))
                })
        });
        let result = v_flex()
            .flex_1()
            .min_h_0()
            .bg(p.bg)
            .child(
                pane_header(&p, &m)
                    .child(Icon::new(AppIcon::Check).text_color(p.add))
                    .child(
                        div()
                            .flex_shrink_0()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.fg)
                            .child(t!("conflict.result").into_owned()),
                    )
                    .child(truncated("note", note).text_color(note_color))
                    .child(div().flex_1())
                    .children(picks)
                    .child(
                        Button::new("conflict-edit")
                            .xsmall()
                            .ghost()
                            .label(
                                if self.editing {
                                    t!("conflict.edit_done")
                                } else {
                                    t!("conflict.edit_by_hand")
                                }
                                .into_owned(),
                            )
                            .on_click(
                                cx.listener(|this, _, _, cx| this.set_editing(!this.editing, cx)),
                            ),
                    )
                    .child(
                        Button::new("conflict-save")
                            .xsmall()
                            .ghost()
                            .label(t!("conflict.save").into_owned())
                            .on_click(cx.listener(|this, _, _, cx| this.save(false, cx))),
                    )
                    .child(
                        Button::new("conflict-save-resolved")
                            .xsmall()
                            .primary()
                            .label(t!("conflict.save_resolved").into_owned())
                            .disabled(markers_left > 0)
                            .on_click(cx.listener(|this, _, _, cx| this.save(true, cx))),
                    ),
            )
            .child({
                let preview = self.render_preview(wrap, &p, &m, cx);
                if self.editing {
                    // The box, and what it holds colored beside it, following the typing.
                    h_flex()
                        .flex_1()
                        .min_h_0()
                        .items_stretch()
                        .gap(px(1.))
                        .bg(p.line)
                        .child(
                            div()
                                .id("conflict-result")
                                .flex_1()
                                .min_w_0()
                                .bg(p.bg)
                                .overflow_y_scroll()
                                .px(z(6.))
                                .py(z(4.))
                                .font_family(mono_font())
                                .text_size(m.text.mono)
                                .child(Textarea::new(&self.result).appearance(false)),
                        )
                        .child(preview)
                        .into_any_element()
                } else {
                    preview
                }
            });

        v_flex()
            .key_context(CONTEXT)
            // With no conflict panes (none left), the view itself holds the keys.
            .when(!has_sides, |root| root.track_focus(&self.focus))
            .on_action(cx.listener(Self::escape))
            .on_action(cx.listener(|this, _: &OpenFind, window, cx| this.open_find(window, cx)))
            .on_action(cx.listener(|this, _: &NextHunk, _, cx| this.step(1, cx)))
            .on_action(cx.listener(|this, _: &PreviousHunk, _, cx| this.step(-1, cx)))
            .size_full()
            .bg(p.bg)
            .child(file_bar)
            .children(self.find.element(cx))
            .child(
                // The panes are separated by 1 px lines: the line color shows through the gaps.
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .gap(px(1.))
                    .bg(p.line)
                    .children(sides)
                    .child(result),
            )
            .into_any_element()
    }
}

/// What a whole-file card says about a side's file: a symlink's target, a submodule's
/// commit, or the size and whether it's text; then "Executable" for an executable file.
fn stage_details(stage: &StageInfo) -> Vec<String> {
    let first = if let Some(target) = &stage.symlink_target {
        t!("conflict.side_symlink", target = target.to_string())
    } else if stage.is_submodule() {
        let commit = stage.oid.to_hex_with_len(8).to_string();
        t!("conflict.side_submodule", commit = commit)
    } else {
        let kind = if stage.binary {
            t!("conflict.kind_binary")
        } else {
            t!("conflict.kind_text")
        };
        let size = crate::diff_view::human_size(stage.size as usize);
        t!("conflict.side_detail", size = size, kind = kind)
    };
    let mut details = vec![first.into_owned()];
    if stage.is_executable() {
        details.push(t!("conflict.side_executable").into_owned());
    }
    details
}

/// A pane's 34 px header row; callers add its contents.
fn pane_header(p: &Palette, m: &Metrics) -> Div {
    let z = |value: f32| px(value * m.zoom);
    h_flex()
        .h(z(34.))
        .flex_shrink_0()
        .px(z(12.))
        .gap(z(8.))
        .bg(p.bg)
        .border_b_1()
        .border_color(p.line)
        .text_size(m.text.mono)
        .text_color(p.fg2)
}

/// A text control in the file bar ("↑ Previous"); dimmed and inert when not `enabled`.
/// The file bar's words (Previous, Next, Close): tab stops, clicked with Enter or Space too
/// (#30); a disabled one ignores clicks.
fn link(
    id: &'static str,
    label: String,
    color: Hsla,
    enabled: bool,
    m: &Metrics,
) -> gitweft_ui::widgets::LinkButton {
    gitweft_ui::widgets::LinkButton::new(id, label, color)
        .size(m.text.small)
        .enabled(enabled)
}

/// A line as the tool shows it: without its line ending, tabs expanded to tab stops `tab`
/// columns apart.
fn display(text: &str, tab: usize) -> String {
    expand_tabs(text.trim_end_matches(['\n', '\r']), tab).into_owned()
}

/// The largest line number the tool shows: the longer side's last line, or the preview's.
fn largest_number(regions: &[Region], preview: usize) -> usize {
    let side = regions
        .iter()
        .map(|region| match region {
            Region::Clean(lines) => lines.len(),
            Region::Conflict(conflict) => conflict.ours.len().max(conflict.theirs.len()),
        })
        .sum::<usize>();
    side.max(preview)
}

/// Syntax colors for `lines` of the file at `path` (none past the size limit, or for an
/// unknown language). Slow the first time: run it off the UI thread.
fn colors(path: &str, lines: &[String], dark: bool) -> Vec<Spans> {
    if lines.len() > MAX_HIGHLIGHT_LINES {
        return Vec::new();
    }
    syntax_for_path(path)
        .map(|syntax| {
            let texts: Vec<&str> = lines.iter().map(String::as_str).collect();
            highlight_lines(syntax, &texts, dark)
        })
        .unwrap_or_default()
}

/// Text styles for a line's syntax colors; ranges that don't fit `text` (colors of an older
/// text) are left out.
fn styles(text: &str, spans: Option<&Spans>) -> Vec<(Range<usize>, HighlightStyle)> {
    spans
        .into_iter()
        .flatten()
        .filter(|(range, _)| {
            range.start < range.end
                && range.end <= text.len()
                && text.is_char_boundary(range.start)
                && text.is_char_boundary(range.end)
        })
        .map(|(range, color)| {
            let style = HighlightStyle {
                color: Some(crate::diff_view::rgb(color).into()),
                ..HighlightStyle::default()
            };
            (range.clone(), style)
        })
        .collect()
}

/// The lines picked on each side when a line of a conflict is clicked: what the conflict
/// was resolved to, as picks (`sizes`: its ours and theirs line counts).
fn picks_from(resolution: Option<&Resolution>, sizes: (usize, usize)) -> (Vec<usize>, Vec<usize>) {
    let all = |count: usize| (0..count).collect::<Vec<_>>();
    match resolution {
        Some(Resolution::Picked { ours, theirs }) => (ours.clone(), theirs.clone()),
        Some(Resolution::Ours) => (all(sizes.0), Vec::new()),
        Some(Resolution::Theirs) => (Vec::new(), all(sizes.1)),
        Some(Resolution::Both | Resolution::BothTheirsFirst) => (all(sizes.0), all(sizes.1)),
        None | Some(Resolution::Base | Resolution::Custom(_)) => (Vec::new(), Vec::new()),
    }
}

/// Adds `line` to `list` (kept in order), or removes it when it's there.
fn toggle(list: &mut Vec<usize>, line: usize) {
    match list.binary_search(&line) {
        Ok(at) => {
            list.remove(at);
        }
        Err(at) => list.insert(at, line),
    }
}

/// A 22 px code row: a 3 px bar and a tint for the conflict's lines, the number (in a column
/// that wide), the text
/// (moved sideways by `offset` and cut, or wrapped: the bar then runs the whole height).
fn code_row(
    (number, number_width): (Option<usize>, Pixels),
    text: impl IntoElement,
    tint: Option<(Hsla, Hsla)>,
    wrap: bool,
    offset: Pixels,
    p: &Palette,
    m: &Metrics,
) -> Div {
    let z = |value: f32| px(value * m.zoom);
    code_scroll::code_row(wrap, m.conflict_line)
        .w_full()
        .text_color(p.fg)
        .when_some(tint, |row, (bg, _)| row.bg(bg))
        .child(
            div()
                .w(z(3.))
                .flex_shrink_0()
                // An explicit height would stop a wrapped row from stretching it.
                .when(!wrap, |bar| bar.h_full())
                .when_some(tint, |bar, (_, color)| bar.bg(color)),
        )
        .child(
            div()
                .w(number_width)
                .flex_shrink_0()
                .pr(z(12.))
                .text_right()
                .text_color(p.fg3)
                .child(number.map(|n| n.to_string()).unwrap_or_default()),
        )
        .child(code_scroll::code_text(text, wrap, offset))
}

/// The longest line, in characters, the panes show for conflict `index` (both sides, with
/// the clean lines around it).
fn pane_longest(regions: &[Region], index: usize, tab: usize) -> usize {
    [PaneSide::Ours, PaneSide::Theirs]
        .into_iter()
        .flat_map(|side| {
            let lines = pane_lines(regions, index, side, CONTEXT_LINES, tab);
            lines
                .before
                .into_iter()
                .chain(lines.conflict)
                .chain(lines.after)
        })
        .map(|line| columns(&line.text))
        .max()
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneSide {
    Ours,
    Theirs,
}

#[derive(Debug, Clone, PartialEq)]
struct PaneLine {
    /// The line's number in that side's version of the file.
    number: usize,
    text: SharedString,
}

#[derive(Debug, Default, PartialEq)]
struct PaneLines {
    before: Vec<PaneLine>,
    conflict: Vec<PaneLine>,
    after: Vec<PaneLine>,
}

/// What a pane shows of conflict `index` (counting conflicts only): the side's lines, and up
/// to `context` clean lines before and after them. Lines are numbered as in that side's
/// version of the file, where every conflict has that side's lines.
fn pane_lines(
    regions: &[Region],
    index: usize,
    side: PaneSide,
    context: usize,
    tab: usize,
) -> PaneLines {
    let mut out = PaneLines::default();
    let Some(target) = regions
        .iter()
        .enumerate()
        .filter(|(_, region)| matches!(region, Region::Conflict(_)))
        .nth(index)
        .map(|(ix, _)| ix)
    else {
        return out;
    };
    let line = |number: usize, text: &str| PaneLine {
        number,
        text: display(text, tab).into(),
    };
    let numbered = |lines: &[String], first: usize, skip: usize, take: usize| -> Vec<PaneLine> {
        lines
            .iter()
            .enumerate()
            .skip(skip)
            .take(take)
            .map(|(offset, text)| line(first + offset, text))
            .collect()
    };
    // The number of the next line in this side's version.
    let mut next = 1;
    for (ix, region) in regions.iter().enumerate().take(target + 2) {
        let (lines, clean): (&[String], bool) = match region {
            Region::Clean(lines) => (lines, true),
            Region::Conflict(conflict) => match side {
                PaneSide::Ours => (&conflict.ours, false),
                PaneSide::Theirs => (&conflict.theirs, false),
            },
        };
        if clean && ix + 1 == target {
            let skip = lines.len().saturating_sub(context);
            out.before = numbered(lines, next, skip, context);
        } else if ix == target {
            out.conflict = numbered(lines, next, 0, lines.len());
        } else if clean && ix == target + 1 {
            out.after = numbered(lines, next, 0, context);
        }
        next += lines.len();
    }
    out
}

#[cfg(test)]
mod tests {
    // Not `super::*`: GPUI's `test` macro would shadow `#[test]`.
    use super::{PaneSide, pane_lines, pane_longest, picks_from, styles, toggle};

    #[test]
    fn a_conflict_is_taken_as_base_only_when_it_has_one() {
        use gitweft_diff::conflict::Resolution;
        let with = super::ConflictView::picks(true);
        let without = super::ConflictView::picks(false);
        assert_eq!(with.len(), 5);
        assert_eq!(without.len(), 4);
        assert!(matches!(with[4].0, Resolution::Base));
        assert!(
            without
                .iter()
                .all(|(resolution, _)| !matches!(resolution, Resolution::Base))
        );
        assert!(matches!(without[0].0, Resolution::Ours));
    }

    #[test]
    fn the_panes_scroll_as_far_as_their_longest_line() {
        let regions = parse_conflicts(TEXT);
        // Conflict 0: "theirs1" is the longest of both sides and the context.
        assert_eq!(pane_longest(&regions, 0, 4), 7);
        // Conflict 1: "\ty2" shows as "    y2".
        assert_eq!(pane_longest(&regions, 1, 4), 6);
        assert_eq!(pane_longest(&regions, 2, 4), 0);
    }

    /// The number column fits the longer side's last line, or the preview's (#42).
    #[test]
    fn the_largest_number_is_the_longer_sides_last_line() {
        let regions = parse_conflicts(TEXT);
        // a–d, ours' two lines, e f, theirs' three lines, g.
        assert_eq!(super::largest_number(&regions, 0), 12);
        assert_eq!(super::largest_number(&regions, 40), 40);
    }
    use gitweft_diff::conflict::{Resolution, parse_conflicts};
    use gitweft_syntax::Rgb;

    #[test]
    fn a_click_starts_from_what_the_conflict_was_resolved_to() {
        let sizes = (3, 2);
        assert_eq!(picks_from(None, sizes), (vec![], vec![]));
        assert_eq!(
            picks_from(Some(&Resolution::Ours), sizes),
            (vec![0, 1, 2], vec![])
        );
        assert_eq!(
            picks_from(Some(&Resolution::Theirs), sizes),
            (vec![], vec![0, 1])
        );
        assert_eq!(
            picks_from(Some(&Resolution::BothTheirsFirst), sizes),
            (vec![0, 1, 2], vec![0, 1])
        );
        assert_eq!(
            picks_from(Some(&Resolution::Custom("x".into())), sizes),
            (vec![], vec![])
        );
        let picked = Resolution::Picked {
            ours: vec![2],
            theirs: vec![0],
        };
        assert_eq!(picks_from(Some(&picked), sizes), (vec![2], vec![0]));

        let mut list = vec![0, 2];
        toggle(&mut list, 1);
        assert_eq!(list, [0, 1, 2]);
        toggle(&mut list, 0);
        assert_eq!(list, [1, 2]);
    }

    #[test]
    fn colors_that_do_not_fit_the_text_are_left_out() {
        let spans = vec![
            (0..2, Rgb(1, 2, 3)),
            (2..9, Rgb(4, 5, 6)),
            (3..3, Rgb(0, 0, 0)),
        ];
        let fitting: Vec<_> = styles("let", Some(&spans))
            .into_iter()
            .map(|(range, _)| range)
            .collect();
        assert_eq!(fitting.len(), 1);
        assert_eq!(fitting[0], 0..2);
        // Not on a character boundary.
        let spans = vec![(0..1, Rgb(1, 2, 3))];
        assert!(styles("ü", Some(&spans)).is_empty());
        assert!(styles("x", None).is_empty());
    }

    const TEXT: &str = "a\nb\nc\nd\n<<<<<<< HEAD\nours1\nours2\n=======\ntheirs1\n>>>>>>> topic\n\
                        e\nf\n<<<<<<< HEAD\nx\n=======\ny1\n\ty2\ny3\n>>>>>>> topic\ng\n";

    fn numbers_and_texts(lines: &[super::PaneLine]) -> Vec<(usize, &str)> {
        lines
            .iter()
            .map(|line| (line.number, line.text.as_ref()))
            .collect()
    }

    #[test]
    fn a_side_is_numbered_in_its_own_version_with_context_around_it() {
        let regions = parse_conflicts(TEXT);
        let first = pane_lines(&regions, 0, PaneSide::Ours, 3, 4);
        assert_eq!(
            numbers_and_texts(&first.before),
            [(2, "b"), (3, "c"), (4, "d")]
        );
        assert_eq!(
            numbers_and_texts(&first.conflict),
            [(5, "ours1"), (6, "ours2")]
        );
        assert_eq!(numbers_and_texts(&first.after), [(7, "e"), (8, "f")]);

        // Theirs: its first conflict has one line, so everything after moves up by one.
        let second = pane_lines(&regions, 1, PaneSide::Theirs, 3, 4);
        assert_eq!(numbers_and_texts(&second.before), [(6, "e"), (7, "f")]);
        assert_eq!(
            numbers_and_texts(&second.conflict),
            [(8, "y1"), (9, "    y2"), (10, "y3")]
        );
        assert_eq!(numbers_and_texts(&second.after), [(11, "g")]);

        assert_eq!(
            pane_lines(&regions, 2, PaneSide::Ours, 3, 4),
            Default::default()
        );
    }
}
