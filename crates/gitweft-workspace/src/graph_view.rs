//! The commit graph: a virtualized list whose rows paint their own slice of the lanes,
//! with resizable columns and incremental search.

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gitweft_core::repo::{GraphModel, RefKind, RefLabel};
use gitweft_core::settings::GraphSettings;
use gitweft_git::ObjectId;
use gitweft_git::bisect::BisectMark;
use gitweft_git::read::{CommitMeta, EntryKind, SignatureStatus, Status, TipFilter, WalkOptions};
use gitweft_git::rewrite::ResetMode;
use gitweft_graph::{Connector, ConnectorKind, Node, PALETTE_SIZE, Span};
use gitweft_ui::palette::LANES;
use gitweft_ui::{AppIcon, LookExt, Metrics, Palette, SettingsStore};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::list_keys::{self, Step};
use metas::Metas;

mod metas;
use crate::util::{format_time, join_list};
use gitweft_ui::current::mono_font;
use gitweft_ui::truncated::truncated;

// Sizes in design pixels (the Graphite design at 100 % zoom); `Metrics::zoom` scales them.
/// Where lane 0's centre sits in the graph column.
const LANE_INSET: f32 = 12.;
/// The graph column starts at most this wide; wider graphs are clipped until resized.
const DEFAULT_MAX_GRAPH_WIDTH: f32 = 132.;
const DEFAULT_REFS_WIDTH: f32 = 200.;
pub(crate) const DEFAULT_AUTHOR_WIDTH: f32 = 150.;
pub(crate) const DEFAULT_DATE_WIDTH: f32 = 140.;
pub(crate) const SHA_WIDTH: f32 = 90.;
/// Space between columns; in the header it doubles as the resize handle.
const SEPARATOR: f32 = 8.;
const RIGHT_PADDING: f32 = 8.;
pub(crate) const MIN_COLUMN: f32 = 40.;
const MAX_COLUMN: f32 = 640.;
/// The message column keeps at least this width; the other columns shrink first.
pub(crate) const MIN_MESSAGE_WIDTH: f32 = 160.;
const PILL_MAX_WIDTH: f32 = 170.;
/// A pill after the first is shown only with at least this many characters of its name.
const MIN_PILL_CHARS: usize = 4;
/// Left padding of the Branch / Tag cell, in every row (the header's label is 2 px further in).
const REFS_INSET: f32 = 10.;
const SEARCH_MAX_WIDTH: f32 = 420.;
const SEARCH_HEIGHT: f32 = 30.;
/// The WIP node's dashed outline and the thin ring around the HEAD node.
const THIN_STROKE: f32 = 1.5;
/// Lanes beyond this are never drawn.
const MAX_DRAWN_LANES: u16 = 64;
/// Rows moved by PageUp/PageDown.
const PAGE_ROWS: usize = 20;
/// Ref labels shown per row before collapsing the rest into "+N".
const MAX_PILLS: usize = 2;

pub(crate) const CONTEXT: &str = "GraphView";

actions!(
    graph,
    [
        SelectNext,
        SelectPrevious,
        SelectFirst,
        SelectLast,
        ExtendNext,
        ExtendPrevious,
        ExtendFirst,
        ExtendLast,
        PageDown,
        PageUp,
        FocusSearch
    ]
);

pub fn bind_keys(cx: &mut App) {
    let context = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("down", SelectNext, context),
        KeyBinding::new("j", SelectNext, context),
        KeyBinding::new("up", SelectPrevious, context),
        KeyBinding::new("k", SelectPrevious, context),
        KeyBinding::new("home", SelectFirst, context),
        KeyBinding::new("end", SelectLast, context),
        KeyBinding::new("shift-down", ExtendNext, context),
        KeyBinding::new("shift-up", ExtendPrevious, context),
        KeyBinding::new("shift-home", ExtendFirst, context),
        KeyBinding::new("shift-end", ExtendLast, context),
        KeyBinding::new("pagedown", PageDown, context),
        KeyBinding::new("pageup", PageUp, context),
        KeyBinding::new("ctrl-f", FocusSearch, context),
    ]);
}

/// Where a Shift range goes: from `anchor` to `end` moved by `to` within `rows`
/// rows. Returns the new end and the rows between the two ends.
fn extend_range(
    anchor: usize,
    end: usize,
    to: Step,
    rows: usize,
) -> (usize, std::ops::RangeInclusive<usize>) {
    let last = rows.saturating_sub(1);
    let end = match to {
        Step::Next => (end + 1).min(last),
        Step::Previous => end.saturating_sub(1),
        Step::First => 0,
        Step::Last => last,
    };
    (end, anchor.min(end)..=anchor.max(end))
}

pub enum GraphEvent {
    Selected(ObjectId),
    /// The "Working changes" row was selected.
    WipSelected,
    /// Row menu: "Create branch here…".
    CreateBranchAt(ObjectId),
    /// Row menu: check out the commit itself (detached HEAD).
    CheckoutCommit(ObjectId),
    /// Row menu: check out a local branch that points at the row's commit.
    CheckoutBranch(String),
    /// Row menu: "Create tag here…".
    CreateTagAt(ObjectId),
    /// Row menu: "Browse files".
    BrowseFiles(ObjectId),
    /// Row menu: cherry-pick the commit onto the checked-out branch.
    CherryPick(RowCommit),
    /// Row menu: apply the commit's changes, staged, without committing.
    CherryPickNoCommit(RowCommit),
    /// Rows on screen whose signature marks aren't known yet: read them (they arrive in
    /// the cache the view shares with the workspace).
    NeedSignatures(Vec<ObjectId>),
    /// Row menu: add a commit that undoes this one.
    Revert(RowCommit),
    /// Row menu: move the checked-out branch to the commit.
    Reset(RowCommit, ResetMode),
    /// Row menu: edit the commits after this one (interactive rebase).
    InteractiveRebase(RowCommit),
    /// Row menu: edit this commit and the ones after it (from the first commit on, it
    /// includes that one too).
    InteractiveRebaseFrom(RowCommit),
    /// Commit rows were dropped on the checked-out commit: cherry-pick them, oldest first
    /// (the dragged row, or every selected one when it was among them).
    DropCommits(Vec<RowCommit>),
    /// Several commits are selected (Ctrl+click, Shift+click), oldest first.
    MultiSelected(Vec<RowCommit>),
    /// Row menu: cherry-pick the selected commits, oldest first.
    CherryPickMany(Vec<RowCommit>),
    /// Row menu: write the selected commits as numbered patch files, oldest first.
    SavePatches(Vec<RowCommit>),
    /// A branch label was dropped on the checked-out commit: merge it (full ref name,
    /// and the name shown).
    DropBranch {
        full_name: String,
        shown: String,
    },
    /// A branch label was dropped on a commit with local branches (`targets`, their short
    /// names): merge into one of them or rebase onto it.
    DropBranchOnBranch {
        full_name: String,
        shown: String,
        targets: Vec<String>,
    },
    /// The view menu: show a hidden ref again (an entry of the tip filter, as stored).
    ShowHidden(String),
    /// The graph column was dragged to this width (design pixels), or `None`: fit it to
    /// the lanes again. Kept for the repository.
    GraphWidth(Option<f32>),
    /// Search the commits' changes (`git log -S`/`-G`); results come back through
    /// [`GraphView::add_change_results`].
    SearchChanges(gitweft_git::read::Pickaxe),
    /// Stop the running change search.
    CancelChangeSearch,
    /// Row menu: start a bisect with this commit as bad or good, or mark it while one runs.
    Bisect(ObjectId, BisectMark),
    /// Row menu: write the commit as a patch file (`git format-patch`).
    SavePatch(ObjectId),
    /// Row menu: save the commit's files as a zip or tar.gz (`git archive`).
    Archive(ObjectId),
    /// Shift+F10: the selected commit's row menu, in the palette.
    RowMenu,
    /// A double click on a commit: check out its branch, or the commit itself.
    Activated(ObjectId),
    /// Row menu: select where the commit's history and HEAD's meet.
    SelectMergeBase(ObjectId),
    /// Row menu: compare the commit with a branch or tag chosen in the palette.
    CompareWith(ObjectId),
    /// The graph's view menu: walk the history with other options (the graph reloads).
    SetWalk(WalkOptions),
    /// The view menu's All branches (false) or Current branch only (true); either ends a solo.
    SetScope(bool),
    /// The view menu's "Show every branch": nothing hidden, no solo, all branches.
    ShowEveryBranch,
    /// Row menu: reword the commit, squash it into its parent or drop it (a rebase of the
    /// commits after it, without the editor).
    QuickRebase(RowCommit, QuickEdit),
    /// Row menu: commit what's staged as `fixup! <the commit's summary>`.
    CreateFixup(RowCommit),
}

/// The graph's quick rebase actions on one commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuickEdit {
    Reword,
    /// Folded into the parent, both messages kept.
    SquashIntoParent,
    /// Folded into the parent, which keeps its own message.
    FixupIntoParent,
    Drop,
}

/// The quick rebase actions with their labels, for the row menu and the palette.
pub(crate) fn quick_edits() -> [(QuickEdit, String); 4] {
    [
        (QuickEdit::Reword, t!("rebase.quick_reword").into_owned()),
        (
            QuickEdit::SquashIntoParent,
            t!("rebase.quick_squash").into_owned(),
        ),
        (
            QuickEdit::FixupIntoParent,
            t!("rebase.quick_fixup_parent").into_owned(),
        ),
        (QuickEdit::Drop, t!("rebase.quick_drop").into_owned()),
    ]
}

/// The commit a row-menu action works on.
#[derive(Debug, Clone)]
pub struct RowCommit {
    pub id: ObjectId,
    /// A merge commit is picked and reverted against its first parent.
    pub merge: bool,
    /// For labels and notifications; the short id until the row's text has loaded.
    pub summary: String,
}

/// Counts shown in the "Working changes" row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WipSummary {
    pub staged: usize,
    pub unstaged: usize,
    pub conflicts: usize,
}

impl WipSummary {
    /// `None` for a clean working tree (no row is shown then).
    pub fn from_status(status: &Status) -> Option<Self> {
        (!status.is_clean()).then(|| Self {
            staged: status.staged().count(),
            unstaged: status
                .unstaged()
                .filter(|e| e.kind != EntryKind::Conflicted)
                .count(),
            conflicts: status.conflicts().count(),
        })
    }
}

impl EventEmitter<GraphEvent> for GraphView {}

/// Columns the user can resize by dragging the separator in the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Column {
    Refs,
    Graph,
    Author,
    Date,
}

/// Drag payload of a column resize handle.
struct ColumnDrag(Column);

/// Column widths in design pixels. Branch / Tag, Author and Date come from the settings;
/// the graph column's width depends on the repository, so it stays with the view.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Columns {
    refs: f32,
    /// `None` = sized to the lanes (up to [`DEFAULT_MAX_GRAPH_WIDTH`]).
    graph: Option<f32>,
    author: f32,
    date: f32,
}

impl Columns {
    fn from_settings(settings: &GraphSettings, graph: Option<f32>) -> Self {
        Self {
            refs: settings.refs_width.unwrap_or(DEFAULT_REFS_WIDTH),
            graph,
            author: settings.author_width.unwrap_or(DEFAULT_AUTHOR_WIDTH),
            date: settings.date_width.unwrap_or(DEFAULT_DATE_WIDTH),
        }
    }
}

/// The columns as drawn in this frame: zoomed, and `None` when hidden in the settings.
#[derive(Debug, Clone, Copy)]
struct ColumnLayout {
    refs: Pixels,
    graph: Pixels,
    author: Option<Pixels>,
    date: Option<Pixels>,
    sha: Option<Pixels>,
    separator: Pixels,
    right: Pixels,
    min_message: Pixels,
    /// Branch / Tag and Author shrink in a narrow panel, but not below this.
    min_column: Pixels,
}

/// A dragged width in design pixels, within the limits.
fn design_width(width: Pixels, zoom: f32) -> f32 {
    (f32::from(width) / zoom)
        .clamp(MIN_COLUMN, MAX_COLUMN)
        .round()
}

/// A change search (pickaxe): the query as typed, the rows found so far (ascending) and
/// whether git is still searching.
struct ChangeSearch {
    query: String,
    rows: Vec<usize>,
    running: bool,
    error: Option<String>,
}

