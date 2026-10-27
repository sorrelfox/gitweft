//! Centre panel showing who last changed each line of a file (replaces the graph while
//! open). The text and its syntax colors arrive first; the blame fills in while git
//! streams it. Clicking a line's commit shows that commit in the graph; its menu blames
//! the parent commit (what the line was before) or ignores the commit.

use std::collections::HashMap;
use std::ops::Range;

use gitweft_git::ObjectId;
use gitweft_git::gix::bstr::BString;
use gitweft_git::read::{BlameCommit, BlameUpdate, LineMark};
use gitweft_ui::truncated::truncated;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, Sizable, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::code_keys::{
    self, NextHunk, NextPage, PreviousHunk, PreviousPage, ScrollLeft, ScrollRight,
};
use crate::code_scroll::{self, HScroll};
use crate::file_text::FileText;
use crate::find::{FindEvent, FindSlot, OpenFind, find_matches};
use crate::list_keys::{self, FirstRow, LastRow, NextRow, OpenRow, PreviousRow, RowMenu, Step};
use crate::palette::{PaletteCommand, view_command};
use crate::util::format_time;
use gitweft_git::display::columns;
use gitweft_ui::current::{mono_font, ui_font};
use gitweft_ui::widgets::centre_header;
use gitweft_ui::{AppIcon, LookExt, Metrics};

/// The commit column and the line number column, in design pixels.
const GUTTER_WIDTH: f32 = 320.;
const NUMBER_WIDTH: f32 = 48.;
pub(crate) const CONTEXT: &str = "BlameView";

actions!(blame, [CloseBlame]);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", CloseBlame, Some(CONTEXT)),
        KeyBinding::new("ctrl-f", OpenFind, Some(CONTEXT)),
    ]);
}

#[derive(Clone)]
pub enum BlameEvent {
    Close,
    /// A partial clone lacks versions of the file blame needs: download its history.
    FetchMissing,
    /// Show this commit in the graph.
    ShowCommit(ObjectId),
    /// Blame `path` at `parent`, the version before a commit changed the line, and show
    /// about line `line` (0-based) of it.
    BlameParent {
        parent: ObjectId,
        path: BString,
        line: usize,
    },
    /// Leave this commit out of the blame (in this repository, from now on).
    Ignore(ObjectId),
    /// Stop ignoring one commit chosen with [`BlameEvent::Ignore`], or all of them (`None`).
    StopIgnoring(Option<ObjectId>),
    /// Back to the blame shown before "Blame the parent".
    Back,
}

impl EventEmitter<BlameEvent> for BlameView {}

impl Focusable for BlameView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

pub struct BlameView {
    /// The file blamed and the revision (`None`: the working tree), to read the text again
    /// with new syntax colors after a light/dark switch.
    pub(crate) source: Option<(BString, Option<ObjectId>)>,
    title: String,
    subtitle: String,
    text: Option<FileText>,
    /// Commits reported so far; `owners` indexes into it.
    commits: Vec<BlameCommit>,
    commit_index: HashMap<ObjectId, usize>,
    /// Per line: the commit that last changed it, once the blame has reached it.
    owners: Vec<Option<usize>>,
    /// Per line: its number (1-based) in that commit's version of the file.
    orig_lines: Vec<u32>,
    /// Lines blamed so far.
    blamed: usize,
    /// Oldest and newest author time among the commits, for the age bar.
    time_range: Option<(i64, i64)>,
    done: bool,
    error: Option<String>,
    /// It failed because a partial clone lacks versions of the file.
    missing: bool,
    /// How many commits the user chose to ignore, and the repository's own list of
    /// commits to ignore (`blame.ignoreRevsFile`), as shown.
    ignored: usize,
    ignore_file: Option<String>,
    /// The commits the user chose to ignore, with their summaries (`None`: not a commit
    /// here), for the Stop ignoring menu.
    ignored_commits: Vec<(ObjectId, Option<String>)>,
    /// Per line (0-based): how an ignored commit shaped its blame.
    marks: HashMap<usize, LineMark>,
    /// A blame of a later version is waiting under this one ("Back").
    can_go_back: bool,
    /// The line to show at the top once the text is in.
    pending_scroll: Option<usize>,
    scroll: UniformListScrollHandle,
    /// The rows when long lines wrap (their heights differ).
    list: ListState,
    /// Whether the rows were drawn wrapped last.
    wrapped: bool,
    hscroll: HScroll,
    /// The longest line, in characters.
    longest: usize,
    /// The number column's width, for the last line's number (from the last frame).
    number_width: Pixels,
    focus: FocusHandle,
    /// Take keyboard focus on the first frame, so Esc works right away.
    focus_on_render: bool,
    /// Whether it had the keyboard focus when last drawn (the cursor shows then).
    focused: bool,
    /// The line the keys are on.
    cursor: Option<usize>,
    /// Ctrl+F's bar.
    find: FindSlot,
}

impl BlameView {
    pub fn new(title: String, subtitle: String, cx: &mut Context<Self>) -> Self {
        Self {
            source: None,
            title,
            subtitle,
            text: None,
            commits: Vec::new(),
            commit_index: HashMap::new(),
            owners: Vec::new(),
            orig_lines: Vec::new(),
            blamed: 0,
            time_range: None,
            done: false,
            error: None,
            missing: false,
            ignored: 0,
            ignore_file: None,
            ignored_commits: Vec::new(),
            marks: HashMap::new(),
            can_go_back: false,
            pending_scroll: None,
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
            find: FindSlot::default(),
        }
    }

    /// Ctrl+F: the find bar.
    fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.find.open(window, cx, Self::on_find);
    }

    fn on_find(&mut self, event: &FindEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            FindEvent::Changed => self.refind(cx),
            FindEvent::Show => {
                if let Some(found) = self.find.current(cx) {
                    self.put_cursor(found.row, cx);
                    if let Some(text) = &self.text
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
        let matches = match (self.find.query(cx), &self.text) {
            (Some((query, case)), Some(text)) => find_matches(
                text.lines.iter().map(|line| line.as_ref()).enumerate(),
                &query,
                case,
            ),
            _ => Vec::new(),
        };
        self.find.set_matches(matches, self.cursor.unwrap_or(0), cx);
        cx.notify();
    }

    /// What the header says about ignored commits and going back.
    pub fn set_context(&mut self, ignored: usize, can_go_back: bool, scroll_to: Option<usize>) {
        self.ignored = ignored;
        self.can_go_back = can_go_back;
        self.pending_scroll = scroll_to;
    }

    /// The repository's `blame.ignoreRevsFile`, when it's set (git applies it itself).
    pub fn set_ignore_file(&mut self, file: Option<String>, cx: &mut Context<Self>) {
        self.ignore_file = file;
        cx.notify();
    }

    /// The ignored commits with their summaries, for the Stop ignoring menu.
    pub fn set_ignored_commits(
        &mut self,
        commits: Vec<(ObjectId, Option<String>)>,
        cx: &mut Context<Self>,
    ) {
        self.ignored_commits = commits;
        cx.notify();
    }

    /// The first line shown, to come back to it (`row_height`: the metrics' code line).
    pub fn top_line(&self, row_height: Pixels) -> usize {
        if self.wrapped {
            self.list.logical_scroll_top().item_ix
        } else {
            code_scroll::uniform_top(&self.scroll, row_height)
        }
    }

    pub fn set_text(&mut self, text: FileText, cx: &mut Context<Self>) {
        if self.owners.len() < text.lines.len() {
            self.owners.resize(text.lines.len(), None);
            self.orig_lines.resize(text.lines.len(), 0);
        }
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
        if let Some(line) = self.pending_scroll.take() {
            let line = line.min(text.lines.len().saturating_sub(1));
            code_scroll::scroll_to_top(self.wrapped, &self.scroll, &self.list, line);
        }
        self.text = Some(text);
        // New text: an open find bar's matches are found in it again.
        self.refind(cx);
        cx.notify();
    }

    pub fn apply(&mut self, updates: Vec<BlameUpdate>, cx: &mut Context<Self>) {
        for update in updates {
            match update {
                BlameUpdate::Commit(commit) => {
                    if !commit.id.is_null() {
                        let time = commit.author.time;
                        self.time_range = Some(match self.time_range {
                            Some((oldest, newest)) => (oldest.min(time), newest.max(time)),
                            None => (time, time),
                        });
                    }
                    self.commit_index.insert(commit.id, self.commits.len());
                    self.commits.push(commit);
                }
                BlameUpdate::Chunk(chunk) => {
                    let Some(&owner) = self.commit_index.get(&chunk.commit) else {
                        continue;
                    };
                    let start = chunk.final_line.saturating_sub(1) as usize;
                    let end = start + chunk.lines as usize;
                    if self.owners.len() < end {
                        self.owners.resize(end, None);
                        self.orig_lines.resize(end, 0);
                    }
                    self.owners[start..end].fill(Some(owner));
                    for (offset, orig) in self.orig_lines[start..end].iter_mut().enumerate() {
                        *orig = chunk.orig_line + offset as u32;
                    }
                    self.blamed += chunk.lines as usize;
                }
                BlameUpdate::Marks(marks) => {
                    self.marks = marks
                        .into_iter()
                        .map(|(line, mark)| (line.saturating_sub(1) as usize, mark))
                        .collect();
                }
            }
        }
        cx.notify();
    }

    /// The blame finished, or failed with `Err(message)`.
    pub fn finish(&mut self, result: Result<(), String>, cx: &mut Context<Self>) {
        self.done = true;
        self.error = result.err();
        cx.notify();
    }

    /// The blame (or the file's text) stopped at versions this partial clone lacks, which
    /// reads don't download: says so, with "Fetch missing contents".
    pub fn finish_missing(&mut self, cx: &mut Context<Self>) {
        self.done = true;
        self.missing = true;
        self.error = Some(t!("blame.missing_objects").into_owned());
        cx.notify();
    }

    /// Takes the keys again once a view shown on top of it closes.
    pub fn refocus(&mut self, cx: &mut Context<Self>) {
        self.focus_on_render = true;
        cx.notify();
    }

    fn close(&mut self, _: &CloseBlame, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(BlameEvent::Close);
    }

    fn owner(&self, line: usize) -> Option<usize> {
        self.owners.get(line).copied().flatten()
    }

    fn line_count(&self) -> usize {
        self.text.as_ref().map_or(0, |text| text.lines.len())
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

    /// Alt+Down / Alt+Up: the first line of the next or previous run of lines from one
    /// commit, shown at the top.
    fn jump_block(&mut self, forward: bool, cx: &mut Context<Self>) {
        let same = |a: usize, b: usize| self.owner(a) == self.owner(b);
        let Some(ix) = code_keys::block_row(self.line_count(), self.cursor, forward, same) else {
            return;
        };
        self.cursor = Some(ix);
        code_scroll::scroll_to_top(self.wrapped, &self.scroll, &self.list, ix);
        cx.notify();
    }

    /// Left / Right: the lines scrolled sideways.
    fn scroll_sideways(&mut self, left: bool, window: &mut Window, cx: &mut Context<Self>) {
        let step = code_keys::sideways_step(window, cx);
        if self.hscroll.scroll_by(if left { step } else { -step }) {
            cx.notify();
        }
    }

    /// The commit of the cursor's line, when the blame has reached it and it's committed.
    fn cursor_commit(&self) -> Option<&BlameCommit> {
        let commit = &self.commits[self.owner(self.cursor?)?];
        (!commit.id.is_null()).then_some(commit)
    }

    /// Enter: the cursor line's commit in the graph, as a click on its gutter does.
    fn open_row(&mut self, cx: &mut Context<Self>) {
        if let Some(commit) = self.cursor_commit() {
            cx.emit(BlameEvent::ShowCommit(commit.id));
        }
    }

    /// Shift+F10: the gutter's menu for the cursor's line, in the palette (Blame the parent
    /// only where the commit has one; the gutter's menu shows it greyed out instead).
    fn row_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((detail, items)) = self.cursor_items() else {
            return;
        };
        let commands = list_keys::event_commands(
            &cx.entity().downgrade(),
            &t!("palette.group.blame"),
            &detail,
            items,
        );
        crate::palette::open(commands, window, cx);
    }

    /// The cursor line's commit (as the palette's detail) and what it offers: Show in
    /// graph, Blame the parent where the commit has one, Ignore.
    fn cursor_items(&self) -> Option<(String, Vec<(String, BlameEvent)>)> {
        let (ix, commit) = (self.cursor?, self.cursor_commit()?);
        let id = commit.id;
        let detail = format!("{} {}", id.to_hex_with_len(8), commit.summary);
        let mut items = vec![(
            t!("history.show_in_graph").into_owned(),
            BlameEvent::ShowCommit(id),
        )];
        if let Some(parent) = self.parent_of_line(ix) {
            items.push((t!("blame.parent").into_owned(), parent));
        }
        items.push((t!("blame.ignore").into_owned(), BlameEvent::Ignore(id)));
        Some((detail, items))
    }

    /// The blame's commands for the palette: the cursor line's items, then Back to
    /// the later blame (when there is one) and Close.
    pub(crate) fn palette_commands(&self, this: &WeakEntity<Self>) -> Vec<PaletteCommand> {
        let group = t!("palette.group.blame").into_owned();
        let mut commands = match self.cursor_items() {
            Some((detail, items)) => list_keys::event_commands(this, &group, &detail, items),
            None => Vec::new(),
        };
        let group: SharedString = group.into();
        if self.can_go_back {
            commands.push(view_command(this, &group, t!("blame.back"), |_, _, cx| {
                cx.emit(BlameEvent::Back)
            }));
        }
        commands.push(view_command(
            this,
            &group,
            t!("palette.blame_close"),
            |_, _, cx| cx.emit(BlameEvent::Close),
        ));
        commands
    }

    /// "Blame the parent" for line `ix`: its commit's parent, the file's path there, and
    /// about where the line was (its number in the commit, which the parent's differs
    /// from only by what the commit added or removed above it).
    fn parent_of_line(&self, ix: usize) -> Option<BlameEvent> {
        let commit = &self.commits[self.owner(ix)?];
        let (parent, path) = commit.previous.clone()?;
        let line = self
            .orig_lines
            .get(ix)
            .map_or(0, |orig| orig.saturating_sub(1)) as usize;
        Some(BlameEvent::BlameParent { parent, path, line })
    }

    /// The age bar's color: the accent, stronger for newer commits; the modified color for
    /// uncommitted lines.
    fn age_color(&self, commit: &BlameCommit, cx: &App) -> Option<Hsla> {
        let p = cx.palette();
        if commit.id.is_null() {
            return Some(p.modified);
        }
        let (oldest, newest) = self.time_range?;
        let age = if newest > oldest {
            (commit.author.time - oldest) as f32 / (newest - oldest) as f32
        } else {
            1.
        };
        Some(p.acc.opacity(0.15 + 0.85 * age))
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

    /// Line `ix`: its commit, number and text; `wrap`: as high as its wrapped text.
    fn render_line(&self, ix: usize, wrap: bool, cx: &mut Context<Self>) -> AnyElement {
        let Some(text) = &self.text else {
            return div().into_any_element();
        };
        let (p, m) = (*cx.palette(), *cx.metrics());
        code_scroll::code_row(wrap, m.code_line)
            .w_full()
            .child(self.render_gutter(ix, wrap, p.fg2, p.hover, &m, cx))
            .child(
                div()
                    .w(self.number_width)
                    .flex_shrink_0()
                    .pr_2()
                    .text_right()
                    .text_color(p.fg3)
                    .child((ix + 1).to_string()),
            )
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

    /// The commit column: an age bar, and on the first line of each run of lines from one
    /// commit its date, author and summary. Clicking it shows the commit; its menu blames
    /// the parent or ignores the commit.
    fn render_gutter(
        &self,
        ix: usize,
        wrap: bool,
        muted: Hsla,
        hover: Hsla,
        m: &Metrics,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let owner = self.owner(ix);
        let commit = owner.map(|owner| &self.commits[owner]);
        let first_of_run = ix == 0 || self.owner(ix - 1) != owner;
        let gutter = h_flex()
            .id(("blame-gutter", ix))
            .w(px(GUTTER_WIDTH * m.zoom))
            // Wrapped rows stretch it instead: a set height would stop that.
            .when(!wrap, |this| this.h_full())
            .flex_shrink_0()
            .gap_2()
            .pr_2()
            // Wrapped: the label on the first line, the bar down the whole row.
            .when(wrap, |this| this.items_stretch())
            // The row menu opens inside the gutter and takes its font: the interface's,
            // with the label in the code font.
            .font_family(ui_font())
            .text_color(muted);
        let Some(commit) = commit else {
            return gutter
                .child(
                    div()
                        .w(m.dp(3.))
                        .when(!wrap, |this| this.h_full())
                        .flex_shrink_0(),
                )
                .into_any_element();
        };
        let bar = self.age_color(commit, cx);
        // git's own signs: `?` passed through an ignored commit, `*` stayed with one.
        let mark = self.marks.get(&ix).map(|mark| match mark {
            LineMark::Ignored => (t!("blame.mark_ignored"), t!("blame.mark_ignored_tip")),
            LineMark::Unblamable => (t!("blame.mark_unblamable"), t!("blame.mark_unblamable_tip")),
        });
        let mark_cell = (!self.marks.is_empty()).then(|| {
            div()
                .id(("blame-mark", ix))
                .w(px(10. * m.zoom))
                .flex_shrink_0()
                .font_family(mono_font())
                .text_color(cx.palette().modified)
                .when_some(mark, |this, (sign, tip)| {
                    let tip = tip.into_owned();
                    this.child(sign.into_owned())
                        .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                })
        });
        let label = first_of_run.then(|| {
            if commit.id.is_null() {
                t!("blame.uncommitted").into_owned()
            } else {
                format!(
                    "{}  {}  {}",
                    format_time(commit.author.time),
                    commit.author.name,
                    commit.summary
                )
            }
        });
        let id = commit.id;
        let gutter = gutter
            .child(
                div()
                    .w(m.dp(3.))
                    .when(!wrap, |this| this.h_full())
                    .flex_shrink_0()
                    .when_some(bar, |this, bar| this.bg(bar)),
            )
            .children(mark_cell)
            .when_some(label, |this, label| {
                this.child(truncated("author", label).font_family(mono_font()))
            });
        if id.is_null() {
            return gutter.into_any_element();
        }
        let view = cx.entity().downgrade();
        let parent = self.parent_of_line(ix).is_some();
        gutter
            .cursor_pointer()
            .hover(move |style| style.bg(hover))
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(BlameEvent::ShowCommit(id))))
            .context_menu(move |menu, _, _| {
                let item = |label: String, event: fn(&mut BlameView, ObjectId, usize) -> Option<BlameEvent>| {
                    let view = view.clone();
                    PopupMenuItem::new(label).on_click(move |_, _, cx| {
                        view.update(cx, |this, cx| {
                            if let Some(event) = event(this, id, ix) {
                                cx.emit(event);
                            }
                        })
                        .ok();
                    })
                };
                menu.item(item(t!("history.show_in_graph").into_owned(), |_, id, _| {
                    Some(BlameEvent::ShowCommit(id))
                }))
                .item(
                    item(t!("blame.parent").into_owned(), |this, _, ix| {
                        this.parent_of_line(ix)
                    })
                    .disabled(!parent),
                )
                .item(item(t!("blame.ignore").into_owned(), |_, id, _| {
                    Some(BlameEvent::Ignore(id))
                }))
            })
            .into_any_element()
    }
}

impl BlameView {
    /// "Stop ignoring": a menu of the ignored commits, each shown again on its own, then
    /// all of them at once.
    fn render_ignored_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let commits = self.ignored_commits.clone();
        Button::new("blame-show-ignored")
            .small()
            .ghost()
            .icon(Icon::from(AppIcon::ChevronDown))
            .label(t!("blame.show_ignored").into_owned())
            .dropdown_menu_with_anchor(Anchor::TopRight, move |mut menu, _, _| {
                let item = |label: String, id: Option<ObjectId>| {
                    let view = view.clone();
                    PopupMenuItem::new(label).on_click(move |_, _, cx| {
                        view.update(cx, |_, cx| cx.emit(BlameEvent::StopIgnoring(id)))
                            .ok();
                    })
                };
                for (id, summary) in &commits {
                    let sha = id.to_hex_with_len(8).to_string();
                    let label = match summary {
                        Some(summary) => {
                            t!("blame.stop_ignoring_commit", sha = sha, summary = summary)
                        }
                        None => t!("blame.stop_ignoring_missing", sha = sha),
                    };
                    menu = menu.item(item(label.into_owned(), Some(*id)));
                }
                if commits.len() != 1 {
                    if !commits.is_empty() {
                        menu = menu.separator();
                    }
                    menu = menu.item(item(t!("blame.stop_ignoring_all").into_owned(), None));
                }
                menu
            })
    }
}