pub struct GraphView {
    graph: Arc<GraphModel>,
    /// Metadata for rows `0..metas.len()`; later rows show placeholders until loaded.
    metas: Metas,
    selected: Option<usize>,
    scroll: UniformListScrollHandle,
    focus: FocusHandle,
    columns: Columns,
    /// Columns resized by the drag in progress; their widths are saved when the mouse is
    /// released.
    resized: Vec<Column>,
    /// Each commit's ref labels as drawn (their text is built once, not per frame).
    pills: HashMap<ObjectId, Rc<[Pill]>>,
    /// The Branch / Tag cell's width as last drawn (flex shrinks it below its column's
    /// width in a narrow panel), measured in the header; zero before the first frame.
    refs_drawn: Rc<Cell<Pixels>>,
    search: Entity<InputState>,
    /// Lower-cased search query; empty when not searching.
    query: String,
    /// Rows matching `query` (or found by the change search), ascending.
    matches: Vec<usize>,
    _search_events: Subscription,
    /// Search the commits' changes instead of their messages, authors and refs.
    search_changes: bool,
    /// The change search run last.
    change_search: Option<ChangeSearch>,
    /// Uncommitted changes; `None` when the working tree is clean.
    wip: Option<WipSummary>,
    wip_selected: bool,
    /// Row of the checked-out commit (the WIP node sits in its lane).
    head_row: Option<usize>,
    /// While bisecting: the commits marked so far. `None` when no bisect runs.
    bisect: Option<std::collections::HashMap<ObjectId, BisectMark>>,
    /// A shallow clone's boundary: the commits whose parents aren't here.
    shallow: std::collections::HashSet<ObjectId>,
    /// How this graph's history was walked (the view menu shows it).
    walk: WalkOptions,
    /// The branches and tags left out, or the one soloed (the view menu shows it).
    tips: TipFilter,
    /// Every selected row while several are (Ctrl+click, Shift+click), `selected` among
    /// them; empty for a single selection.
    picked: std::collections::BTreeSet<usize>,
    /// The moving end of a Shift range (Shift+click, Shift+arrows); the selected row is its
    /// fixed end.
    range_end: Option<usize>,
    /// Signature marks by commit (`None`: unsigned), shared with the workspace so that a
    /// reload of the graph keeps them.
    signatures: SignatureCache,
    /// Commits asked about already (their answer may still be on its way).
    signatures_asked: std::collections::HashSet<ObjectId>,
    /// Local branches checked out in other worktrees: short name → folder name.
    worktrees: HashMap<String, SharedString>,
}

/// Signature marks by commit, `None` for an unsigned one.
pub type SignatureCache = Rc<std::cell::RefCell<HashMap<ObjectId, Option<SignatureStatus>>>>;

impl GraphView {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        graph: Arc<GraphModel>,
        metas: Vec<CommitMeta>,
        search: Entity<InputState>,
        walk: WalkOptions,
        tips: TipFilter,
        graph_width: Option<f32>,
        signatures: SignatureCache,
        cx: &mut Context<Self>,
    ) -> Self {
        let search_events = cx.subscribe(&search, Self::on_search_event);
        let query = search.read(cx).value().trim().to_lowercase();
        let head_row = graph
            .labels
            .iter()
            .find(|(_, labels)| labels.iter().any(|l| l.kind == RefKind::Head))
            .and_then(|(id, _)| graph.row_of(id));
        let pills = build_pills(&graph, &tips, &HashMap::new());
        let mut packed = Metas::with_capacity(graph.rows());
        packed.append(metas);
        let mut this = Self {
            graph,
            metas: packed,
            selected: None,
            scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
            columns: Columns::from_settings(&SettingsStore::settings(cx).graph, graph_width),
            resized: Vec::new(),
            pills,
            signatures,
            signatures_asked: Default::default(),
            refs_drawn: Rc::new(Cell::new(Pixels::ZERO)),
            search,
            query,
            matches: Vec::new(),
            _search_events: search_events,
            search_changes: false,
            change_search: None,
            wip: None,
            wip_selected: false,
            head_row,
            bisect: None,
            shallow: Default::default(),
            walk,
            tips,
            picked: Default::default(),
            range_end: None,
            worktrees: HashMap::new(),
        };
        this.recompute_matches();
        this
    }

    /// Marks the local branches checked out in other worktrees (short name → folder
    /// name) on their pills.
    pub fn set_worktrees(
        &mut self,
        worktrees: HashMap<String, SharedString>,
        cx: &mut Context<Self>,
    ) {
        if self.worktrees != worktrees {
            self.pills = build_pills(&self.graph, &self.tips, &worktrees);
            self.worktrees = worktrees;
            cx.notify();
        }
    }

    /// The graph column's width kept for the repository (read after the graph was built).
    /// A drag going on wins.
    pub fn set_graph_width(&mut self, width: Option<f32>, cx: &mut Context<Self>) {
        if self.resized.is_empty() && self.columns.graph != width {
            self.columns.graph = width;
            cx.notify();
        }
    }

    /// Shows a bisect's marks (or none, when no bisect runs); the row menu follows.
    pub fn set_bisect(
        &mut self,
        marks: Option<std::collections::HashMap<ObjectId, BisectMark>>,
        cx: &mut Context<Self>,
    ) {
        if self.bisect != marks {
            self.bisect = marks;
            cx.notify();
        }
    }

    /// A shallow clone's boundary commits, marked "history cut here"; empty when the
    /// history is complete.
    pub fn set_shallow(&mut self, boundary: Vec<ObjectId>, cx: &mut Context<Self>) {
        let boundary: std::collections::HashSet<ObjectId> = boundary.into_iter().collect();
        if self.shallow != boundary {
            self.shallow = boundary;
            cx.notify();
        }
    }

    pub fn set_wip(&mut self, wip: Option<WipSummary>, cx: &mut Context<Self>) {
        self.wip = wip;
        if wip.is_none() {
            self.wip_selected = false;
        }
        cx.notify();
    }

    /// Drops the commit selection: selects the working changes when there are some (and
    /// returns true), else nothing.
    pub fn deselect(&mut self, cx: &mut Context<Self>) -> bool {
        if self.wip.is_some() {
            self.select_wip(cx);
            return true;
        }
        self.selected = None;
        self.picked.clear();
        cx.notify();
        false
    }

    pub fn select_wip(&mut self, cx: &mut Context<Self>) {
        if self.wip.is_none() {
            return;
        }
        self.wip_selected = true;
        self.selected = None;
        self.picked.clear();
        cx.emit(GraphEvent::WipSelected);
        cx.notify();
    }

    /// Scrolls the list back to its first row: the newest commits, right under the working
    /// changes.
    pub fn scroll_to_top(&mut self, cx: &mut Context<Self>) {
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    /// The lane color of the checked-out commit, once it is in the graph.
    pub fn head_color(&self) -> Option<usize> {
        self.head_row
            .map(|row| usize::from(self.graph.layout.node(row).color % PALETTE_SIZE))
    }

    /// Column widths and visibility follow the settings (a drag in another tab, the
    /// settings window); the graph column keeps its own width.
    pub fn settings_changed(&mut self, cx: &mut Context<Self>) {
        if self.resized.is_empty() {
            let settings = &SettingsStore::settings(cx).graph;
            self.columns = Columns::from_settings(settings, self.columns.graph);
        }
        cx.notify();
    }

    pub fn loaded_rows(&self) -> usize {
        self.metas.len()
    }

    pub fn append_metas(&mut self, more: Vec<CommitMeta>, cx: &mut Context<Self>) {
        self.metas.append(more);
        if !self.query.is_empty() {
            self.recompute_matches();
        }
        cx.notify();
    }

    /// The row showing commit `id` (the headless UI tests).
    #[cfg(test)]
    pub(crate) fn row_of(&self, id: ObjectId) -> Option<usize> {
        self.graph.commits.iter().position(|commit| commit == id)
    }

    /// The selected commit (`None` when nothing or the working changes are selected).
    pub fn selected_id(&self) -> Option<ObjectId> {
        self.selected.and_then(|row| self.graph.commits.get(row))
    }

    /// The selected commit as the row menu acts on it (`None` when nothing or the working
    /// changes are selected).
    pub(crate) fn selected_commit(&self) -> Option<RowCommit> {
        self.row_commit(self.selected?)
    }

    /// The selected commits while several are, oldest first; without the checked-out one
    /// unless `with_head` (picking it would do nothing). Empty for a single selection.
    pub(crate) fn picked_commits(&self, with_head: bool) -> Vec<RowCommit> {
        self.picked
            .iter()
            .rev()
            .filter(|&&row| with_head || Some(row) != self.head_row)
            .filter_map(|&row| self.row_commit(row))
            .collect()
    }

    /// Ctrl+click adds the row to the selection or takes it out; Shift+click (`range`)
    /// selects every row from the selected one to it.
    fn extend_selection(&mut self, row: usize, range: bool, cx: &mut Context<Self>) {
        let Some(anchor) = self.selected else {
            return self.select(row, ScrollStrategy::Nearest, cx);
        };
        if range {
            self.picked = (anchor.min(row)..=anchor.max(row)).collect();
            self.range_end = Some(row);
        } else {
            self.range_end = None;
            if self.picked.is_empty() {
                self.picked.insert(anchor);
            }
            if self.picked.insert(row) {
                self.selected = Some(row);
            } else {
                self.picked.remove(&row);
                if self.selected == Some(row) {
                    self.selected = self.picked.iter().next().copied();
                }
            }
        }
        match self.picked.len() {
            0 => {
                self.selected = None;
                cx.notify();
            }
            1 => {
                let only = self.picked.pop_first().unwrap_or(row);
                self.select(only, ScrollStrategy::Nearest, cx);
            }
            _ => {
                self.wip_selected = false;
                cx.emit(GraphEvent::MultiSelected(self.picked_commits(true)));
                cx.notify();
            }
        }
    }

    fn row_commit(&self, row: usize) -> Option<RowCommit> {
        let id = self.graph.commits.get(row)?;
        Some(RowCommit {
            id,
            merge: self.graph.merges[row],
            summary: self.metas.get(row).map_or_else(
                || id.to_hex_with_len(8).to_string(),
                |meta| meta.summary.to_string(),
            ),
        })
    }

    /// Puts the keyboard focus in the search field (Ctrl+F, and the palette).
    pub(crate) fn focus_search_box(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |input, cx| input.focus(window, cx));
    }

    /// Selects and centers the commit without announcing it: after a reload the details
    /// already show it, and an open diff should stay open.
    pub fn restore_selection(&mut self, id: ObjectId, cx: &mut Context<Self>) {
        if let Some(row) = self.graph.row_of(&id) {
            self.selected = Some(row);
            self.picked.clear();
            self.wip_selected = false;
            self.scroll.scroll_to_item(row, ScrollStrategy::Center);
            cx.notify();
        }
    }

    /// Selects and centers the commit (e.g. when a branch is clicked in the sidebar).
    /// Returns false if the commit isn't in the graph.
    pub fn reveal(&mut self, id: ObjectId, cx: &mut Context<Self>) -> bool {
        let Some(row) = self.graph.row_of(&id) else {
            return false;
        };
        self.select(row, ScrollStrategy::Center, cx);
        true
    }

    fn select(&mut self, row: usize, scroll: ScrollStrategy, cx: &mut Context<Self>) {
        let Some(id) = self.graph.commits.get(row) else {
            return;
        };
        self.selected = Some(row);
        self.picked.clear();
        self.range_end = None;
        self.wip_selected = false;
        self.scroll.scroll_to_item(row, scroll);
        cx.emit(GraphEvent::Selected(id));
        cx.notify();
    }

    /// Moves the selection by `delta` rows, clamped to the graph.
    /// The "Working changes" row sits above row 0: Up from the first commit selects it,
    /// Down from it selects the first commit.
    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let last = self.graph.rows().saturating_sub(1);
        if self.wip_selected {
            if delta > 0 {
                self.select(0, ScrollStrategy::Top, cx);
            }
            return;
        }
        let row = match self.selected {
            Some(0) if delta < 0 && self.wip.is_some() => return self.select_wip(cx),
            Some(row) => row.saturating_add_signed(delta).min(last),
            None => 0,
        };
        self.select(row, ScrollStrategy::Top, cx);
    }

    /// Shift with the arrows, Home or End: the range from the selected commit to a moving
    /// end, as Shift+click makes it. Without a selected commit it selects, like the
    /// arrows alone.
    fn extend(&mut self, to: Step, cx: &mut Context<Self>) {
        let rows = self.graph.rows();
        let Some(anchor) = self.selected.filter(|_| !self.wip_selected && rows > 0) else {
            return match to {
                Step::Previous => self.step(-1, cx),
                Step::First => self.select(0, ScrollStrategy::Top, cx),
                Step::Last => self.select(rows.saturating_sub(1), ScrollStrategy::Top, cx),
                Step::Next => self.step(1, cx),
            };
        };
        let (end, range) = extend_range(anchor, self.range_end.unwrap_or(anchor), to, rows);
        self.scroll.scroll_to_item(end, ScrollStrategy::Nearest);
        if range.start() == range.end() {
            // Back to one commit: a single selection again.
            return self.select(anchor, ScrollStrategy::Nearest, cx);
        }
        self.picked = range.collect();
        self.range_end = Some(end);
        cx.emit(GraphEvent::MultiSelected(self.picked_commits(true)));
        cx.notify();
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.step(1, cx);
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        self.step(-1, cx);
    }

    fn page_down(&mut self, _: &PageDown, _: &mut Window, cx: &mut Context<Self>) {
        self.step(PAGE_ROWS as isize, cx);
    }

    fn page_up(&mut self, _: &PageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.step(-(PAGE_ROWS as isize), cx);
    }

    fn select_first(&mut self, _: &SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        self.select(0, ScrollStrategy::Top, cx);
    }

    fn select_last(&mut self, _: &SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        let last = self.graph.rows().saturating_sub(1);
        self.select(last, ScrollStrategy::Top, cx);
    }

    fn focus_search(&mut self, _: &FocusSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |input, cx| input.focus(window, cx));
    }

    // ---- search ------------------------------------------------------------------

    fn on_search_event(
        &mut self,
        input: Entity<InputState>,
        event: &InputEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => {
                let query = input.read(cx).value();
                self.set_query(&query, cx);
            }
            // Searching changes is costly, so it runs on Enter, not while typing. Enter
            // again (same query) moves between the results.
            InputEvent::PressEnter { shift, .. } if self.search_changes => {
                let query = input.read(cx).value().trim().to_owned();
                let same = self
                    .change_search
                    .as_ref()
                    .is_some_and(|search| search.query == query);
                if same {
                    self.next_match(!*shift, cx);
                } else {
                    self.start_change_search(query, cx);
                }
            }
            InputEvent::PressEnter { shift, .. } => self.next_match(!*shift, cx),
            _ => {}
        }
    }

    fn start_change_search(&mut self, query: String, cx: &mut Context<Self>) {
        let Some(pickaxe) = gitweft_git::read::Pickaxe::parse(&query) else {
            return;
        };
        self.change_search = Some(ChangeSearch {
            query,
            rows: Vec::new(),
            running: true,
            error: None,
        });
        self.recompute_matches();
        cx.emit(GraphEvent::SearchChanges(pickaxe));
        cx.notify();
    }

    fn set_search_changes(&mut self, on: bool, cx: &mut Context<Self>) {
        if self
            .change_search
            .as_ref()
            .is_some_and(|search| search.running)
        {
            cx.emit(GraphEvent::CancelChangeSearch);
        }
        self.search_changes = on;
        self.change_search = None;
        self.recompute_matches();
        cx.notify();
    }

    fn cancel_change_search(&mut self, cx: &mut Context<Self>) {
        if let Some(search) = &mut self.change_search
            && search.running
        {
            search.running = false;
            cx.emit(GraphEvent::CancelChangeSearch);
            cx.notify();
        }
    }

    /// Commits found by the running change search; the first one found is selected.
    /// Commits the graph doesn't show (the stash, for one) are skipped.
    pub fn add_change_results(&mut self, ids: Vec<ObjectId>, cx: &mut Context<Self>) {
        let Some(search) = &mut self.change_search else {
            return;
        };
        let was_empty = search.rows.is_empty();
        for id in ids {
            if let Some(row) = self.graph.row_of(&id)
                && let Err(position) = search.rows.binary_search(&row)
            {
                search.rows.insert(position, row);
            }
        }
        let first = search.rows.first().copied().filter(|_| was_empty);
        self.recompute_matches();
        if let Some(row) = first {
            self.select(row, ScrollStrategy::Center, cx);
        }
        cx.notify();
    }

    /// The change search ended: finished, or failed with `error`.
    pub fn change_search_finished(&mut self, error: Option<String>, cx: &mut Context<Self>) {
        if let Some(search) = &mut self.change_search {
            search.running = false;
            search.error = error;
        }
        cx.notify();
    }

    fn set_query(&mut self, query: &str, cx: &mut Context<Self>) {
        let query = query.trim().to_lowercase();
        if query == self.query {
            return;
        }
        self.query = query;
        self.recompute_matches();
        // Jump to the first match at or below the current selection.
        let from = self.selected.unwrap_or(0);
        let target = self
            .matches
            .iter()
            .copied()
            .find(|&row| row >= from)
            .or_else(|| self.matches.first().copied());
        if let Some(row) = target {
            self.select(row, ScrollStrategy::Center, cx);
        }
        cx.notify();
    }

    fn recompute_matches(&mut self) {
        self.matches = if self.search_changes {
            self.change_search
                .as_ref()
                .map(|search| search.rows.clone())
                .unwrap_or_default()
        } else if self.query.is_empty() {
            Vec::new()
        } else {
            (0..self.graph.rows())
                .filter(|&row| self.row_matches(row))
                .collect()
        };
    }

    /// Case-insensitive match on summary, author, email and ref names, or a SHA prefix.
    fn row_matches(&self, row: usize) -> bool {
        let query = self.query.as_str();
        let id = self.graph.commits.id(row);
        if query.len() >= 4
            && query.bytes().all(|b| b.is_ascii_hexdigit())
            && id.to_string().starts_with(query)
        {
            return true;
        }
        let contains = |text: &str| text.to_lowercase().contains(query);
        if let Some(meta) = self.metas.get(row)
            && (contains(meta.summary)
                || contains(&meta.author.name)
                || contains(&meta.author.email))
        {
            return true;
        }
        self.graph
            .labels
            .get(&id)
            .is_some_and(|labels| labels.iter().any(|label| contains(&label.name)))
    }

    fn next_match(&mut self, forward: bool, cx: &mut Context<Self>) {
        let (Some(&first), Some(&last)) = (self.matches.first(), self.matches.last()) else {
            return;
        };
        let current = self.selected.unwrap_or(0);
        let row = if forward {
            self.matches
                .iter()
                .copied()
                .find(|&row| row > current)
                .unwrap_or(first)
        } else {
            self.matches
                .iter()
                .rev()
                .copied()
                .find(|&row| row < current)
                .unwrap_or(last)
        };
        self.select(row, ScrollStrategy::Center, cx);
    }

    /// `(position of the selection among matches, number of matches)` while searching.
    fn match_status(&self) -> Option<(usize, usize)> {
        if self.query.is_empty() {
            return None;
        }
        Some(self.match_position())
    }

    /// `(position of the selection among matches, number of matches)`.
    fn match_position(&self) -> (usize, usize) {
        let current = self
            .selected
            .and_then(|row| self.matches.binary_search(&row).ok())
            .map_or(0, |index| index + 1);
        (current, self.matches.len())
    }

    /// The search bar's status text and color, and whether a change search can be cancelled.
    fn search_status(&self, cx: &App) -> (String, Hsla, bool) {
        let p = cx.palette();
        let (muted, danger) = (p.fg3, p.del);
        if !self.search_changes {
            let text = match self.match_status() {
                None => String::new(),
                Some((_, 0)) => t!("graph.search_no_matches").into_owned(),
                Some((current, total)) => {
                    t!("graph.search_matches", current = current, total = total).into_owned()
                }
            };
            return (text, muted, false);
        }
        match &self.change_search {
            None => (t!("graph.search_changes_hint").into_owned(), muted, false),
            Some(search) if search.running => (
                tn!("graph.search_changes_running", count = search.rows.len()).into_owned(),
                muted,
                true,
            ),
            Some(ChangeSearch {
                error: Some(error), ..
            }) => (error.clone(), danger, false),
            Some(search) if search.rows.is_empty() => {
                (t!("graph.search_changes_none").into_owned(), muted, false)
            }
            Some(_) => {
                let (current, total) = self.match_position();
                let text = t!("graph.search_matches", current = current, total = total);
                (text.into_owned(), muted, false)
            }
        }
    }

    // ---- columns -----------------------------------------------------------------

    fn column_layout(&self, cx: &App) -> ColumnLayout {
        let m = cx.metrics();
        let z = |value: f32| px(value * m.zoom);
        let shown = &SettingsStore::settings(cx).graph;
        let columns = self.columns;
        let graph = match columns.graph {
            Some(width) => z(width),
            None => {
                // At least three lanes wide, for the "Graph" header label.
                let lanes = self.graph.layout.lane_count().clamp(3, MAX_DRAWN_LANES);
                let needed = m.lane * f32::from(lanes - 1) + z(2. * LANE_INSET);
                if needed < z(DEFAULT_MAX_GRAPH_WIDTH) {
                    needed
                } else {
                    z(DEFAULT_MAX_GRAPH_WIDTH)
                }
            }
        };
        ColumnLayout {
            refs: z(columns.refs),
            graph,
            author: shown.show_author.then(|| z(columns.author)),
            date: shown.show_date.then(|| z(columns.date)),
            sha: shown.show_sha.then(|| z(SHA_WIDTH)),
            separator: z(SEPARATOR),
            right: z(RIGHT_PADDING),
            min_message: z(MIN_MESSAGE_WIDTH),
            min_column: z(MIN_COLUMN),
        }
    }

    fn on_column_drag(
        &mut self,
        event: &DragMoveEvent<ColumnDrag>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let column = event.drag(cx).0;
        let x = event.event.position.x;
        let bounds = event.bounds;
        let layout = self.column_layout(cx);
        let zoom = cx.metrics().zoom;
        let sep = layout.separator;
        let after = |width: Option<Pixels>| width.map_or(px(0.), |width| width + sep);
        // The right edges of the date and author columns (the columns right of them may be hidden).
        let date_right = bounds.right() - layout.right - after(layout.sha);
        let author_right = date_right - after(layout.date);
        // Branch / Tag and Author share what the other columns leave them (the message at
        // its minimum). Without room for both, flex draws them shrunk in proportion.
        let author_shown = layout.author.is_some();
        let room = author_right
            - layout.author.map_or(px(0.), |_| sep)
            - layout.min_message
            - sep
            - layout.graph
            - sep
            - bounds.left();
        let wanted = layout.refs + layout.author.unwrap_or(px(0.));
        let scale = if wanted > room && wanted > px(0.) {
            f32::from(room) / f32::from(wanted)
        } else {
            1.
        };
        let (refs, author) = (layout.refs * scale, layout.author.unwrap_or(px(0.)) * scale);
        let mut resize = |column: Column, width: Pixels| {
            let width = design_width(width, zoom);
            let columns = &mut self.columns;
            match column {
                Column::Refs => columns.refs = width,
                Column::Graph => columns.graph = Some(width),
                Column::Author => columns.author = width,
                Column::Date => columns.date = width,
            }
            if !self.resized.contains(&column) {
                self.resized.push(column);
            }
        };
        // Dragged into the other shrinking column's room: that column gives the space up,
        // so the edge stays under the pointer (both widths are saved).
        match column {
            Column::Refs => {
                let refs = x - bounds.left();
                resize(Column::Refs, refs);
                if author_shown && refs + author > room {
                    resize(Column::Author, room - refs);
                }
            }
            Column::Author => {
                let author = author_right - x;
                resize(Column::Author, author);
                if refs + author > room {
                    resize(Column::Refs, room - author);
                }
            }
            Column::Graph => resize(Column::Graph, x - (bounds.left() + refs + sep)),
            Column::Date => resize(Column::Date, date_right - x),
        }
        cx.notify();
    }

    /// A resize ended: the widths go to the settings, the graph column's to the
    /// repository's (it depends on the lanes).
    fn save_column(&mut self, cx: &mut Context<Self>) {
        if self.resized.is_empty() {
            return;
        }
        let (resized, columns) = (std::mem::take(&mut self.resized), self.columns);
        if resized.contains(&Column::Graph) {
            cx.emit(GraphEvent::GraphWidth(columns.graph));
        }
        SettingsStore::update(cx, |settings| {
            let graph = &mut settings.graph;
            for column in resized {
                match column {
                    Column::Refs => graph.refs_width = Some(columns.refs),
                    Column::Author => graph.author_width = Some(columns.author),
                    Column::Date => graph.date_width = Some(columns.date),
                    Column::Graph => {}
                }
            }
        });
    }

    // ---- rendering ---------------------------------------------------------------

    fn render_rows(
        &mut self,
        range: Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);
        let graph_style = GraphStyle::new(&p, &m);
        let columns = self.column_layout(cx);
        let mono = mono_font();
        let drop_bg = p.acc.opacity(0.25);
        // The signatures of rows on screen nobody has asked about yet.
        let unknown: Vec<ObjectId> = {
            let known = self.signatures.borrow();
            range
                .clone()
                .map(|row| self.graph.commits.id(row))
                .filter(|id| !known.contains_key(id) && !self.signatures_asked.contains(id))
                .collect()
        };
        if !unknown.is_empty() {
            self.signatures_asked.extend(unknown.iter().copied());
            cx.emit(GraphEvent::NeedSignatures(unknown));
        }
        // The dashed line from the "Working changes" node runs down to HEAD's row, in HEAD's
        // column, through the rows above it.
        let wip_head = self.head_row.filter(|_| self.wip.is_some());
        let head_lane = wip_head.map(|row| self.graph.layout.node(row).lane);
        let searching = if self.search_changes {
            self.change_search.is_some()
        } else {
            !self.query.is_empty()
        };
        let view = cx.entity().downgrade();
        // "Create fixup! commit" needs something staged.
        let staged = self.wip.is_some_and(|wip| wip.staged > 0);

        range
            .map(|row| {
                let layout = &self.graph.layout;
                let cell = RowGeometry {
                    row,
                    node: layout.node(row),
                    is_merge: self.graph.merges[row],
                    is_head: self.head_row == Some(row),
                    wip_above: wip_head == Some(row),
                    wip_through: head_lane.filter(|_| wip_head.is_some_and(|head| row < head)),
                    spans: layout.spans(row).copied().collect(),
                    connectors: layout.connectors(row).to_vec(),
                    style: graph_style,
                };
                let id = self.graph.commits.id(row);
                let meta = self.metas.get(row);
                let signature = self.signatures.borrow().get(&id).copied().flatten();
                let labels = self.graph.labels.get(&id);
                let pills = self.pills.get(&id).cloned();
                let refs_drawn = self.refs_drawn.get();
                let node_color = graph_style.color(cell.node.color);
                let dimmed = searching && self.matches.binary_search(&row).is_err();
                // Local branches on this commit that aren't checked out can be switched to.
                let local_branches: Vec<String> = labels
                    .map(|labels| {
                        labels
                            .iter()
                            .filter(|label| label.kind == RefKind::Local)
                            .map(|label| label.name.clone())
                            .collect()
                    })
                    .unwrap_or_default();
                // Another branch's label dropped here merges into or rebases onto one of
                // these (the workspace asks which when there are several).
                let drop_targets = local_branches.clone();
                let row_view = view.clone();
                // Cherry-picking or resetting to the checked-out commit does nothing.
                let is_head = self.head_row == Some(row);
                let bisecting = self.bisect.is_some();
                let mark = self
                    .bisect
                    .as_ref()
                    .and_then(|marks| marks.get(&id).copied());
                let cut = self.shallow.contains(&id);
                let chosen = self.selected == Some(row) || self.picked.contains(&row);
                // Dragged or right-clicked among several selected rows: they all go.
                let several = (self.picked.len() > 1 && self.picked.contains(&row))
                    .then(|| self.picked_commits(false))
                    .filter(|picks| !picks.is_empty());
                let commit = RowCommit {
                    id,
                    merge: self.graph.merges[row],
                    summary: meta.map_or_else(
                        || id.to_hex_with_len(8).to_string(),
                        |meta| meta.summary.to_string(),
                    ),
                };

                h_flex()
                    .id(row)
                    // How view tests find the row (a no-op outside them).
                    .debug_selector(|| format!("graph-row-{row}"))
                    .h(m.row)
                    // In a very narrow panel the last columns are cut, not drawn outside.
                    .overflow_hidden()
                    .w_full()
                    .pr(columns.right)
                    .when(dimmed, |this| this.opacity(p.dim))
                    .when(chosen, |this| this.bg(p.sel))
                    .when(!chosen, |this| this.hover(move |style| style.bg(p.hover)))
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        this.focus.focus(window, cx);
                        let modifiers = event.modifiers();
                        if modifiers.control || modifiers.shift {
                            this.extend_selection(row, modifiers.shift, cx);
                        } else if event.click_count() >= 2 {
                            // The first click of the two has selected the row already.
                            cx.emit(GraphEvent::Activated(id));
                        } else {
                            this.select(row, ScrollStrategy::Top, cx);
                        }
                    }))
                    // Any other commit can be dragged onto the checked-out one (cherry-pick),
                    // and so can branch labels (merge); a label's drag wins over its row's.
                    .when(!is_head, |this| {
                        let dragged = several.clone().unwrap_or_else(|| vec![commit.clone()]);
                        this.on_drag(CommitDrag(dragged), |drag, _, _, cx| {
                            let text = match drag.0.as_slice() {
                                [one] => t!("graph.drag_commit", summary = one.summary.as_str()),
                                many => tn!("graph.drag_commits", count = many.len()),
                            };
                            cx.new(|_| DragLabel(text.into_owned()))
                        })
                    })
                    .when(is_head, |this| {
                        this.drag_over::<CommitDrag>(move |style, _, _, _| style.bg(drop_bg))
                            .drag_over::<BranchDrag>(move |style, _, _, _| style.bg(drop_bg))
                            .on_drop(cx.listener(|_, drag: &CommitDrag, _, cx| {
                                cx.emit(GraphEvent::DropCommits(drag.0.clone()))
                            }))
                            .on_drop(cx.listener(|_, drag: &BranchDrag, _, cx| {
                                cx.emit(GraphEvent::DropBranch {
                                    full_name: drag.full_name.to_string(),
                                    shown: drag.shown.to_string(),
                                })
                            }))
                    })
                    .when(!is_head && !drop_targets.is_empty(), |this| {
                        this.drag_over::<BranchDrag>(move |style, _, _, _| style.bg(drop_bg))
                            .on_drop(cx.listener(move |_, drag: &BranchDrag, _, cx| {
                                cx.emit(GraphEvent::DropBranchOnBranch {
                                    full_name: drag.full_name.to_string(),
                                    shown: drag.shown.to_string(),
                                    targets: drop_targets.clone(),
                                })
                            }))
                    })
                    .child(
                        refs_cell(&columns, m.zoom)
                            .flex()
                            .items_center()
                            .overflow_hidden()
                            .gap(px(4. * m.zoom))
                            .when_some(mark, |this, mark| this.child(bisect_pill(mark, &p, &m)))
                            .when(cut, |this| this.child(shallow_pill(&p, &m)))
                            .when_some(pills, |this, pills| {
                                // Natural widths: mono text (0.6 em a character) plus the
                                // frame's padding and border, at most the pill's maximum.
                                let char_width = f32::from(m.text.mono_small) * 0.6;
                                let frame = 14. * m.zoom + 2.;
                                let natural: Vec<f32> = pills
                                    .iter()
                                    .take(MAX_PILLS)
                                    .map(|pill| {
                                        (pill.chars as f32 * char_width + frame)
                                            .min(PILL_MAX_WIDTH * m.zoom)
                                    })
                                    .collect();
                                // As drawn in the last frame; the column's width before that.
                                let drawn = if refs_drawn > Pixels::ZERO {
                                    refs_drawn
                                } else {
                                    columns.refs
                                };
                                let available = f32::from(drawn) - REFS_INSET * m.zoom;
                                let (shown, count_fits) = pills_that_fit(
                                    &natural,
                                    MIN_PILL_CHARS as f32 * char_width + frame,
                                    available,
                                    4. * m.zoom,
                                    3. * char_width,
                                );
                                let hidden = pills.len() - shown;
                                this.children(pills.iter().take(shown).enumerate().map(
                                    |(ix, pill)| {
                                        let id = row * MAX_PILLS + ix;
                                        draggable_pill(pill, id, node_color, &p, &m)
                                    },
                                ))
                                .when(
                                    hidden > 0 && count_fits,
                                    |this| {
                                        // Hovered, the refs it stands for.
                                        let names = join_list(
                                            pills.iter().skip(shown).map(|pill| pill.text.clone()),
                                        );
                                        this.child(
                                            div()
                                                .id("more-pills")
                                                .flex_shrink_0()
                                                .font_family(mono.clone())
                                                .text_size(m.text.mono_small)
                                                .text_color(p.fg3)
                                                .child(format!("+{hidden}"))
                                                .tooltip(move |window, cx| {
                                                    Tooltip::new(names.clone()).build(window, cx)
                                                }),
                                        )
                                    },
                                )
                            }),
                    )
                    .child(separator(columns.separator))
                    .child(
                        canvas(
                            |_, _, _| {},
                            move |bounds, (), window, _| cell.paint(bounds, window),
                        )
                        .w(columns.graph)
                        .h_full()
                        .flex_shrink_0(),
                    )
                    .child(separator(columns.separator))
                    .child(
                        message_cell(columns.min_message)
                            .text_color(p.fg)
                            // A signed commit's mark, then the summary cut to what's left.
                            .when_some(signature, |this, status| {
                                this.flex()
                                    .items_center()
                                    .gap(px(5. * m.zoom))
                                    .child(signature_mark(status, &p, &m))
                            })
                            .child(truncated(
                                "summary",
                                meta.map(|meta| SharedString::from(meta.summary))
                                    .unwrap_or_default(),
                            )),
                    )
                    .when_some(columns.author, |this, width| {
                        this.child(separator(columns.separator)).child(
                            shrinkable(width, columns.min_column)
                                .text_color(p.fg2)
                                .child(truncated(
                                    "author",
                                    meta.map(|meta| SharedString::from(&*meta.author.name))
                                        .unwrap_or_default(),
                                )),
                        )
                    })
                    .when_some(columns.date, |this, width| {
                        this.child(separator(columns.separator)).child(
                            fixed(width)
                                .font_family(mono.clone())
                                .text_size(m.text.mono)
                                .text_color(p.fg2)
                                .child(truncated(
                                    "date",
                                    meta.map(|meta| format_time(meta.time)).unwrap_or_default(),
                                )),
                        )
                    })
                    .when_some(columns.sha, |this, width| {
                        this.child(separator(columns.separator)).child(
                            fixed(width)
                                .font_family(mono.clone())
                                .text_size(m.text.mono)
                                .text_color(p.fg3)
                                .child(truncated("sha", id.to_hex_with_len(8).to_string())),
                        )
                    })
                    .context_menu(move |menu, window, cx| {
                        let view = &row_view;
                        let mut menu = menu;
                        if let Some(picks) = &several {
                            let (picks, saves) = (picks.clone(), picks.clone());
                            let label = tn!("graph.cherry_pick_multiple", count = picks.len());
                            let save = tn!("files.save_patches", count = saves.len());
                            menu = menu
                                .item(event_item(view, label.into_owned(), move || {
                                    GraphEvent::CherryPickMany(picks.clone())
                                }))
                                .item(event_item(view, save.into_owned(), move || {
                                    GraphEvent::SavePatches(saves.clone())
                                }))
                                .separator();
                        }
                        for name in &local_branches {
                            let name = name.clone();
                            let label = t!("branch.checkout_named", name = name).into_owned();
                            menu = menu.item(event_item(view, label, move || {
                                GraphEvent::CheckoutBranch(name.clone())
                            }));
                        }
                        menu = menu
                            .item(event_item(
                                view,
                                t!("branch.new_here").into_owned(),
                                move || GraphEvent::CreateBranchAt(id),
                            ))
                            .item(event_item(
                                view,
                                t!("branch.checkout_detached").into_owned(),
                                move || GraphEvent::CheckoutCommit(id),
                            ))
                            .item(event_item(
                                view,
                                t!("tag.new_here").into_owned(),
                                move || GraphEvent::CreateTagAt(id),
                            ))
                            .item(event_item(
                                view,
                                t!("files.save_patch").into_owned(),
                                move || GraphEvent::SavePatch(id),
                            ))
                            .item(event_item(
                                view,
                                t!("files.export_archive").into_owned(),
                                move || GraphEvent::Archive(id),
                            ))
                            .separator();
                        // Bisect: start from here, or mark the commit while one runs.
                        for (mark, label) in bisect_items(bisecting) {
                            menu = menu.item(event_item(view, label, move || {
                                GraphEvent::Bisect(id, mark)
                            }));
                        }
                        menu = menu.separator();
                        if !is_head {
                            let commit = commit.clone();
                            let loose = commit.clone();
                            menu = menu.item(event_item(
                                view,
                                t!("rewrite.cherry_pick").into_owned(),
                                move || GraphEvent::CherryPick(commit.clone()),
                            ));
                            menu = menu.item(event_item(
                                view,
                                t!("rewrite.cherry_pick_no_commit").into_owned(),
                                move || GraphEvent::CherryPickNoCommit(loose.clone()),
                            ));
                        }
                        let reverted = commit.clone();
                        menu = menu.item(event_item(
                            view,
                            t!("rewrite.revert").into_owned(),
                            move || GraphEvent::Revert(reverted.clone()),
                        ));
                        if !is_head {
                            let commit = commit.clone();
                            menu = menu.item(event_item(
                                view,
                                t!("rebase.menu").into_owned(),
                                move || GraphEvent::InteractiveRebase(commit.clone()),
                            ));
                        }
                        let from = commit.clone();
                        menu = menu.item(event_item(
                            view,
                            t!("rebase.menu_from").into_owned(),
                            move || GraphEvent::InteractiveRebaseFrom(from.clone()),
                        ));
                        // A merge can't be picked on its own in a rebase.
                        if !commit.merge {
                            for (edit, label) in quick_edits() {
                                let commit = commit.clone();
                                menu = menu.item(event_item(view, label, move || {
                                    GraphEvent::QuickRebase(commit.clone(), edit)
                                }));
                            }
                            let fixup = commit.clone();
                            menu = menu.item(
                                event_item(
                                    view,
                                    t!("rebase.quick_fixup").into_owned(),
                                    move || GraphEvent::CreateFixup(fixup.clone()),
                                )
                                .disabled(!staged),
                            );
                        }
                        if !is_head {
                            let (view, commit) = (view.clone(), commit.clone());
                            menu = menu.submenu(
                                t!("rewrite.reset_menu").into_owned(),
                                window,
                                cx,
                                move |menu, _, _| {
                                    [
                                        (ResetMode::Soft, t!("rewrite.reset_soft")),
                                        (ResetMode::Mixed, t!("rewrite.reset_mixed")),
                                        (ResetMode::Hard, t!("rewrite.reset_hard")),
                                    ]
                                    .into_iter()
                                    .fold(
                                        menu,
                                        |menu, (mode, label)| {
                                            let commit = commit.clone();
                                            menu.item(event_item(
                                                &view,
                                                label.into_owned(),
                                                move || GraphEvent::Reset(commit.clone(), mode),
                                            ))
                                        },
                                    )
                                },
                            );
                        }
                        menu = menu.separator();
                        if !is_head {
                            menu = menu.item(event_item(
                                view,
                                t!("graph.select_merge_base").into_owned(),
                                move || GraphEvent::SelectMergeBase(id),
                            ));
                        }
                        menu = menu.item(event_item(
                            view,
                            t!("compare.with_another").into_owned(),
                            move || GraphEvent::CompareWith(id),
                        ));
                        menu.item(event_item(
                            view,
                            t!("tree.browse").into_owned(),
                            move || GraphEvent::BrowseFiles(id),
                        ))
                    })
                    .into_any_element()
            })
            .collect()
    }

    /// The pinned "Working changes" row above the commit list.
    fn render_wip_row(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let wip = self.wip?;
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);
        let graph_style = GraphStyle::new(&p, &m);
        let lane = self
            .head_row
            .map_or(0, |row| self.graph.layout.node(row).lane);
        // The dashed line leads down to HEAD, wherever it is in the list.
        let link = self.head_row.is_some();
        let hover = p.panel2.blend(p.hover);
        let mut parts = Vec::new();
        if wip.conflicts > 0 {
            parts.push(tn!("wip.conflicts", count = wip.conflicts).into_owned());
        }
        if wip.staged > 0 {
            parts.push(tn!("wip.staged", count = wip.staged).into_owned());
        }
        if wip.unstaged > 0 {
            parts.push(tn!("wip.unstaged", count = wip.unstaged).into_owned());
        }
        let columns = self.column_layout(cx);
        Some(
            h_flex()
                .id("wip-row")
                .overflow_hidden()
                .h(m.row)
                .w_full()
                .flex_shrink_0()
                .pr(columns.right)
                .bg(if self.wip_selected { p.sel } else { p.panel2 })
                .when(!self.wip_selected, |row| {
                    row.hover(move |style| style.bg(hover))
                })
                .text_color(p.fg2)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.focus.focus(window, cx);
                    this.select_wip(cx);
                }))
                .child(refs_cell(&columns, m.zoom))
                .child(separator(columns.separator))
                .child(
                    canvas(
                        |_, _, _| {},
                        move |bounds, (), window, _| {
                            paint_wip_node(bounds, lane, link, &graph_style, window)
                        },
                    )
                    .w(columns.graph)
                    .h_full()
                    .flex_shrink_0(),
                )
                .child(separator(columns.separator))
                .child(
                    // Not cut at the message column: the cells after it are empty here.
                    div()
                        .flex_1()
                        .min_w(columns.min_message)
                        .whitespace_nowrap()
                        .italic()
                        .child(t!("wip.row", counts = parts.join(" · ")).into_owned()),
                )
                // Empty cells for the other columns, so that this row shrinks like the
                // commit rows when the panel is narrow, and its node stays in its lane.
                .when_some(columns.author, |this, width| {
                    this.child(separator(columns.separator))
                        .child(shrinkable(width, columns.min_column))
                })
                .when_some(columns.date, |this, width| {
                    this.child(separator(columns.separator)).child(fixed(width))
                })
                .when_some(columns.sha, |this, width| {
                    this.child(separator(columns.separator)).child(fixed(width))
                })
                .into_any_element(),
        )
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui_kit::component::Sizable as _;
        use gpui_kit::component::button::{Button, ButtonVariants as _};
        use gpui_kit::component::checkbox::Checkbox;

        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);
        let z = |value: f32| px(value * m.zoom);
        let (status, color, cancellable) = self.search_status(cx);
        let cancel = cancellable.then(|| {
            Button::new("cancel-change-search")
                .xsmall()
                .ghost()
                .label(t!("graph.search_changes_cancel").into_owned())
                .on_click(cx.listener(|this, _, _, cx| this.cancel_change_search(cx)))
        });
        let search = Input::new(&self.search)
            .prefix(Icon::new(AppIcon::Search).text_color(p.fg3))
            .cleanable(true);
        h_flex()
            .flex_shrink_0()
            .px(z(12.))
            .py(z(10.))
            .gap(z(12.))
            .child(
                div()
                    .flex_1()
                    .max_w(z(SEARCH_MAX_WIDTH))
                    // The kit's own `Input::h` sizes only multi-line inputs.
                    .child(Styled::h(search, z(SEARCH_HEIGHT)).bg(p.panel)),
            )
            .child(
                div().text_color(p.fg2).text_size(m.text.small).child(
                    Checkbox::new("search-changes")
                        .small()
                        .label(t!("graph.search_changes").into_owned())
                        .checked(self.search_changes)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.set_search_changes(*checked, cx)
                        })),
                ),
            )
            .child(div().text_size(m.text.xs).text_color(color).child(status))
            .children(cancel)
            .child(
                div()
                    .ml_auto()
                    .flex_shrink_0()
                    .child(self.render_walk_menu(cx)),
            )
    }

    /// The view menu: which branches the graph walks, first parents only, the order.
    /// Its label names the options in force, in the accent color when they hide commits
    /// or change the order.
    fn render_walk_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui_kit::component::Sizable as _;
        use gpui_kit::component::button::{Button, ButtonVariants as _};

        let p = *cx.palette();
        let walk = self.walk;
        let view = cx.entity().downgrade();
        let changed = walk != WalkOptions::default() || !self.tips.is_empty();
        let (soloed, filtered) = (self.tips.solo.is_some(), !self.tips.is_empty());
        let hidden = self.tips.hidden.clone();
        let sized = self.columns.graph.is_some();
        let fit_view = view.clone();
        Button::new("graph-walk")
            .ghost()
            .small()
            .icon(Icon::from(AppIcon::ChevronDown))
            .label(walk_summary(&walk, &self.tips))
            .when(changed, |button| button.text_color(p.acc))
            // The label grows with the options: anchored at the right, the menu stays put.
            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                let emit = |label: Cow<'static, str>, checked: bool, event: fn() -> GraphEvent| {
                    let view = view.clone();
                    PopupMenuItem::new(label.into_owned())
                        .checked(checked)
                        .on_click(move |_, _, cx| {
                            view.update(cx, |_, cx| cx.emit(event())).ok();
                        })
                };
                let item = |label: Cow<'static, str>, checked: bool, options: WalkOptions| {
                    let view = view.clone();
                    PopupMenuItem::new(label.into_owned())
                        .checked(checked)
                        .on_click(move |_, _, cx| {
                            view.update(cx, |_, cx| cx.emit(GraphEvent::SetWalk(options)))
                                .ok();
                        })
                };
                // A solo is neither: picking either ends it.
                menu.item(emit(
                    t!("graph.walk.all_branches"),
                    !walk.current_branch && !soloed,
                    || GraphEvent::SetScope(false),
                ))
                .item(emit(
                    t!("graph.walk.current_branch"),
                    walk.current_branch && !soloed,
                    || GraphEvent::SetScope(true),
                ))
                .when(filtered, |menu| {
                    menu.item(emit(t!("graph.walk.show_every_branch"), false, || {
                        GraphEvent::ShowEveryBranch
                    }))
                })
                // What's hidden, each with the way to show it again.
                .when(!hidden.is_empty(), |mut menu| {
                    menu = menu.separator();
                    for entry in hidden.iter().take(MAX_HIDDEN_ITEMS) {
                        let (view, shown) = (view.clone(), entry.clone());
                        menu = menu.item(PopupMenuItem::new(show_hidden_label(entry)).on_click(
                            move |_, _, cx| {
                                let event = GraphEvent::ShowHidden(shown.clone());
                                view.update(cx, |_, cx| cx.emit(event)).ok();
                            },
                        ));
                    }
                    let more = hidden.len().saturating_sub(MAX_HIDDEN_ITEMS);
                    if more > 0 {
                        let label = tn!("graph.walk.more_hidden", count = more).into_owned();
                        menu = menu.item(PopupMenuItem::new(label).disabled(true));
                    }
                    menu
                })
                .when(sized, |menu| {
                    let view = fit_view.clone();
                    menu.separator().item(
                        PopupMenuItem::new(t!("graph.walk.fit_graph").into_owned()).on_click(
                            move |_, _, cx| {
                                view.update(cx, |this, cx| {
                                    this.columns.graph = None;
                                    cx.emit(GraphEvent::GraphWidth(None));
                                    cx.notify();
                                })
                                .ok();
                            },
                        ),
                    )
                })
                .separator()
                .item(item(
                    t!("graph.walk.first_parent"),
                    walk.first_parent,
                    WalkOptions {
                        first_parent: !walk.first_parent,
                        ..walk
                    },
                ))
                .separator()
                .item(item(
                    t!("graph.walk.date_order"),
                    !walk.topo_order,
                    WalkOptions {
                        topo_order: false,
                        ..walk
                    },
                ))
                .item(item(
                    t!("graph.walk.topo_order"),
                    walk.topo_order,
                    WalkOptions {
                        topo_order: true,
                        ..walk
                    },
                ))
            })
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);
        let columns = self.column_layout(cx);
        let handle = |column| resize_handle(column, columns.separator, p.line);
        h_flex()
            .id("graph-header")
            .overflow_hidden()
            .h(m.header)
            .w_full()
            .flex_shrink_0()
            .pr(columns.right)
            .bg(p.panel)
            .border_t_1()
            .border_b_1()
            .border_color(p.line)
            .text_size(m.text.xs)
            .font_weight(FontWeight::MEDIUM)
            .text_color(p.fg3)
            .on_drag_move(cx.listener(Self::on_column_drag))
            .child(
                refs_cell(&columns, m.zoom)
                    .relative()
                    .child(
                        truncated("column-refs", t!("graph.column.refs").into_owned())
                            .pl(px(2. * m.zoom)),
                    )
                    .child(measure_width(self.refs_drawn.clone())),
            )
            .child(handle(Column::Refs))
            .child(
                truncated("column-graph", t!("graph.column.graph").into_owned())
                    .w(columns.graph)
                    .flex_shrink_0()
                    .pl(px(4. * m.zoom)),
            )
            .child(handle(Column::Graph))
            .child(message_cell(columns.min_message).child(truncated(
                "column-message",
                t!("graph.column.message").into_owned(),
            )))
            .when_some(columns.author, |this, width| {
                this.child(handle(Column::Author)).child(
                    shrinkable(width, columns.min_column).child(truncated(
                        "column-author",
                        t!("graph.column.author").into_owned(),
                    )),
                )
            })
            .when_some(columns.date, |this, width| {
                this.child(handle(Column::Date))
                    .child(fixed(width).child(truncated(
                        "column-date",
                        t!("graph.column.date").into_owned(),
                    )))
            })
            .when_some(columns.sha, |this, width| {
                this.child(separator(columns.separator)).child(
                    fixed(width)
                        .child(truncated("column-sha", t!("graph.column.sha").into_owned())),
                )
            })
    }
}