impl Render for BlameView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.focus_on_render) {
            self.focus.focus(window, cx);
        }
        self.focused = self.focus.contains_focused(window, cx);
        let (p, m) = (*cx.palette(), *cx.metrics());
        let (muted, danger) = (p.fg3, p.del);
        let wrap = code_scroll::wrapping(cx);
        if wrap != self.wrapped {
            if let Some(text) = &self.text {
                let lines = text.lines.len();
                code_scroll::switch_mode(wrap, &self.scroll, &self.list, lines, m.code_line);
            }
            self.wrapped = wrap;
        }
        // Wide enough for the last line's number, with the column's `pr_2`.
        let lines = self.text.as_ref().map_or(0, |text| text.lines.len());
        self.number_width = code_scroll::number_width(
            window,
            m.text.mono,
            lines,
            code_scroll::pr_2(&m),
            px(NUMBER_WIDTH * m.zoom),
        );
        if !wrap {
            let content = code_scroll::text_width(window, m.text.mono, self.longest);
            let fixed = px(GUTTER_WIDTH * m.zoom) + self.number_width;
            self.hscroll
                .update(content, self.hscroll.viewport() - fixed);
        }

        let total = self.text.as_ref().map_or(0, |text| text.lines.len());
        let status = match (&self.error, self.done) {
            (Some(error), _) if self.text.is_some() => Some((error.clone(), danger)),
            (None, false) if self.text.is_some() => Some((
                tn!(
                    "blame.progress",
                    count = total,
                    done = self.blamed.min(total)
                )
                .into_owned(),
                muted,
            )),
            _ => None,
        };
        let mut notes = Vec::new();
        if let Some(file) = &self.ignore_file {
            notes.push(t!("blame.ignore_file", file = file).into_owned());
        }
        if self.ignored > 0 {
            notes.push(tn!("blame.ignored", count = self.ignored).into_owned());
        }
        if !self.marks.is_empty() {
            notes.push(tn!("blame.marked", count = self.marks.len()).into_owned());
        }
        let subtitle = std::iter::once(self.subtitle.clone())
            .chain(notes)
            .collect::<Vec<_>>()
            .join(" · ");
        let header = centre_header(self.title.clone(), subtitle, &p, &m)
            .when_some(status, |this, (text, color)| {
                this.child(div().text_size(m.text.xs).text_color(color).child(text))
            })
            .when(self.ignored > 0, |this| {
                this.child(self.render_ignored_menu(cx))
            })
            .when(self.missing, |this| {
                this.child(
                    Button::new("fetch-missing-blame")
                        .debug_selector(|| "fetch-missing-blame".into())
                        .small()
                        .primary()
                        .label(t!("diff.fetch_missing").into_owned())
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(BlameEvent::FetchMissing))),
                )
            })
            .when(self.can_go_back, |this| {
                this.child(
                    Button::new("blame-back")
                        .small()
                        .ghost()
                        .label(t!("blame.back").into_owned())
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(BlameEvent::Back))),
                )
            })
            .child(code_scroll::wrap_button(wrap))
            .child(
                Button::new("close-blame")
                    .small()
                    .ghost()
                    .label(t!("diff.close").into_owned())
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(BlameEvent::Close))),
            );

        let body = match &self.text {
            Some(text) => {
                let lines = if wrap {
                    list(
                        self.list.clone(),
                        cx.processor(|this, ix, _, cx| this.render_row(ix, true, cx)),
                    )
                    .size_full()
                    .into_any_element()
                } else {
                    uniform_list(
                        "blame-lines",
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
            None => {
                let (message, color) = match &self.error {
                    Some(error) => (error.clone(), danger),
                    None => (t!("repo.loading").into_owned(), muted),
                };
                v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .text_color(color)
                    .child(message)
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
            .on_action(cx.listener(|this, _: &NextHunk, _, cx| this.jump_block(true, cx)))
            .on_action(cx.listener(|this, _: &PreviousHunk, _, cx| this.jump_block(false, cx)))
            .on_action(cx.listener(|this, _: &OpenRow, _, cx| this.open_row(cx)))
            .on_action(cx.listener(|this, _: &RowMenu, window, cx| this.row_menu(window, cx)))
            .child(header)
            .children(self.find.element(cx))
            .child(body)
    }
}