impl Render for GraphView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (p, m) = (*cx.palette(), *cx.metrics());
        if self.graph.rows() == 0 {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .text_color(p.fg3)
                .child(t!("graph.empty").into_owned())
                .into_any_element();
        }
        v_flex()
            .size_full()
            .text_size(m.text.ui)
            // A column resize ends wherever the mouse is released; the width is saved then.
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.save_column(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.save_column(cx)),
            )
            .child(self.render_toolbar(cx))
            .child(
                // Graph shortcuts only apply while the list (not the search box) has focus.
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .key_context(CONTEXT)
                    .track_focus(&self.focus)
                    .on_action(cx.listener(Self::select_next))
                    .on_action(
                        cx.listener(|this, _: &ExtendNext, _, cx| this.extend(Step::Next, cx)),
                    )
                    .on_action(cx.listener(|this, _: &ExtendPrevious, _, cx| {
                        this.extend(Step::Previous, cx)
                    }))
                    .on_action(
                        cx.listener(|this, _: &ExtendFirst, _, cx| this.extend(Step::First, cx)),
                    )
                    .on_action(
                        cx.listener(|this, _: &ExtendLast, _, cx| this.extend(Step::Last, cx)),
                    )
                    .on_action(cx.listener(Self::select_previous))
                    .on_action(cx.listener(Self::select_first))
                    .on_action(cx.listener(Self::select_last))
                    .on_action(cx.listener(Self::page_down))
                    .on_action(cx.listener(Self::page_up))
                    .on_action(cx.listener(Self::focus_search))
                    .on_action(cx.listener(|this, _: &list_keys::RowMenu, _, cx| {
                        if this.selected.is_some() {
                            cx.emit(GraphEvent::RowMenu);
                        }
                    }))
                    .child(self.render_header(cx))
                    .children(self.render_wip_row(cx))
                    .child(
                        uniform_list(
                            "commit-graph",
                            self.graph.rows(),
                            cx.processor(Self::render_rows),
                        )
                        .track_scroll(&self.scroll)
                        .flex_1(),
                    ),
            )
            .into_any_element()
    }
}

impl Focusable for GraphView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// Drag payload of a commit row, to cherry-pick.
struct CommitDrag(Vec<RowCommit>);

/// Drag payload of a branch label, to merge.
#[derive(Debug, Clone, PartialEq)]
struct BranchDrag {
    full_name: SharedString,
    shown: SharedString,
}

/// What follows the pointer while something is dragged.
struct DragLabel(String);

impl Render for DragLabel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (p, m) = (*cx.palette(), *cx.metrics());
        div()
            .px_2()
            .py_0p5()
            .rounded(m.radius)
            .border_1()
            .border_color(p.acc)
            .bg(p.panel)
            .text_color(p.fg)
            .text_size(m.text.xs)
            .child(self.0.clone())
    }
}

/// The view menu's label: the branches walked, then the other options in force.
pub(crate) fn walk_summary(walk: &WalkOptions, tips: &TipFilter) -> String {
    let mut parts = vec![match &tips.solo {
        Some(name) => t!("graph.walk.short_solo", name = short_ref(name)),
        None if walk.current_branch => t!("graph.walk.short_current"),
        None => t!("graph.walk.short_all"),
    }];
    if tips.solo.is_none() && !tips.hidden.is_empty() {
        parts.push(tn!("graph.walk.short_hidden", count = tips.hidden.len()));
    }
    if walk.first_parent {
        parts.push(t!("graph.walk.short_first_parent"));
    }
    if walk.topo_order {
        parts.push(t!("graph.walk.short_topo"));
    }
    parts.join(" · ")
}

/// The view menu lists at most this many hidden refs.
const MAX_HIDDEN_ITEMS: usize = 20;

/// The view menu's item for a hidden entry: "Show topic", or for a whole remote's
/// branches (`refs/remotes/origin/`) "Show origin's branches".
fn show_hidden_label(entry: &str) -> String {
    match entry
        .strip_prefix("refs/remotes/")
        .and_then(|rest| rest.strip_suffix('/'))
    {
        Some(remote) => t!("graph.walk.show_remote", remote = remote),
        None => t!("graph.walk.show_ref", name = short_ref(entry)),
    }
    .into_owned()
}

/// A signed commit's mark before its summary: a check in the added-lines colour when git
/// trusts the signature, muted when it can't tell (an unknown or missing key), a cross in
/// the deleted-lines colour when it's bad, expired or revoked. The details panel says which.
fn signature_mark(status: SignatureStatus, p: &Palette, m: &Metrics) -> impl IntoElement {
    let (icon, color) = match status {
        SignatureStatus::Good => (AppIcon::Check, p.add),
        SignatureStatus::Untrusted | SignatureStatus::CannotCheck => (AppIcon::Check, p.fg3),
        SignatureStatus::Bad
        | SignatureStatus::ExpiredSignature
        | SignatureStatus::ExpiredKey
        | SignatureStatus::RevokedKey => (AppIcon::Close, p.del),
    };
    div()
        .flex_shrink_0()
        .child(Icon::new(icon).size(px(12. * m.zoom)).text_color(color))
}

/// A label's full ref name (none for HEAD's own label).
fn label_full_name(label: &RefLabel) -> Option<String> {
    match label.kind {
        RefKind::Local => Some(format!("refs/heads/{}", label.name)),
        RefKind::Remote => Some(format!("refs/remotes/{}", label.name)),
        RefKind::Tag => Some(format!("refs/tags/{}", label.name)),
        RefKind::Head => None,
    }
}

/// Whether `tips` hides the ref of `label` (its label then isn't drawn).
fn label_hidden(label: &RefLabel, tips: &TipFilter) -> bool {
    label_full_name(label).is_some_and(|name| tips.hides(name.as_bytes()))
}

/// A full ref name as the sidebar shows it: `refs/heads/x` → `x`, `refs/remotes/origin/x`
/// → `origin/x`, `refs/tags/v1` → `v1`.
pub(crate) fn short_ref(full_name: &str) -> &str {
    ["refs/heads/", "refs/remotes/", "refs/tags/"]
        .iter()
        .find_map(|prefix| full_name.strip_prefix(prefix))
        .unwrap_or(full_name)
}

/// The row menu's bisect items: how to start one, or how to mark a commit in one.
pub(crate) fn bisect_items(bisecting: bool) -> Vec<(BisectMark, String)> {
    if bisecting {
        vec![
            (BisectMark::Good, t!("bisect.mark_good").into_owned()),
            (BisectMark::Bad, t!("bisect.mark_bad").into_owned()),
            (BisectMark::Skip, t!("bisect.mark_skip").into_owned()),
        ]
    } else {
        vec![
            (BisectMark::Bad, t!("bisect.start_bad").into_owned()),
            (BisectMark::Good, t!("bisect.start_good").into_owned()),
        ]
    }
}

/// A bisect mark in the refs column: bad in red, good in green, skipped muted.
fn bisect_pill(mark: BisectMark, p: &Palette, m: &Metrics) -> impl IntoElement {
    let (text, color) = match mark {
        BisectMark::Bad => (t!("bisect.pill_bad"), p.del),
        BisectMark::Good => (t!("bisect.pill_good"), p.add),
        BisectMark::Skip => (t!("bisect.pill_skip"), p.fg3),
    };
    pill_frame(m)
        .flex_shrink_0()
        .border_color(color)
        .text_color(color)
        .child(text.into_owned())
}

/// A shallow clone's boundary in the refs column, muted: its parents aren't in this
/// clone (the banner offers to fetch them).
fn shallow_pill(p: &Palette, m: &Metrics) -> impl IntoElement {
    pill_frame(m)
        .flex_shrink_0()
        .border_color(p.fg3)
        .text_color(p.fg3)
        .child(t!("graph.shallow_pill").into_owned())
}

/// A row-menu item that emits `event` from the graph view.
fn event_item(
    view: &WeakEntity<GraphView>,
    label: String,
    event: impl Fn() -> GraphEvent + 'static,
) -> PopupMenuItem {
    let view = view.clone();
    PopupMenuItem::new(label).on_click(move |_, _, cx| {
        view.update(cx, |_, cx| cx.emit(event())).ok();
    })
}

/// A column that keeps `width` while there is room and shrinks (with ellipsis) when the
/// panel gets narrow, so the message column never disappears. Used for the branch and
/// author columns, whose text is still recognizable when shortened.
fn shrinkable(width: Pixels, min: Pixels) -> Div {
    div().w(width).min_w(min).truncate() // untooltipped: a cell; its text is `truncated`
}

/// The Branch / Tag cell of the header, the working changes and the commits. Flex shrinks
/// cells in proportion to their width inside the padding, so every row's must have the
/// same padding: otherwise, in a narrow panel, that row's lanes move against the others'.
fn refs_cell(columns: &ColumnLayout, zoom: f32) -> Div {
    shrinkable(columns.refs, columns.min_column).pl(px(REFS_INSET * zoom))
}

/// An invisible layer over its parent (which must be `relative`) that keeps the parent's
/// drawn width in `width`, drawing once more when it changed: the rows fit their pills to
/// it ([`pills_that_fit`]).
fn measure_width(width: Rc<Cell<Pixels>>) -> impl IntoElement {
    canvas(
        move |bounds, window, _| {
            if width.replace(bounds.size.width) != bounds.size.width {
                window.request_animation_frame();
            }
        },
        |_, (), _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// A column that always keeps its width (date and SHA are useless when cut).
fn fixed(width: Pixels) -> Div {
    div().w(width).flex_shrink_0().truncate() // untooltipped: a cell; its text is `truncated`
}

/// The message column: takes the remaining space, never below `min`.
fn message_cell(min: Pixels) -> Div {
    div().flex_1().min_w(min).truncate() // untooltipped: a cell; its text is `truncated`
}

fn separator(width: Pixels) -> Div {
    div().w(width).flex_shrink_0()
}

/// Header separator that resizes `column` when dragged; its line shows on hover.
fn resize_handle(column: Column, width: Pixels, line: Hsla) -> impl IntoElement {
    let id = match column {
        Column::Refs => "resize-refs",
        Column::Graph => "resize-graph",
        Column::Author => "resize-author",
        Column::Date => "resize-date",
    };
    div()
        .id(id)
        .group(id)
        .w(width)
        .h_full()
        .flex_shrink_0()
        .flex()
        .justify_center()
        .cursor(CursorStyle::ResizeLeftRight)
        .child(
            div()
                .w(px(1.))
                .h_full()
                .group_hover(id, move |style| style.bg(line)),
        )
        .on_drag(ColumnDrag(column), |_, _, _, cx| cx.new(|_| EmptyView))
}

/// A ref label as drawn: its text, built once per graph, and for a branch other than the
/// checked-out one, what dragging it onto the checked-out commit merges.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Pill {
    kind: RefKind,
    text: SharedString,
    /// Characters in `text`, for fitting pills into the column ([`pills_that_fit`]); a
    /// worktree's folder icon counts as two.
    chars: usize,
    drag: Option<BranchDrag>,
    /// The folder of another worktree that has this local branch checked out.
    worktree: Option<SharedString>,
}

impl Pill {
    /// The pill of a local branch checked out in another worktree (`folder`): a folder
    /// icon before its name.
    fn in_worktree(mut self, folder: SharedString) -> Self {
        self.chars += 2;
        self.worktree = Some(folder);
        self
    }

    pub(crate) fn new(label: &RefLabel) -> Self {
        // A branch and its remote branch on one commit are two pills, as designed.
        let text = label.name.clone();
        let full_name = match label.kind {
            RefKind::Local => Some(format!("refs/heads/{}", label.name)),
            RefKind::Remote => Some(format!("refs/remotes/{}", label.name)),
            RefKind::Head | RefKind::Tag => None,
        };
        Self {
            kind: label.kind,
            chars: text.chars().count(),
            text: text.into(),
            drag: full_name.map(|full_name| BranchDrag {
                full_name: full_name.into(),
                shown: label.name.clone().into(),
            }),
            worktree: None,
        }
    }
}

/// Each commit's pills: its labels less the hidden refs' (a hidden ref's label stays off
/// the commits other tips still reach), local branches checked out in other worktrees
/// (short name → folder) marked.
fn build_pills(
    graph: &GraphModel,
    tips: &TipFilter,
    worktrees: &HashMap<String, SharedString>,
) -> HashMap<ObjectId, Rc<[Pill]>> {
    graph
        .labels
        .iter()
        .map(|(id, labels)| {
            let shown = labels.iter().filter(|label| !label_hidden(label, tips));
            (
                *id,
                shown.map(|label| worktree_pill(label, worktrees)).collect(),
            )
        })
        .collect()
}

/// A label's pill, marked when it's a local branch checked out in another worktree
/// (`worktrees`: short name → folder name). The details panel uses it too.
pub(crate) fn worktree_pill(label: &RefLabel, worktrees: &HashMap<String, SharedString>) -> Pill {
    let pill = Pill::new(label);
    match worktrees.get(&label.name) {
        Some(folder) if label.kind == RefKind::Local => pill.in_worktree(folder.clone()),
        _ => pill,
    }
}

/// How many of a row's pills to show in `available` px (their `natural` widths, in
/// order): the first always, even cut; each next one only when it fits whole or at least
/// `minimum` wide, leaving room (`gap` + `more`) for the "+N" after it while more follow.
/// So a narrow column shows `main +1` rather than `m…` and `ori…`. The count is left out
/// (the second value) when even the first pill wouldn't keep `minimum` beside it: the pill
/// gets that room instead.
fn pills_that_fit(
    natural: &[f32],
    minimum: f32,
    available: f32,
    gap: f32,
    more: f32,
) -> (usize, bool) {
    let mut used = 0.0;
    let mut shown = 0;
    for (ix, &width) in natural.iter().enumerate() {
        let start = if ix == 0 { 0.0 } else { used + gap };
        let tail = if ix + 1 < natural.len() {
            gap + more
        } else {
            0.0
        };
        let room = available - start - tail;
        if ix > 0 && room < width.min(minimum) {
            break;
        }
        shown += 1;
        if room < width {
            break; // cut: nothing more fits after it
        }
        used = start + width;
    }
    let shown = shown.max(1).min(natural.len());
    let first = natural.first().map_or(0., |width| width.min(minimum));
    (shown, available >= first + gap + more)
}

/// A pill; local and remote branches other than the checked-out one can be dragged onto
/// the checked-out commit to merge them.
fn draggable_pill(pill: &Pill, id: usize, lane: Hsla, p: &Palette, m: &Metrics) -> AnyElement {
    let element = ref_pill(pill, lane, p, m);
    let Some(drag) = pill.drag.clone() else {
        return element.into_any_element();
    };
    let worktree = pill.worktree.clone();
    div()
        .id(("ref-pill", id))
        .min_w_0()
        .flex()
        .child(element)
        .on_drag(drag, |drag, _, _, cx| {
            let text = t!("graph.drag_branch", name = drag.shown.to_string());
            cx.new(|_| DragLabel(text.into_owned()))
        })
        .when_some(worktree, |this, folder| {
            let tip = t!("graph.in_worktree", folder = folder.as_ref()).into_owned();
            this.tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
        })
        .into_any_element()
}

/// The pill shape of ref labels and bisect marks: 20 px high, mono, outlined.
fn pill_frame(m: &Metrics) -> Div {
    let z = |value: f32| px(value * m.zoom);
    div()
        .h(m.pill)
        .min_w_0()
        .max_w(z(PILL_MAX_WIDTH))
        .px(z(7.))
        .rounded(z(5.))
        .border_1()
        .line_height(m.pill - m.dp(2.))
        .font_family(mono_font())
        .text_size(m.text.mono_small)
        .font_weight(FontWeight::MEDIUM)
        .truncate() // untooltipped: a frame; a ref's name in it is `truncated`
}

/// Branches are outlined in their lane color and the checked-out one is filled; tags sit
/// on the chip color. The details panel shows them too.
pub(crate) fn ref_pill(pill: &Pill, lane: Hsla, p: &Palette, m: &Metrics) -> Div {
    let (bg, fg, border) = match pill.kind {
        RefKind::Head => (lane, gitweft_ui::palette::ink_on(lane), lane),
        RefKind::Tag => (p.chip, p.fg2, p.chip),
        RefKind::Local | RefKind::Remote => (transparent_black(), lane, lane),
    };
    pill_frame(m)
        .bg(bg)
        .text_color(fg)
        .border_color(border)
        .when(pill.kind == RefKind::Head, |this| {
            this.font_weight(FontWeight::SEMIBOLD)
        })
        .map(|this| match &pill.worktree {
            // Checked out in another worktree: a folder icon, then the name cut to fit.
            Some(_) => this
                .flex()
                .items_center()
                .gap(px(3. * m.zoom))
                .child(
                    Icon::new(AppIcon::Folder)
                        .size(px(11. * m.zoom))
                        .flex_shrink_0(),
                )
                .child(truncated(pill_id(pill), pill.text.clone())),
            None => this.child(truncated(pill_id(pill), pill.text.clone())),
        })
}

/// A pill's text's id: its ref's name, unique among a row's (or a commit's) pills.
fn pill_id(pill: &Pill) -> ElementId {
    ElementId::Name(format!("pill:{}", pill.text).into())
}

/// Sizes and colors of the graph column, resolved once per frame.
#[derive(Clone, Copy)]
struct GraphStyle {
    lanes: [Hsla; LANES],
    /// Fills hollow merge nodes and cuts HEAD's node out of its lines.
    bg: Hsla,
    /// The "Working changes" node and its dashed line.
    wip: Hsla,
    inset: Pixels,
    lane: Pixels,
    stroke: Pixels,
    thin: Pixels,
    dash: Pixels,
    gap: Pixels,
    node: Pixels,
    head_node: Pixels,
    head_ring: Pixels,
}

impl GraphStyle {
    fn new(p: &Palette, m: &Metrics) -> Self {
        let z = |value: f32| px(value * m.zoom);
        Self {
            lanes: p.lanes,
            bg: p.bg,
            wip: p.fg3,
            inset: z(LANE_INSET),
            lane: m.lane,
            stroke: m.stroke,
            thin: z(THIN_STROKE),
            dash: z(2.),
            gap: z(3.),
            node: m.node,
            head_node: m.head_node,
            head_ring: m.head_ring,
        }
    }

    fn color(&self, id: u16) -> Hsla {
        self.lanes[usize::from(id % PALETTE_SIZE)]
    }

    fn lane_x(&self, bounds: &Bounds<Pixels>, lane: u16) -> Pixels {
        bounds.origin.x + self.inset + self.lane * f32::from(lane)
    }
}

/// What one row of the graph column needs to paint itself.
struct RowGeometry {
    row: usize,
    node: Node,
    is_merge: bool,
    /// The checked-out commit: a bigger node with a ring.
    is_head: bool,
    /// The dashed line from the "Working changes" row comes down to this row's node.
    wip_above: bool,
    /// The dashed line passes this row (above HEAD's) in this lane.
    wip_through: Option<u16>,
    spans: Vec<Span>,
    connectors: Vec<Connector>,
    style: GraphStyle,
}

impl RowGeometry {
    fn paint(&self, bounds: Bounds<Pixels>, window: &mut Window) {
        // Lanes wider than the column are clipped instead of drawn over the text.
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            self.paint_unclipped(bounds, window)
        });
    }

    fn paint_unclipped(&self, bounds: Bounds<Pixels>, window: &mut Window) {
        let style = &self.style;
        let lane_x = |lane: u16| style.lane_x(&bounds, lane);
        let visible = |lane: u16| lane < MAX_DRAWN_LANES;
        let half = bounds.size.height / 2.;
        let top_half_row = 2 * self.row as u32;
        let y = |half_row: u32| bounds.origin.y + half * (half_row - top_half_row) as f32;
        let (top, center, bottom) = (y(top_half_row), y(top_half_row + 1), y(top_half_row + 2));
        let node_x = lane_x(self.node.lane);

        for span in self.spans.iter().filter(|s| visible(s.lane)) {
            let from = span.from.max(top_half_row);
            let to = span.to.min(top_half_row + 2);
            let x = lane_x(span.lane);
            window.paint_quad(fill(
                Bounds::from_corners(
                    point(x - style.stroke / 2., y(from)),
                    point(x + style.stroke / 2., y(to)),
                ),
                style.color(span.color),
            ));
        }

        for connector in self.connectors.iter().filter(|c| visible(c.lane)) {
            let lane = lane_x(connector.lane);
            // Curves leave/enter lanes vertically and the node horizontally.
            let (from, to, ctrl) = match connector.kind {
                ConnectorKind::MergeIn => {
                    (point(lane, top), point(node_x, center), point(lane, center))
                }
                ConnectorKind::BranchOut => (
                    point(node_x, center),
                    point(lane, bottom),
                    point(lane, center),
                ),
            };
            let mut path = PathBuilder::stroke(style.stroke);
            path.move_to(from);
            path.curve_to(to, ctrl);
            if let Ok(path) = path.build() {
                window.paint_path(path, style.color(connector.color));
            }
        }

        // Where no line runs in HEAD's column, the dashed line from the working changes
        // shows the way down to it.
        if let Some(lane) = self.wip_through
            && visible(lane)
            && dash_passes(self.node.lane, &self.spans, lane)
        {
            paint_dashed(window, lane_x(lane), top, bottom, style);
        }

        if !visible(self.node.lane) {
            return;
        }
        let lane_color = style.color(self.node.color);
        let at = point(node_x, center);
        // Radii are to the middle of the stroke, as in the design's SVG.
        let edge = style.stroke / 2.;
        if self.is_head {
            let ring = style.head_ring + style.thin / 2.;
            paint_circle(
                window,
                at,
                style.head_node + edge,
                lane_color,
                style.stroke,
                style.bg,
            );
            paint_ring(window, at, ring, style.thin, lane_color, BorderStyle::Solid);
            // Not over a line that already comes down to HEAD from above.
            let line_above = self
                .spans
                .iter()
                .any(|span| span.lane == self.node.lane && span.from <= top_half_row);
            if self.wip_above && !line_above {
                paint_dashed(window, node_x, top, center - ring, style);
            }
        } else if self.is_merge {
            paint_circle(
                window,
                at,
                style.node + edge,
                style.bg,
                style.stroke,
                lane_color,
            );
        } else {
            paint_circle(
                window,
                at,
                style.node + edge,
                lane_color,
                px(0.),
                lane_color,
            );
        }
    }
}

/// A filled circle of `radius` (to the outer edge) with a `border` inside it.
fn paint_circle(
    window: &mut Window,
    center: Point<Pixels>,
    radius: Pixels,
    fill: Hsla,
    border: Pixels,
    border_color: Hsla,
) {
    window.paint_quad(quad(
        Bounds::new(
            point(center.x - radius, center.y - radius),
            size(radius * 2., radius * 2.),
        ),
        radius,
        fill,
        border,
        border_color,
        BorderStyle::Solid,
    ));
}

/// An unfilled circle: `radius` to the outer edge, a line `width` wide.
fn paint_ring(
    window: &mut Window,
    center: Point<Pixels>,
    radius: Pixels,
    width: Pixels,
    color: Hsla,
    border: BorderStyle,
) {
    window.paint_quad(quad(
        Bounds::new(
            point(center.x - radius, center.y - radius),
            size(radius * 2., radius * 2.),
        ),
        radius,
        transparent_black(),
        width,
        color,
        border,
    ));
}

/// Whether the dashed line to HEAD shows in a row: its column (`lane`) is empty there, no
/// line of `spans` (the row's) and not the row's own node.
fn dash_passes(node_lane: u16, spans: &[Span], lane: u16) -> bool {
    node_lane != lane && !spans.iter().any(|span| span.lane == lane)
}

/// The dashed vertical line between the "Working changes" node and HEAD.
fn paint_dashed(window: &mut Window, x: Pixels, from: Pixels, to: Pixels, style: &GraphStyle) {
    if from >= to {
        return;
    }
    let mut path = PathBuilder::stroke(style.thin).dash_array(&[style.dash, style.gap]);
    path.move_to(point(x, from));
    path.line_to(point(x, to));
    if let Ok(path) = path.build() {
        window.paint_path(path, style.wip);
    }
}

/// The "Working changes" node: a dashed, hollow circle in HEAD's lane, with a dashed line
/// down to HEAD when it is the next row.
fn paint_wip_node(
    bounds: Bounds<Pixels>,
    lane: u16,
    link: bool,
    style: &GraphStyle,
    window: &mut Window,
) {
    let x = style.lane_x(&bounds, lane);
    let y = bounds.origin.y + bounds.size.height / 2.;
    let radius = style.node + style.thin / 2.;
    paint_ring(
        window,
        point(x, y),
        radius,
        style.thin,
        style.wip,
        BorderStyle::Dashed,
    );
    if link {
        paint_dashed(window, x, y + radius, bounds.bottom(), style);
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: GPUI's `test` macro would shadow `#[test]`.
    use super::{
        Columns, MAX_COLUMN, MIN_COLUMN, Pill, Span, dash_passes, design_width, label_hidden,
        short_ref, show_hidden_label, walk_summary,
    };
    use super::{Step, extend_range};
    use gitweft_core::repo::{RefKind, RefLabel};
    use gitweft_core::settings::GraphSettings;
    use gitweft_git::read::{TipFilter, WalkOptions};
    use gpui_kit::px;

    #[test]
    fn hidden_refs_lose_their_labels_and_the_menu_names_them() {
        let tips = TipFilter {
            hidden: vec![
                "refs/heads/topic".into(),
                "refs/remotes/origin/".into(),
                "refs/tags/v1".into(),
            ],
            solo: None,
        };
        let hidden = |name: &str, kind| label_hidden(&label(name, kind), &tips);
        assert!(hidden("topic", RefKind::Local));
        assert!(!hidden("main", RefKind::Local));
        assert!(hidden("origin/main", RefKind::Remote));
        assert!(!hidden("upstream/main", RefKind::Remote));
        assert!(hidden("v1", RefKind::Tag));
        assert!(!hidden("HEAD", RefKind::Head));
        assert_eq!(show_hidden_label("refs/heads/topic"), "Show topic");
        assert_eq!(
            show_hidden_label("refs/remotes/origin/"),
            "Show origin's branches"
        );
        assert_eq!(show_hidden_label("refs/remotes/origin/x"), "Show origin/x");
    }

    fn label(name: &str, kind: RefKind) -> RefLabel {
        RefLabel {
            name: name.into(),
            kind,
        }
    }

    #[test]
    fn pills_that_dont_fit_become_the_count() {
        use super::pills_that_fit;
        // main (42) and origin/main (89), 4 px apart, "+N" 20 px, at least 40 px each.
        let shown =
            |natural: &[f32], available: f32| pills_that_fit(natural, 40., available, 4., 20.);
        let pills = [42., 89.];
        assert_eq!(shown(&pills, 200.), (2, true));
        assert_eq!(shown(&pills, 100.), (2, true), "the second cut to 54");
        assert_eq!(shown(&pills, 80.), (1, true), "main +1");
        // Too narrow for the first pill and the count: the pill keeps the room.
        assert_eq!(shown(&pills, 40.), (1, false));
        // Three: the second fits only while the count still fits after it.
        let three = [42., 42., 42.];
        assert_eq!(shown(&three, 110.), (2, true));
        assert_eq!(shown(&three, 134.).0, 3);
        assert_eq!(shown(&[], 100.).0, 0);
    }

    #[test]
    fn pills_name_their_ref_and_only_other_branches_drag() {
        let main = Pill::new(&label("main", RefKind::Local));
        assert_eq!(main.text.as_ref(), "main");
        let drag = main.drag.expect("a local branch drags");
        assert_eq!(drag.full_name.as_ref(), "refs/heads/main");
        assert_eq!(drag.shown.as_ref(), "main");

        // In sync or not, the remote branch is a pill of its own.
        let remote = Pill::new(&label("origin/main", RefKind::Remote));
        assert_eq!(remote.text.as_ref(), "origin/main");
        let drag = remote.drag.expect("a remote branch drags");
        assert_eq!(drag.full_name.as_ref(), "refs/remotes/origin/main");

        let head = Pill::new(&label("work", RefKind::Head));
        assert!(head.drag.is_none());
        let tag = Pill::new(&label("v1.0", RefKind::Tag));
        assert_eq!(tag.text.as_ref(), "v1.0");
        assert!(tag.drag.is_none());
    }

    #[test]
    fn the_dashed_line_to_head_runs_only_where_its_column_is_empty() {
        let span = |lane: u16| Span {
            lane,
            color: 0,
            from: 0,
            to: 4,
        };
        // HEAD's column is 1: free in a row whose node and lines are elsewhere.
        assert!(dash_passes(0, &[span(0), span(2)], 1));
        // Not over a line in that column, nor over another commit's node there.
        assert!(!dash_passes(0, &[span(1)], 1));
        assert!(!dash_passes(1, &[], 1));
    }

    #[test]
    fn columns_are_the_design_until_dragged() {
        let columns = Columns::from_settings(&GraphSettings::default(), None);
        assert_eq!(
            (columns.refs, columns.author, columns.date),
            (200., 150., 140.)
        );
        let dragged = GraphSettings {
            refs_width: Some(260.),
            ..GraphSettings::default()
        };
        let columns = Columns::from_settings(&dragged, Some(90.));
        assert_eq!((columns.refs, columns.graph), (260., Some(90.)));
    }

    #[test]
    fn the_view_menu_label_names_the_options_in_force() {
        let none = TipFilter::default();
        assert_eq!(walk_summary(&WalkOptions::default(), &none), "All branches");
        let all = WalkOptions {
            first_parent: true,
            current_branch: true,
            topo_order: true,
        };
        assert_eq!(
            walk_summary(&all, &none),
            "Current branch · First parent · Topological"
        );
        let hidden = TipFilter {
            hidden: vec!["refs/heads/a".into(), "refs/remotes/origin/".into()],
            solo: None,
        };
        assert_eq!(
            walk_summary(&WalkOptions::default(), &hidden),
            "All branches · 2 hidden"
        );
        // A solo replaces the branches walked, and the hidden ones don't matter then.
        let solo = TipFilter {
            solo: Some("refs/remotes/origin/feature/x".into()),
            ..hidden
        };
        assert_eq!(
            walk_summary(&all, &solo),
            "Solo: origin/feature/x · First parent · Topological"
        );
    }

    #[test]
    fn short_ref_names_drop_the_namespace() {
        assert_eq!(short_ref("refs/heads/feature/x"), "feature/x");
        assert_eq!(short_ref("refs/remotes/origin/main"), "origin/main");
        assert_eq!(short_ref("refs/tags/v1.0"), "v1.0");
        assert_eq!(short_ref("refs/notes/commits"), "refs/notes/commits");
    }

    #[test]
    fn dragged_widths_are_saved_unzoomed_and_limited() {
        assert_eq!(design_width(px(300.), 1.5), 200.);
        assert_eq!(design_width(px(10.), 1.0), MIN_COLUMN);
        assert_eq!(design_width(px(5000.), 1.0), MAX_COLUMN);
    }

    /// Shift with the arrows, Home and End moves one end of the range; the other stays on
    /// the selected commit.
    #[test]
    fn a_shift_range_moves_its_end_and_keeps_the_anchor() {
        let rows = 10;
        assert_eq!(extend_range(4, 4, Step::Next, rows), (5, 4..=5));
        assert_eq!(extend_range(4, 5, Step::Next, rows), (6, 4..=6));
        // Back past the anchor: the range turns around it.
        assert_eq!(extend_range(4, 4, Step::Previous, rows), (3, 3..=4));
        assert_eq!(extend_range(4, 5, Step::Previous, rows), (4, 4..=4));
        assert_eq!(extend_range(4, 7, Step::First, rows), (0, 0..=4));
        assert_eq!(extend_range(4, 0, Step::Last, rows), (9, 4..=9));
        // Clamped at the ends.
        assert_eq!(extend_range(9, 9, Step::Next, rows), (9, 9..=9));
        assert_eq!(extend_range(0, 0, Step::Previous, rows), (0, 0..=0));
    }
}

/// The graph in a GPUI test window: several commits selected with Ctrl+click and
/// Shift+click, and dragged onto the checked-out one, through the rows' own mouse handlers.
#[cfg(test)]
mod view_tests {
    use std::cell::RefCell;

    use gitweft_core::repo::{LoadEnv, load_repo_in};
    use gitweft_git::CancelToken;
    use gitweft_testkit::Sandbox;
    use gitweft_ui::SettingsStore;
    // Rust's own `#[test]`: `gpui_kit::*` (through `super::*`) brings GPUI's `test` macro.
    use std::prelude::v1::test;

    use super::*;

    /// What the graph emitted about selections and drops, by commit summary.
    #[derive(Debug, PartialEq)]
    enum Seen {
        Selected(String),
        Multi(Vec<String>),
        Dropped(Vec<String>),
    }

    fn names(summaries: &[&str]) -> Vec<String> {
        summaries.iter().map(|s| s.to_string()).collect()
    }

    fn summaries(commits: &[RowCommit]) -> Vec<String> {
        commits
            .iter()
            .map(|commit| commit.summary.clone())
            .collect()
    }

    /// The graph of c1 to c4 on main (c4 checked out, in row 0; c1 in row 3) in a test
    /// window, drawn; returns its window and what it emits.
    fn graph<'a>(
        cx: &'a mut TestAppContext,
        sb: &Sandbox,
    ) -> (&'a mut VisualTestContext, Rc<RefCell<Vec<Seen>>>) {
        let repo = sb.init_repo("r");
        for n in 1..=4 {
            sb.commit_file(&repo, "f.txt", &format!("{n}\n"), &format!("c{n}"));
        }
        let env = LoadEnv::isolated(sb.path().join("data"));
        let loaded = load_repo_in(&repo, &env, None, None, &CancelToken::new()).unwrap();
        let summary: HashMap<ObjectId, String> = loaded
            .graph
            .commits
            .iter()
            .zip(loaded.metas.iter().map(|meta| meta.summary.to_string()))
            .collect();
        cx.update(|cx| {
            gpui_kit::init(cx);
            SettingsStore::init_with_file(cx, &sb.path().join("settings.json"));
        });
        let (view, window) = cx.add_window_view(|window, cx| {
            let search = cx.new(|cx| InputState::new(window, cx));
            let signatures = SignatureCache::default();
            let (walk, tips) = (loaded.walk, loaded.tips);
            GraphView::new(
                loaded.graph,
                loaded.metas,
                search,
                walk,
                tips,
                None,
                signatures,
                cx,
            )
        });
        let seen = Rc::new(RefCell::new(Vec::new()));
        let sink = seen.clone();
        window.update(|window, cx| {
            cx.subscribe(&view, move |_, event: &GraphEvent, _| {
                let item = match event {
                    GraphEvent::Selected(id) => Seen::Selected(summary[id].clone()),
                    GraphEvent::MultiSelected(commits) => Seen::Multi(summaries(commits)),
                    GraphEvent::DropCommits(commits) => Seen::Dropped(summaries(commits)),
                    _ => return,
                };
                sink.borrow_mut().push(item);
            })
            .detach();
            window.draw(cx).clear(cx);
        });
        window.run_until_parked();
        (window, seen)
    }

    /// Draws a frame: the rows build their drag payload (the selection) as they're drawn.
    fn redraw(window: &mut VisualTestContext) {
        window.update(|window, cx| window.draw(cx).clear(cx));
    }

    /// The middle of commit row `row`, as last drawn.
    fn row_at(window: &mut VisualTestContext, row: usize) -> Point<Pixels> {
        const ROWS: [&str; 4] = ["graph-row-0", "graph-row-1", "graph-row-2", "graph-row-3"];
        window
            .debug_bounds(ROWS[row])
            .expect("the row is drawn")
            .center()
    }

    fn click(window: &mut VisualTestContext, row: usize, modifiers: Modifiers) {
        let at = row_at(window, row);
        window.simulate_click(at, modifiers);
        redraw(window);
    }

    /// Drags row `from` onto row `to` as a hand would: pressed, moved past GPUI's drag
    /// threshold, over the target, released there.
    fn drag(window: &mut VisualTestContext, from: usize, to: usize) {
        let (start, end) = (row_at(window, from), row_at(window, to));
        let none = Modifiers::none();
        window.simulate_mouse_down(start, MouseButton::Left, none);
        let nudged = start + point(px(0.), px(5.));
        window.simulate_mouse_move(nudged, MouseButton::Left, none);
        window.simulate_mouse_move(end, MouseButton::Left, none);
        window.simulate_mouse_up(end, MouseButton::Left, none);
        redraw(window);
    }

    #[test]
    fn ctrl_click_adds_and_removes_commits_and_shift_click_takes_a_range() {
        let sb = Sandbox::new();
        let mut cx = TestAppContext::single();
        let (window, seen) = graph(&mut cx, &sb);
        let (plain, ctrl, shift) = (Modifiers::none(), Modifiers::control(), Modifiers::shift());

        click(window, 1, plain); // c3
        click(window, 3, ctrl); // and c1
        click(window, 2, ctrl); // and c2
        click(window, 3, ctrl); // c1 out again
        click(window, 2, ctrl); // c2 out: c3 alone, a single selection
        click(window, 3, shift); // c3 to c1
        click(window, 0, plain); // c4 alone

        assert_eq!(
            *seen.borrow(),
            [
                Seen::Selected("c3".into()),
                Seen::Multi(names(&["c1", "c3"])),
                Seen::Multi(names(&["c1", "c2", "c3"])),
                Seen::Multi(names(&["c2", "c3"])),
                Seen::Selected("c3".into()),
                Seen::Multi(names(&["c1", "c2", "c3"])),
                Seen::Selected("c4".into()),
            ]
        );
    }

    /// Shift with the arrows extends the selection from the clicked commit and back,
    /// through the graph's own key bindings.
    #[test]
    fn shift_arrows_extend_the_selection_and_shrink_it_back() {
        let sb = Sandbox::new();
        let mut cx = TestAppContext::single();
        cx.update(bind_keys);
        let (window, seen) = graph(&mut cx, &sb);

        click(window, 1, Modifiers::none()); // c3, which also gives the graph the keys
        window.simulate_keystrokes("shift-down shift-down");
        redraw(window);
        window.simulate_keystrokes("shift-up shift-up shift-up");
        redraw(window);
        window.simulate_keystrokes("shift-end");
        redraw(window);

        assert_eq!(
            *seen.borrow(),
            [
                Seen::Selected("c3".into()),
                Seen::Multi(names(&["c2", "c3"])),
                Seen::Multi(names(&["c1", "c2", "c3"])),
                Seen::Multi(names(&["c2", "c3"])),
                Seen::Selected("c3".into()),
                // Past the anchor, the range turns around it: c3 and c4 above.
                Seen::Multi(names(&["c3", "c4"])),
                Seen::Multi(names(&["c1", "c2", "c3"])),
            ]
        );
    }

    #[test]
    fn a_selected_row_dropped_on_the_checked_out_commit_takes_the_others_along() {
        let sb = Sandbox::new();
        let mut cx = TestAppContext::single();
        let (window, seen) = graph(&mut cx, &sb);

        click(window, 1, Modifiers::none()); // c3
        click(window, 2, Modifiers::control()); // and c2
        drag(window, 2, 0); // c2 onto c4: both go, oldest first
        drag(window, 3, 0); // c1, not selected: alone

        let dropped: Vec<_> = seen
            .borrow_mut()
            .drain(..)
            .filter(|seen| matches!(seen, Seen::Dropped(_)))
            .collect();
        assert_eq!(
            dropped,
            [
                Seen::Dropped(names(&["c2", "c3"])),
                Seen::Dropped(names(&["c1"])),
            ]
        );
    }
}
