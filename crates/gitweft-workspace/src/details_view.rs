//! Right panel: details of the selected commit, or of a stash picked in the sidebar, with a
//! preview of the selected file's diff at the bottom.

use std::cell::{Cell, OnceCell};
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;

use gitweft_core::repo::{RefKind, RefLabel};
use gitweft_git::ObjectId;
use gitweft_git::display::columns;
use gitweft_git::gix::bstr::BString;
use gitweft_git::read::{
    ChangeKind, CommitDetails, CommitSignature, FileChange, FileDiff, LineCount, LineKind,
    Signature, SignatureStatus,
};
use gitweft_git::stash::{StashContents, StashEntry};
use gitweft_syntax::Spans;
use gitweft_ui::current::mono_font;

use crate::code_scroll::{self, HScroll};
use crate::diff_view::rgb;
use crate::graph_view::RowCommit;
use gitweft_ui::truncated::truncated;
use gitweft_ui::widgets::{LinkButton, path_label, status_badge};
use gitweft_ui::{AppIcon, LookExt, Metrics, Palette};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::{Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::graph_view::{ref_pill, worktree_pill};
use crate::list_keys::{self, FirstRow, LastRow, NextRow, OpenRow, PreviousRow, RowMenu};
use crate::split::{self, Split};
use crate::util::{avatar, format_time};

/// Diff lines the preview draws; the full diff shows the rest.
const PREVIEW_LINES: usize = 400;
/// A commit body longer than this (lines, or characters) is shown cut, with "Show more".
const BODY_LINES: usize = 8;
const BODY_CHARS: usize = 600;

pub enum DetailsState {
    Empty,
    Loaded(Box<LoadedCommit>),
    /// A stash picked in the sidebar: what it holds.
    Stash(Box<StashDetails>),
    /// Several commits selected in the graph, oldest first.
    Several(Vec<RowCommit>),
    Failed(String),
}

/// A commit as the panel shows it: its details, and the labels and lane color it has in
/// the graph.
pub struct LoadedCommit {
    pub details: CommitDetails,
    pub refs: Vec<RefLabel>,
    /// Index into the palette's lane colors.
    pub lane: usize,
}

/// A stash and what it holds, for the panel.
pub struct StashDetails {
    pub entry: StashEntry,
    /// Its name for people (`workspace::stash_label`).
    pub label: String,
    pub contents: StashContents,
}

#[derive(Clone)]
pub enum DetailsEvent {
    /// Show the diff of a file changed by `commit` (against its first parent).
    OpenFile {
        commit: ObjectId,
        parent: Option<ObjectId>,
        change: FileChange,
    },
    /// A file was selected: read its diff for the preview ([`DetailsView::set_preview`]).
    Preview {
        commit: ObjectId,
        parent: Option<ObjectId>,
        change: FileChange,
    },
    /// Show who last changed each line of `path` as of `commit` (bytes: a name needn't be UTF-8).
    Blame { commit: ObjectId, path: BString },
    /// List the commits that changed `path`, up to `commit`.
    History { commit: ObjectId, path: BString },
    /// Browse the files of `commit`.
    BrowseFiles(ObjectId),
    /// A parent was clicked: select it in the graph.
    SelectCommit(ObjectId),
    /// The header's ×: back to the working changes, or to no selection.
    Close,
    /// "Cherry-pick N commits" under several selected commits.
    CherryPick(Vec<RowCommit>),
    /// "Save N commits as patches…" under several selected commits.
    SavePatches(Vec<RowCommit>),
    /// Shift+F10 or the menu key in the file list: offer the previewed file's row menu.
    /// Its items are [`file_menu_items`]`(commit, &change)`, each a label and the event
    /// that runs it (handled like the context menu's). `parent` is what the file's diff is
    /// read against, as in [`DetailsEvent::OpenFile`].
    FileMenu {
        commit: ObjectId,
        parent: Option<ObjectId>,
        change: FileChange,
    },
    /// Open the file's change in git's diff tool: `commit` against `parent`.
    DiffTool {
        commit: ObjectId,
        parent: Option<ObjectId>,
        change: FileChange,
    },
    /// Open the file as it is in the working tree: in the editor (`editor`), else in the
    /// file manager.
    WorkingFile { path: BString, editor: bool },
    /// The stash panel's Apply (`pop`: Pop, which also drops it).
    StashApply { entry: StashEntry, pop: bool },
    /// The stash panel's Drop…: asks first.
    StashDrop(StashEntry),
}

impl EventEmitter<DetailsEvent> for DetailsView {}

pub struct DetailsView {
    state: DetailsState,
    /// The file the preview shows: the commit its diff is read from, and its path as bytes
    /// (a path needn't be UTF-8, and the rows compare their own paths with it).
    selected: Option<(ObjectId, BString)>,
    /// Its diff once read; `None` while it loads.
    preview: Option<Result<Preview, String>>,
    /// The file list's focus (its keys). Made on first use: `new` has no context.
    focus: OnceCell<FocusHandle>,
    /// The file list's scroll: a key that moves the preview keeps its file in view.
    files_scroll: UniformListScrollHandle,
    /// The details above the file list, which are cut at 60% of the panel: a link Tab
    /// reaches down there is scrolled into view.
    top_scroll: ScrollHandle,
    /// A section divider being dragged, and where to; saved when it's released.
    split_drag: Option<(Split, f32)>,
    /// The panel's height as drawn last: the message and facts' 60 % cap is taken of it
    /// (a percentage of the column flex sizes came out far too small).
    panel_height: Rc<Cell<Pixels>>,
    /// The preview's lines scrolled sideways (as in the code views).
    hscroll: HScroll,
    /// A long commit body shown whole ("Show more"); back to cut for the next commit.
    body_open: bool,
    /// Local branches checked out in other worktrees (short name → folder name): their
    /// pills are marked, as in the graph.
    worktrees: HashMap<String, SharedString>,
}

impl Focusable for DetailsView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.focus.get_or_init(|| cx.focus_handle()).clone()
    }
}

impl DetailsView {
    /// The stash the panel shows, if it shows one.
    pub fn shown_stash(&self) -> Option<&StashEntry> {
        match &self.state {
            DetailsState::Stash(stash) => Some(&stash.entry),
            _ => None,
        }
    }

    pub fn new() -> Self {
        Self {
            state: DetailsState::Empty,
            selected: None,
            preview: None,
            focus: OnceCell::new(),
            files_scroll: UniformListScrollHandle::new(),
            top_scroll: ScrollHandle::new(),
            split_drag: None,
            panel_height: Rc::new(Cell::new(Pixels::ZERO)),
            hscroll: HScroll::default(),
            body_open: false,
            worktrees: HashMap::new(),
        }
    }

    /// The local branches checked out in other worktrees, for their pills' mark.
    pub fn set_worktrees(
        &mut self,
        worktrees: HashMap<String, SharedString>,
        cx: &mut Context<Self>,
    ) {
        if self.worktrees != worktrees {
            self.worktrees = worktrees;
            cx.notify();
        }
    }

    /// Shows a commit or a stash, and previews the file previewed before when it has it
    /// too, else its first file.
    pub fn set_state(&mut self, state: DetailsState, cx: &mut Context<Self>) {
        let previous = self.selected.take().map(|(_, path)| path);
        self.state = state;
        self.preview = None;
        self.body_open = false;
        let previous = previous.as_ref().map(|path| path.as_slice());
        if let Some((commit, parent, change)) = self.initial_file(previous) {
            self.select(commit, parent, change, cx);
        }
        self.reveal_selected();
        cx.notify();
    }

    /// The preview's diff arrived, with its lines' syntax colors (`[hunk][line]`, as
    /// `diff_decor` makes them) and the tab width they were expanded with. Dropped when
    /// another file was selected meanwhile.
    pub fn set_preview(
        &mut self,
        commit: ObjectId,
        path: &[u8],
        diff: Result<(FileDiff, Vec<Vec<Spans>>, usize), String>,
        cx: &mut Context<Self>,
    ) {
        let current = self
            .selected
            .as_ref()
            .is_some_and(|(id, selected)| *id == commit && selected.as_slice() == path);
        if current {
            self.preview = Some(
                diff.map(|(diff, syntax, tab)| Preview::new(&diff, &syntax, PREVIEW_LINES, tab)),
            );
            cx.notify();
        }
    }

    /// Selects a file for the preview and asks for its diff.
    fn select(
        &mut self,
        commit: ObjectId,
        parent: Option<ObjectId>,
        change: FileChange,
        cx: &mut Context<Self>,
    ) {
        self.selected = Some((commit, change.path.clone()));
        self.preview = None;
        cx.emit(DetailsEvent::Preview {
            commit,
            parent,
            change,
        });
        cx.notify();
    }

    /// The file to preview first: `previous` when this commit has it too, else the first.
    fn initial_file(
        &self,
        previous: Option<&[u8]>,
    ) -> Option<(ObjectId, Option<ObjectId>, FileChange)> {
        let files = self.files();
        let found = previous.and_then(|path| files.iter().find(|(_, _, c)| c.path == path));
        found
            .or(files.first())
            .map(|&(commit, parent, change)| (commit, parent, change.clone()))
    }

    /// Every file shown, in order, with the commit and parent its diff is read against.
    fn files(&self) -> Vec<(ObjectId, Option<ObjectId>, &FileChange)> {
        self.file_lists()
            .into_iter()
            .flat_map(|(commit, parent, changes)| {
                changes.iter().map(move |change| (commit, parent, change))
            })
            .collect()
    }

    /// The previewed file's place in `files` ([`Self::files`]).
    fn selected_index(&self, files: &[(ObjectId, Option<ObjectId>, &FileChange)]) -> Option<usize> {
        let (commit, path) = self.selected.as_ref()?;
        files
            .iter()
            .position(|(id, _, change)| id == commit && change.path == *path)
    }

    /// The previewed file, with the commit and parent its diff is read against.
    pub(crate) fn selected_file(&self) -> Option<(ObjectId, Option<ObjectId>, FileChange)> {
        let files = self.files();
        let (commit, parent, change) = files[self.selected_index(&files)?];
        Some((commit, parent, change.clone()))
    }

    /// The list's arrows, Home and End: previews the file `to` picks from the previewed
    /// one's place (`None` without one) and the number of files, like a click, and keeps
    /// it in view.
    fn select_row(
        &mut self,
        to: impl FnOnce(Option<usize>, usize) -> usize,
        cx: &mut Context<Self>,
    ) {
        let (commit, parent, change) = {
            let files = self.files();
            if files.is_empty() {
                return;
            }
            let current = self.selected_index(&files);
            let ix = to(current, files.len()).min(files.len() - 1);
            if Some(ix) == current {
                return;
            }
            let (commit, parent, change) = files[ix];
            (commit, parent, change.clone())
        };
        self.select(commit, parent, change, cx);
        self.reveal_selected();
    }

    /// Scrolls the file list by the least that shows the previewed file (to the top
    /// without one).
    fn reveal_selected(&self) {
        let files = self.files();
        let item = self
            .selected_index(&files)
            .map_or(0, |ix| self.list_item(ix));
        self.files_scroll
            .scroll_to_item(item, ScrollStrategy::Nearest);
    }

    /// Where file `ix` of [`Self::files`] is in the list: a stash's untracked files come
    /// after their header row.
    fn list_item(&self, ix: usize) -> usize {
        match &self.state {
            DetailsState::Stash(stash) if ix >= stash.contents.tracked.len() => ix + 1,
            _ => ix,
        }
    }

    /// The list's rows `range`, the only ones `uniform_list` draws: a commit's
    /// files, or a stash's tracked files, then a header row and its untracked files.
    fn file_rows(&self, range: Range<usize>, cx: &Context<Self>) -> Vec<AnyElement> {
        let look = cx.look().clone();
        let (p, m) = (&look.palette, &look.metrics);
        let row = |id: (&'static str, usize), commit, parent, change: &FileChange| {
            let selected = self
                .selected
                .as_ref()
                .is_some_and(|(id, path)| *id == commit && change.path == *path);
            // As wide as the list: `uniform_list` lays a row out at its own width.
            v_flex()
                .w_full()
                .child(file_row(id, change, commit, parent, selected, p, m, cx))
                .into_any_element()
        };
        match &self.state {
            DetailsState::Loaded(commit) => {
                let details = &commit.details;
                let parent = details.parents.first().copied();
                range
                    .filter_map(|ix| {
                        let change = details.changes.get(ix)?;
                        Some(row(("commit-file", ix), details.id, parent, change))
                    })
                    .collect()
            }
            DetailsState::Stash(stash) => {
                let contents = &stash.contents;
                let tracked = contents.tracked.len();
                range
                    .filter_map(|ix| {
                        if let Some(change) = contents.tracked.get(ix) {
                            let id = ("stash-file", ix);
                            return Some(row(id, stash.entry.id, Some(contents.base), change));
                        }
                        let commit = contents.untracked_commit?;
                        if ix == tracked {
                            let title =
                                t!("details.stash_untracked", count = contents.untracked.len());
                            // A row's height, as the list needs; its text lines up with the
                            // header above the list.
                            let header = files_header(title.into_owned(), None, None, p, m, cx)
                                .h(m.file_row)
                                .px(px(6. * m.zoom));
                            return Some(header.into_any_element());
                        }
                        let change = contents.untracked.get(ix - tracked - 1)?;
                        Some(row(("stash-untracked", ix), commit, None, change))
                    })
                    .collect()
            }
            DetailsState::Empty | DetailsState::Several(_) | DetailsState::Failed(_) => Vec::new(),
        }
    }

    /// Several commits are shown (the graph's multi-selection).
    pub fn shows_several(&self) -> bool {
        matches!(self.state, DetailsState::Several(_))
    }

    /// Every file list shown, with the commit and parent its diffs are read against.
    fn file_lists(&self) -> Vec<(ObjectId, Option<ObjectId>, &[FileChange])> {
        match &self.state {
            DetailsState::Loaded(commit) => {
                let details = &commit.details;
                vec![(
                    details.id,
                    details.parents.first().copied(),
                    details.changes.as_slice(),
                )]
            }
            DetailsState::Stash(stash) => {
                let contents = &stash.contents;
                let mut lists = vec![(
                    stash.entry.id,
                    Some(contents.base),
                    contents.tracked.as_slice(),
                )];
                if let Some(commit) = contents.untracked_commit {
                    lists.push((commit, None, contents.untracked.as_slice()));
                }
                lists
            }
            DetailsState::Empty | DetailsState::Several(_) | DetailsState::Failed(_) => Vec::new(),
        }
    }

    /// Under the lists: a bar with the selected file and "Open", then its first hunks.
    /// The previewed file's diff: `gutter` wide numbers, the lines scrolled sideways (or
    /// wrapped, with the code views' setting).
    fn render_preview(
        &self,
        gutter: Pixels,
        wrap: bool,
        p: &Palette,
        m: &Metrics,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let (commit, path) = self.selected.clone()?;
        let (parent, change) = self
            .file_lists()
            .into_iter()
            .filter(|(id, _, _)| *id == commit)
            .find_map(|(_, parent, changes)| {
                let change = changes.iter().find(|change| change.path == path)?;
                Some((parent, change.clone()))
            })?;
        let z = |value: f32| px(value * m.zoom);
        let message = |text: String, color: Hsla| {
            div()
                .p(z(12.))
                .text_size(m.text.small)
                .text_color(color)
                .child(text)
                .into_any_element()
        };
        let body = match &self.preview {
            None => message(t!("details.preview_loading").into_owned(), p.fg3),
            Some(Err(error)) => message(error.clone(), p.del),
            Some(Ok(preview)) if preview.binary => {
                message(t!("details.preview_binary").into_owned(), p.fg3)
            }
            Some(Ok(preview)) if preview.rows.is_empty() => {
                message(t!("details.preview_empty").into_owned(), p.fg3)
            }
            Some(Ok(preview)) => {
                let more = match preview.more {
                    0 => None,
                    count => Some(tn!("details.preview_more", count = count).into_owned()),
                };
                v_flex()
                    .py(z(4.))
                    .font_family(mono_font())
                    .text_size(m.text.xs)
                    .children(
                        preview
                            .rows
                            .iter()
                            .map(|row| preview_row(row, gutter, self.hscroll.offset(), wrap, p, m)),
                    )
                    .children(
                        more.map(|more| div().px(z(12.)).py(z(6.)).text_color(p.fg3).child(more)),
                    )
                    .into_any_element()
            }
        };
        let open = cx.listener(move |_, _, _, cx| {
            cx.emit(DetailsEvent::OpenFile {
                commit,
                parent,
                change: change.clone(),
            })
        });
        Some(
            v_flex()
                .flex_1()
                .min_h(z(160.))
                .border_t_1()
                .border_color(p.line)
                .child(
                    h_flex()
                        .h(z(32.))
                        .flex_shrink_0()
                        .px(z(12.))
                        .gap(z(8.))
                        .bg(p.panel2)
                        .border_b_1()
                        .border_color(p.line)
                        .child(path_label(&path.to_string(), p, m))
                        .child(
                            LinkButton::new(
                                "open-diff",
                                t!("details.open_diff").into_owned(),
                                p.acc,
                            )
                            .size(m.text.small)
                            .on_click(open),
                        ),
                )
                .child(
                    div()
                        .relative()
                        .flex_1()
                        .min_h_0()
                        .child(
                            div()
                                .id("diff-preview")
                                .size_full()
                                .overflow_y_scroll()
                                .child(body),
                        )
                        // Sideways scrolling, and where the lines are scrolled to.
                        .when(!wrap, |this| {
                            this.child(code_scroll::overlay(
                                cx.entity().downgrade(),
                                |view: &mut DetailsView| &mut view.hscroll,
                                &self.hscroll,
                                p,
                                m,
                            ))
                        }),
                )
                .into_any_element(),
        )
    }
}

impl Render for DetailsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);
        // The preview's number column fits its widest number; its lines scroll sideways
        // over what's left after the numbers and the sign.
        let wrap = code_scroll::wrapping(cx);
        let (digits, widest) = match &self.preview {
            Some(Ok(preview)) => (preview.digits, preview.widest),
            _ => (0, 0),
        };
        // With its padding: 8 before the number, 10 after, and half a character of slack
        // (with exactly the digits' width, rounding wrapped the last one).
        let gutter = code_scroll::char_width(window, m.text.xs) * (digits.max(2) as f32 + 0.5)
            + px(18. * m.zoom);
        let content = if wrap {
            Pixels::ZERO
        } else {
            code_scroll::text_width(window, m.text.xs, widest)
        };
        let visible = self.hscroll.viewport() - gutter - px(14. * m.zoom);
        self.hscroll.update(content, visible);
        let focus = self.focus_handle(cx);
        // With a preview, the details above it shrink (and their list scrolls) to leave
        // it room; without one, they take the panel.
        let preview = self.render_preview(gutter, wrap, &p, &m, cx);
        let fill = preview.is_none();
        let (header, content) = match &self.state {
            DetailsState::Empty => {
                return placeholder(t!("details.none_selected").into_owned(), p.fg3, &m);
            }
            DetailsState::Failed(error) => return placeholder(error.clone(), p.del, &m),
            DetailsState::Loaded(commit) => (
                header(
                    t!("details.commit").into_owned(),
                    commit.details.id.to_hex_with_len(8).to_string(),
                    &p,
                    &m,
                    cx,
                ),
                render_commit(commit, &focus, self, &p, &m, cx)
                    .when(fill, |this| this.flex_1())
                    .into_any_element(),
            ),
            DetailsState::Stash(stash) => (
                header(
                    t!("details.stash").into_owned(),
                    stash.entry.revision().to_string(),
                    &p,
                    &m,
                    cx,
                ),
                render_stash(stash, &focus, self, &p, &m, cx)
                    .when(fill, |this| this.flex_1())
                    .into_any_element(),
            ),
            DetailsState::Several(commits) => (
                header(
                    t!("details.several").into_owned(),
                    tn!("details.several_count", count = commits.len()).into_owned(),
                    &p,
                    &m,
                    cx,
                ),
                div()
                    .id("commit-details")
                    .when(fill, |this| this.flex_1())
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(render_several(commits, &p, &m, cx))
                    .into_any_element(),
            ),
        };
        // The preview's share of the panel, as its divider was dragged.
        let preview_share = split::current(Split::Preview, self.split_drag, cx);
        let preview = preview.map(|preview| match preview_share {
            Some(share) => v_flex()
                .h(relative(share))
                .flex_shrink_0()
                .child(preview)
                .into_any_element(),
            None => preview,
        });
        let has_preview = preview.is_some();
        v_flex()
            .debug_selector(|| "details-panel".into())
            .size_full()
            .text_size(m.text.ui)
            .on_drag_move(cx.listener(Self::on_split_drag))
            // A section divider's drag ends wherever the mouse is released.
            .on_mouse_up(MouseButton::Left, cx.listener(Self::release_split))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::release_split))
            .child(header)
            .child(content)
            .when(has_preview, |this| {
                this.child(split::divider(Split::Preview, &p, &m))
            })
            .children(preview)
            .relative()
            .child(measure_height(self.panel_height.clone()))
            .into_any_element()
    }
}

/// An invisible layer over its parent (which must be `relative`) that keeps the parent's
/// drawn height in `height`, drawing once more when it changed.
fn measure_height(height: Rc<Cell<Pixels>>) -> impl IntoElement {
    canvas(
        move |bounds, window, _| {
            if height.replace(bounds.size.height) != bounds.size.height {
                window.request_animation_frame();
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// The details' least height above the files when dragged, and what the files and the
/// preview keep below it (design pixels).
const TOP_MIN: f32 = 60.;
/// The message and facts' most, of the room under the header, until dragged.
const TOP_SHARE: f32 = 0.6;
const BELOW_TOP_MIN: f32 = 100.;

impl DetailsView {
    /// The message and facts' height, as their divider was dragged (design pixels).
    fn top_height(&self, cx: &App) -> Option<f32> {
        split::current(Split::DetailsTop, self.split_drag, cx)
    }

    /// The most the message and facts take when not dragged: 60 % of the room under the
    /// header (less a preview's share, when its divider set one). `None` until the panel
    /// has been measured.
    fn top_cap(&self, cx: &App) -> Option<Pixels> {
        let panel = self.panel_height.get();
        if panel <= Pixels::ZERO {
            return None;
        }
        let preview = match (
            &self.selected,
            split::current(Split::Preview, self.split_drag, cx),
        ) {
            (Some(_), Some(share)) => panel * share,
            _ => Pixels::ZERO,
        };
        Some(((panel - cx.metrics().section - preview) * TOP_SHARE).max(Pixels::ZERO))
    }

    /// A divider moved: under the message and facts, their height; above the preview, its
    /// share of the panel.
    fn on_split_drag(
        &mut self,
        event: &DragMoveEvent<Split>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let m = *cx.metrics();
        let (bounds, at) = (event.bounds, event.event.position.y);
        let value = match *event.drag(cx) {
            Split::DetailsTop => {
                let top = bounds.top() + m.section;
                let most = f32::from(bounds.bottom() - top) / m.zoom - BELOW_TOP_MIN;
                (f32::from(at - top) / m.zoom).min(most).max(TOP_MIN)
            }
            Split::Preview => 1. - split::share(at, bounds.top(), bounds.bottom()),
            _ => return,
        };
        self.split_drag = Some((*event.drag(cx), value));
        cx.notify();
    }

    fn release_split(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if split::release(&mut self.split_drag, cx) {
            cx.notify();
        }
    }
}

/// Several selected commits: each one (a click selects it alone), how to change the
/// selection, and "Cherry-pick N commits".
fn render_several(
    commits: &[RowCommit],
    p: &Palette,
    m: &Metrics,
    cx: &Context<DetailsView>,
) -> impl IntoElement {
    use gpui_kit::component::button::{Button, ButtonVariants as _};

    let z = |value: f32| px(value * m.zoom);
    let picks = commits.to_vec();
    let saves = commits.to_vec();
    let rows = commits.iter().rev().enumerate().map(|(ix, commit)| {
        let id = commit.id;
        h_flex()
            .id(("several", ix))
            .h(m.file_row)
            .px(z(12.))
            .gap(z(10.))
            .cursor_pointer()
            .hover(|style| style.bg(p.hover))
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(DetailsEvent::SelectCommit(id))))
            .child(
                div()
                    .flex_shrink_0()
                    .font_family(mono_font())
                    .text_size(m.text.mono)
                    .text_color(p.fg3)
                    .child(id.to_hex_with_len(8).to_string()),
            )
            .child(truncated("summary", commit.summary.clone()).flex_1())
    });
    v_flex().py(z(6.)).children(rows).child(
        v_flex()
            .m(z(12.))
            .p(z(12.))
            .gap(z(10.))
            .rounded(m.radius_lg)
            .bg(p.panel2)
            .text_size(m.text.small)
            .text_color(p.fg2)
            .child(
                div()
                    .whitespace_normal()
                    .child(t!("details.several_hint").into_owned()),
            )
            .child(
                Button::new("pick-several")
                    .primary()
                    .label(tn!("graph.cherry_pick_multiple", count = picks.len()).into_owned())
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(DetailsEvent::CherryPick(picks.clone()))
                    })),
            )
            .child(
                Button::new("save-several")
                    .label(tn!("files.save_patches", count = saves.len()).into_owned())
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(DetailsEvent::SavePatches(saves.clone()))
                    })),
            ),
    )
}

fn placeholder(text: String, color: Hsla, m: &Metrics) -> AnyElement {
    v_flex()
        .size_full()
        .p(px(16. * m.zoom))
        .items_center()
        .justify_center()
        .text_size(m.text.small)
        .text_color(color)
        .child(text)
        .into_any_element()
}

/// 40 px: "Commit", the short SHA, and × on the right.
fn header(title: String, sha: String, p: &Palette, m: &Metrics, cx: &Context<DetailsView>) -> Div {
    let z = |value: f32| px(value * m.zoom);
    let hover = p.hover;
    h_flex()
        .h(m.section)
        .flex_shrink_0()
        .pl(z(12.))
        .pr(z(8.))
        .gap(z(6.))
        .border_b_1()
        .border_color(p.line)
        .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
        .child(
            div()
                .font_family(mono_font())
                .text_size(m.text.mono)
                .text_color(p.fg2)
                .child(sha),
        )
        .child(div().flex_1())
        .child(
            LinkButton::icon("close-details", AppIcon::Close, p.fg2, z(28.))
                .hover_bg(hover)
                .on_click(cx.listener(|_, _, _, cx| cx.emit(DetailsEvent::Close))),
        )
}

fn render_commit(
    commit: &LoadedCommit,
    focus: &FocusHandle,
    view: &DetailsView,
    p: &Palette,
    m: &Metrics,
    cx: &Context<DetailsView>,
) -> Div {
    let z = |value: f32| px(value * m.zoom);
    let body_open = view.body_open;
    let details = &commit.details;
    let (summary, body) = match details.message.split_once('\n') {
        Some((summary, body)) => (summary, body.trim()),
        None => (details.message.as_str(), ""),
    };
    let long_body = body.lines().count() > BODY_LINES || body.chars().count() > BODY_CHARS;
    let shown_body = if long_body && !body_open {
        collapsed(body)
    } else {
        body.to_owned()
    };
    let toggle = long_body.then(|| {
        let label = if body_open {
            t!("details.body_less")
        } else {
            t!("details.body_more")
        };
        div()
            .debug_selector(|| "details-body-toggle".into())
            .mt(z(4.))
            .child(
                LinkButton::new("body-toggle", label.into_owned(), p.acc)
                    .size(m.text.small)
                    .reveal_in(&view.top_scroll)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.body_open = !this.body_open;
                        cx.notify();
                    })),
            )
    });
    let files = details.changes.len();
    let title = tn!("details.files", count = files).into_owned();

    v_flex()
        .min_h_0()
        .child(
            details_top(
                p,
                m,
                &view.top_scroll,
                view.top_height(cx),
                view.top_cap(cx),
            )
            .child(
                v_flex()
                    .px(z(14.))
                    .pt(z(14.))
                    .pb(z(12.))
                    .child(
                        div()
                            .text_size(z(14.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .line_height(relative(1.35))
                            .child(summary.to_owned()),
                    )
                    .when(!body.is_empty(), |this| {
                        this.child(
                            div()
                                .mt(z(8.))
                                .text_size(m.text.small)
                                .text_color(p.fg2)
                                .line_height(relative(1.5))
                                .child(shown_body),
                        )
                        .children(toggle)
                    })
                    // The committer (another person, or a later time) shares the author's line.
                    .child(
                        person(
                            &details.author,
                            authorship_line(&details.author, &details.committer),
                            p,
                            m,
                        )
                        .mt(z(14.)),
                    )
                    .children(
                        details
                            .signature
                            .as_ref()
                            .map(|signature| signature_badge(signature, p, m).mt(z(10.))),
                    )
                    .child(facts(commit, &view.worktrees, &view.top_scroll, p, m, cx).mt(z(14.))),
            ),
        )
        .child(split::divider(Split::DetailsTop, p, m))
        .child(
            file_list(focus, cx)
                .child(files_header(
                    title,
                    details.line_totals(),
                    Some(details.id),
                    p,
                    m,
                    cx,
                ))
                .child(file_rows_list(files, &view.files_scroll, m, cx)),
        )
}

/// The message and facts above the file list: they scroll on their own, and take at most
/// 60% of the room so a long message (shown whole) leaves the list some. `height`: as the
/// user dragged its divider (design pixels).
fn details_top(
    p: &Palette,
    m: &Metrics,
    scroll: &ScrollHandle,
    height: Option<f32>,
    cap: Option<Pixels>,
) -> Stateful<Div> {
    div()
        .id("details-top")
        // How the headless UI tests find it (a no-op outside them).
        .debug_selector(|| "details-top".into())
        .flex_shrink_0()
        .map(|this| match height {
            Some(height) => this.h(m.dp(height)),
            None => match cap {
                Some(cap) => this.max_h(cap),
                // Before the panel was first measured.
                None => this.max_h(relative(0.6)),
            },
        })
        .overflow_y_scroll()
        .track_scroll(scroll)
        .border_b_1()
        .border_color(p.line)
}

/// The list's `count` rows as a `uniform_list`: only the rows on screen are drawn.
/// As tall as its rows, or as the room left under the details.
fn file_rows_list(
    count: usize,
    scroll: &UniformListScrollHandle,
    m: &Metrics,
    cx: &Context<DetailsView>,
) -> Div {
    let z = |value: f32| px(value * m.zoom);
    let rows = cx.processor(|this, range: Range<usize>, _, cx| this.file_rows(range, cx));
    // The height of all the rows, which the column shrinks: `uniform_list` alone would
    // count as none when the column measures what it holds.
    div()
        .h(m.file_row * count as f32 + z(8.))
        .min_h_0()
        .px(z(6.))
        .pb(z(8.))
        .child(
            uniform_list("details-files", count, rows)
                .track_scroll(scroll)
                .size_full(),
        )
}

/// The commit's id, its parents (a click selects one) and its branches and tags.
fn facts(
    commit: &LoadedCommit,
    worktrees: &HashMap<String, SharedString>,
    scroll: &ScrollHandle,
    p: &Palette,
    m: &Metrics,
    cx: &Context<DetailsView>,
) -> Div {
    let z = |value: f32| px(value * m.zoom);
    let details = &commit.details;
    let lane = p.lanes[commit.lane % p.lanes.len()];
    let acc = p.acc;
    let row = |label: String| {
        h_flex().gap(z(14.)).items_start().child(
            div()
                .w(z(64.))
                .flex_shrink_0()
                .text_color(p.fg3)
                .child(label),
        )
    };
    let parents_label = if details.parents.len() > 1 {
        t!("details.parents")
    } else {
        t!("details.parent")
    };
    v_flex()
        .gap(z(6.))
        .text_size(m.text.mono)
        .child(
            row(t!("details.commit").into_owned()).child(
                truncated("commit-id", details.id.to_string())
                    .font_family(mono_font())
                    .text_color(p.fg2),
            ),
        )
        .when(!details.parents.is_empty(), |this| {
            this.child(
                row(parents_label.into_owned()).child(h_flex().gap(z(8.)).flex_wrap().children(
                    details.parents.iter().enumerate().map(|(ix, &parent)| {
                        div()
                            .debug_selector(move || format!("details-parent-{ix}"))
                            .font_family(mono_font())
                            .child(
                                LinkButton::new(
                                    ("parent", ix),
                                    parent.to_hex_with_len(8).to_string(),
                                    acc,
                                )
                                .reveal_in(scroll)
                                .on_click(cx.listener(
                                    move |_, _, _, cx| cx.emit(DetailsEvent::SelectCommit(parent)),
                                )),
                            )
                    }),
                )),
            )
        })
        .when(!commit.refs.is_empty(), |this| {
            this.child(
                row(refs_label(&commit.refs)).child(
                    h_flex()
                        .debug_selector(|| "details-refs".into())
                        .min_w_0()
                        .gap(z(4.))
                        .flex_wrap()
                        .children(
                            commit.refs.iter().map(|label| {
                                ref_pill(&worktree_pill(label, worktrees), lane, p, m)
                            }),
                        ),
                ),
            )
        })
}

/// The name of a commit's refs row by what it holds: branches, tags, or both ("Refs": the
/// label column is too narrow for "Branches and tags").
fn refs_label(refs: &[RefLabel]) -> String {
    let tags = refs.iter().any(|label| label.kind == RefKind::Tag);
    let branches = refs.iter().any(|label| label.kind != RefKind::Tag);
    match (branches, tags) {
        (true, true) => t!("details.refs"),
        (false, true) => t!("details.tags"),
        _ => t!("details.branches"),
    }
    .into_owned()
}

/// 36 px: how many files, the lines added and removed with a ratio bar, and "Browse files"
/// for a commit.
fn files_header(
    title: String,
    totals: Option<LineCount>,
    browse: Option<ObjectId>,
    p: &Palette,
    m: &Metrics,
    cx: &Context<DetailsView>,
) -> Div {
    let z = |value: f32| px(value * m.zoom);
    let acc = p.acc;
    h_flex()
        .h(z(36.))
        .flex_shrink_0()
        .px(z(12.))
        .gap(z(8.))
        .text_size(m.text.mono)
        .text_color(p.fg2)
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.fg)
                .child(title),
        )
        .when_some(totals, |this, lines| {
            this.child(count(format!("+{}", lines.added), p.add, m.text.mono))
                .child(count(format!("−{}", lines.removed), p.del, m.text.mono))
        })
        .child(div().flex_1())
        .when_some(
            totals.filter(|lines| lines.added + lines.removed > 0),
            |this, lines| this.child(ratio_bar(lines, p, m)),
        )
        .when_some(browse, |this, commit| {
            this.child(
                LinkButton::new("browse-files", t!("tree.browse").into_owned(), acc).on_click(
                    cx.listener(move |_, _, _, cx| cx.emit(DetailsEvent::BrowseFiles(commit))),
                ),
            )
        })
}

/// 80×6: the share of added lines in the add color, the rest in the delete color.
fn ratio_bar(lines: LineCount, p: &Palette, m: &Metrics) -> Div {
    let z = |value: f32| px(value * m.zoom);
    let total = (lines.added + lines.removed).max(1) as f32;
    div()
        .w(z(80.))
        .h(z(6.))
        .flex_shrink_0()
        .rounded(z(3.))
        .overflow_hidden()
        .bg(p.del)
        .child(
            div()
                .h_full()
                .w(z(80. * lines.added as f32 / total))
                .bg(p.add),
        )
}

/// "+12" or "−3" in mono.
fn count(text: String, color: Hsla, size: Pixels) -> Div {
    div()
        .flex_shrink_0()
        .font_family(mono_font())
        .text_size(size)
        .text_color(color)
        .child(text)
}

/// A stash: its message, when and on which commit it was made, then its changed files
/// and its untracked files. A tracked file's diff is against that commit; an untracked
/// one shows as a new file (its commit has no parent).
fn render_stash(
    stash: &StashDetails,
    focus: &FocusHandle,
    view: &DetailsView,
    p: &Palette,
    m: &Metrics,
    cx: &Context<DetailsView>,
) -> Div {
    let z = |value: f32| px(value * m.zoom);
    let contents = &stash.contents;
    // The untracked files follow a header row of their own.
    let rows = contents.tracked.len()
        + contents
            .untracked_commit
            .map_or(0, |_| 1 + contents.untracked.len());
    v_flex()
        .min_h_0()
        .child(
            details_top(
                p,
                m,
                &view.top_scroll,
                view.top_height(cx),
                view.top_cap(cx),
            )
            .child(
                v_flex()
                    .px(z(14.))
                    .pt(z(14.))
                    .pb(z(12.))
                    .child(
                        div()
                            .text_size(z(14.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .line_height(relative(1.35))
                            .child(stash.label.clone()),
                    )
                    .child(
                        div()
                            .mt(z(6.))
                            .text_size(m.text.xs)
                            .text_color(p.fg3)
                            .child(
                                t!(
                                    "details.stashed",
                                    name = stash.entry.revision(),
                                    time = format_time(stash.entry.time)
                                )
                                .into_owned(),
                            ),
                    )
                    .child(
                        h_flex()
                            .mt(z(12.))
                            .gap(z(14.))
                            .text_size(m.text.mono)
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_color(p.fg3)
                                    .child(t!("details.stash_base").into_owned()),
                            )
                            .child(
                                truncated("stash-id", contents.base.to_string())
                                    .font_family(mono_font())
                                    .text_color(p.fg2),
                            ),
                    )
                    .child({
                        let action = |id: &'static str, label: String, event: DetailsEvent| {
                            Button::new(id)
                                .small()
                                .label(label)
                                .on_click(cx.listener(move |_, _, _, cx| cx.emit(event.clone())))
                        };
                        let entry = &stash.entry;
                        let apply = DetailsEvent::StashApply {
                            entry: entry.clone(),
                            pop: false,
                        };
                        let pop = DetailsEvent::StashApply {
                            entry: entry.clone(),
                            pop: true,
                        };
                        h_flex()
                            .mt(z(12.))
                            .gap(z(8.))
                            .child(
                                action("stash-apply", t!("stash.apply").into_owned(), apply)
                                    .primary(),
                            )
                            .child(action("stash-pop", t!("stash.pop").into_owned(), pop))
                            .child(
                                action(
                                    "stash-drop",
                                    t!("stash.drop").into_owned(),
                                    DetailsEvent::StashDrop(entry.clone()),
                                )
                                .ghost(),
                            )
                    }),
            ),
        )
        .child(split::divider(Split::DetailsTop, p, m))
        .child(
            file_list(focus, cx)
                .child(files_header(
                    t!("details.stash_changes", count = contents.tracked.len()).into_owned(),
                    None,
                    None,
                    p,
                    m,
                    cx,
                ))
                .child(file_rows_list(rows, &view.files_scroll, m, cx)),
        )
}

/// A 28 px avatar in a lane color, the name, and `line` under it ("authored <date>").
fn person(signature: &Signature, line: String, p: &Palette, m: &Metrics) -> Div {
    let z = |value: f32| px(value * m.zoom);
    h_flex()
        .gap(z(10.))
        .child(avatar(&signature.name, &signature.email, z(28.), p))
        .child(
            v_flex()
                .min_w_0()
                .child(truncated("signer", signature.name.clone()).font_weight(FontWeight::MEDIUM))
                .child(
                    div()
                        .text_size(m.text.xs)
                        .text_color(p.fg3)
                        .whitespace_normal()
                        .child(line),
                ),
        )
}

/// The line under the author: when the commit was authored, then when it was committed
/// (and by whom, when that's someone else), folded into one line as designed.
/// Each value keeps its words together (no-break spaces): a wrap falls between the line's
/// words, never inside "2024-03-01 06:00", "3 hours" or a name, and the last word never
/// goes down alone ("later").
fn authorship_line(author: &Signature, committer: &Signature) -> String {
    let glue = |text: &str| text.replace(' ', "\u{a0}");
    let date = glue(&format_time(author.time));
    let same = author.name == committer.name && author.email == committer.email;
    // A plain commit stamps both at once; under a minute counts as the same time.
    let gap = committer.time - author.time;
    let later = (gap >= 60).then(|| glue(&later_text(gap)));
    let line = match (same, later) {
        (true, None) => t!("details.authored_on", date = date),
        (true, Some(later)) => t!(
            "details.authored_committed_later",
            date = date,
            later = later
        ),
        (false, None) => t!(
            "details.authored_committed_by",
            date = date,
            name = glue(&committer.name)
        ),
        (false, Some(later)) => t!(
            "details.authored_committed_by_later",
            date = date,
            name = glue(&committer.name),
            later = later
        ),
    };
    match line.rfind(' ') {
        Some(ix) => format!("{}\u{a0}{}", &line[..ix], &line[ix + 1..]),
        None => line.into_owned(),
    }
}

/// How much later a commit came than it was written: minutes up to an hour, hours up to
/// two days, then days.
fn later_text(seconds: i64) -> String {
    let minutes = usize::try_from(seconds / 60).unwrap_or(0);
    if minutes < 60 {
        tn!("details.later_minutes", count = minutes)
    } else if minutes < 48 * 60 {
        tn!("details.later_hours", count = minutes / 60)
    } else {
        tn!("details.later_days", count = minutes / (24 * 60))
    }
    .into_owned()
}

/// How the commit's signature checks out, with the signing key under it.
fn signature_badge(signature: &CommitSignature, p: &Palette, m: &Metrics) -> Div {
    let z = |value: f32| px(value * m.zoom);
    let signer = if signature.signer.is_empty() {
        signature.key.clone()
    } else {
        signature.signer.clone()
    };
    let (text, color) = match signature.status {
        SignatureStatus::Good => (t!("signature.good", signer = signer), p.add),
        SignatureStatus::Untrusted => (t!("signature.untrusted"), p.modified),
        SignatureStatus::Bad => (t!("signature.bad"), p.del),
        SignatureStatus::ExpiredSignature => (t!("signature.expired"), p.modified),
        SignatureStatus::ExpiredKey => (t!("signature.expired_key"), p.modified),
        SignatureStatus::RevokedKey => (t!("signature.revoked"), p.del),
        SignatureStatus::CannotCheck => (t!("signature.cannot_check"), p.fg3),
    };
    v_flex()
        .gap(z(2.))
        .text_size(m.text.small)
        .child(
            h_flex()
                .gap(z(8.))
                .child(div().size(z(8.)).flex_shrink_0().rounded_full().bg(color))
                .child(div().text_color(color).child(text.into_owned())),
        )
        .when(!signature.key.is_empty(), |this| {
            this.child(
                truncated("key", signature.key.clone())
                    .pl(z(16.))
                    .font_family(mono_font())
                    .text_size(m.text.mono_small)
                    .text_color(p.fg3),
            )
        })
}

/// Status letter and color for a change (shared with the staging panel).
pub(crate) fn change_style(kind: ChangeKind, p: &Palette) -> (&'static str, Hsla) {
    match kind {
        ChangeKind::Added => ("A", p.add),
        ChangeKind::Modified => ("M", p.modified),
        ChangeKind::Deleted => ("D", p.del),
        ChangeKind::Renamed => ("R", p.acc),
        ChangeKind::Copied => ("C", p.acc),
        ChangeKind::TypeChanged => ("T", p.modified),
        ChangeKind::Other => ("?", p.fg3),
    }
}

/// The files' container: it has the list keys (the arrows, Home and End move the preview,
/// Enter opens the diff, Shift+F10 asks for the row menu) and the focus they need.
/// It takes the room the details leave, and shrinks first.
fn file_list(focus: &FocusHandle, cx: &Context<DetailsView>) -> Div {
    v_flex()
        .flex_grow(1.)
        .min_h_0()
        .key_context(list_keys::DETAILS)
        .track_focus(focus)
        .on_action(cx.listener(|this, _: &NextRow, _, cx| {
            this.select_row(|current, _| current.map_or(0, |ix| ix + 1), cx)
        }))
        .on_action(cx.listener(|this, _: &PreviousRow, _, cx| {
            this.select_row(
                |current, _| current.map_or(0, |ix| ix.saturating_sub(1)),
                cx,
            )
        }))
        .on_action(cx.listener(|this, _: &FirstRow, _, cx| this.select_row(|_, _| 0, cx)))
        .on_action(
            cx.listener(|this, _: &LastRow, _, cx| this.select_row(|_, count| count - 1, cx)),
        )
        .on_action(cx.listener(|this, _: &OpenRow, _, cx| {
            if let Some((commit, parent, change)) = this.selected_file() {
                cx.emit(DetailsEvent::OpenFile {
                    commit,
                    parent,
                    change,
                });
            }
        }))
        .on_action(cx.listener(|this, _: &RowMenu, _, cx| {
            if let Some((commit, parent, change)) = this.selected_file() {
                cx.emit(DetailsEvent::FileMenu {
                    commit,
                    parent,
                    change,
                });
            }
        }))
}

/// A file row's menu for `change` as of `commit` (against `parent`), in order: each item's
/// label and the event that runs it. Blame, the editor and the file manager are left out
/// when the commit deleted the file. The row's context menu and the workspace's answer to
/// [`DetailsEvent::FileMenu`] both use it.
pub(crate) fn file_menu_items(
    commit: ObjectId,
    parent: Option<ObjectId>,
    change: &FileChange,
) -> Vec<(String, DetailsEvent)> {
    // As git names it, whatever its encoding.
    let path = change.path.clone();
    let deleted = change.kind == ChangeKind::Deleted;
    let mut items = Vec::new();
    if !deleted {
        let blame = DetailsEvent::Blame {
            commit,
            path: path.clone(),
        };
        items.push((t!("blame.open").into_owned(), blame));
    }
    let history = DetailsEvent::History {
        commit,
        path: path.clone(),
    };
    items.push((t!("history.open").into_owned(), history));
    let diff_tool = DetailsEvent::DiffTool {
        commit,
        parent,
        change: change.clone(),
    };
    items.push((t!("external.diff_tool").into_owned(), diff_tool));
    if !deleted {
        let editor = DetailsEvent::WorkingFile {
            path: path.clone(),
            editor: true,
        };
        items.push((t!("external.open_editor").into_owned(), editor));
        let folder = DetailsEvent::WorkingFile {
            path,
            editor: false,
        };
        items.push((t!("external.show_in_folder").into_owned(), folder));
    }
    items
}

/// A file changed by `commit` (against `parent`): 28 px, its badge, path and line counts.
/// A click previews its diff (and focuses the list), a double click opens the whole diff;
/// its menu is [`file_menu_items`]. `selected`: it's the previewed file.
#[allow(clippy::too_many_arguments)]
fn file_row(
    id: (&'static str, usize),
    change: &FileChange,
    commit: ObjectId,
    parent: Option<ObjectId>,
    selected: bool,
    p: &Palette,
    m: &Metrics,
    cx: &Context<DetailsView>,
) -> impl IntoElement {
    let z = |value: f32| px(value * m.zoom);
    let (letter, color) = change_style(change.kind, p);
    let path = match &change.old_path {
        Some(old) => format!("{old} → {}", change.path),
        None => change.path.to_string(),
    };
    let (hover, sel) = (p.hover, p.sel);
    let (menu_view, menu_change) = (cx.entity().downgrade(), change.clone());
    let lines = change.lines;
    let change = change.clone();
    let row = h_flex()
        .id(id)
        .h(m.file_row)
        .px(z(6.))
        .gap(z(8.))
        .rounded(m.radius)
        .cursor_pointer()
        .when(selected, |row| row.bg(sel))
        .when(!selected, |row| row.hover(move |style| style.bg(hover)))
        .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
            this.focus_handle(cx).focus(window, cx);
            if event.click_count() >= 2 {
                cx.emit(DetailsEvent::OpenFile {
                    commit,
                    parent,
                    change: change.clone(),
                })
            } else {
                this.select(commit, parent, change.clone(), cx)
            }
        }))
        .child(status_badge(letter, color, m))
        .child(path_label(&path, p, m))
        .when_some(lines, |row, lines| {
            row.child(count(format!("+{}", lines.added), p.add, m.text.mono_small))
                .child(count(
                    format!("−{}", lines.removed),
                    p.del,
                    m.text.mono_small,
                ))
        });
    row.context_menu(move |menu, _, _| {
        let items = file_menu_items(commit, parent, &menu_change);
        items.into_iter().fold(menu, |menu, (label, event)| {
            let view = menu_view.clone();
            menu.item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
                view.update(cx, |_, cx| cx.emit(event.clone())).ok();
            }))
        })
    })
}

/// A diff as the preview draws it: its rows (hunk headers and lines, at most a limit),
/// how many lines were left out, and whether it's binary. Built once when it arrives.
#[derive(Debug, Clone, PartialEq)]
struct Preview {
    rows: Vec<PreviewRow>,
    more: usize,
    binary: bool,
    /// The digits of the widest line number, for the number column.
    digits: usize,
    /// The characters of the widest row, for the sideways scroll.
    widest: usize,
}

#[derive(Debug, Clone, PartialEq)]
struct PreviewRow {
    kind: PreviewKind,
    /// The line's number (the old side's for a removed line); empty for a hunk header.
    number: SharedString,
    text: SharedString,
    /// Syntax colors of `text` (none for a hunk header, or a file without a syntax).
    spans: Spans,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreviewKind {
    Hunk,
    Context,
    Added,
    Removed,
}

impl Preview {
    /// `syntax` is `[hunk][line]`, as `diff_decor::decorate` makes it (may be empty).
    /// Lines are expanded with tab stops `tab` columns apart, as `syntax`'s were.
    fn new(diff: &FileDiff, syntax: &[Vec<Spans>], limit: usize, tab: usize) -> Self {
        let mut rows = Vec::new();
        let mut more = 0;
        let (mut digits, mut widest) = (0, 0);
        for (h, hunk) in diff.hunks.iter().enumerate() {
            if rows.len() >= limit {
                more += hunk.lines.len();
                continue;
            }
            let header = hunk.header();
            widest = widest.max(columns(&header));
            rows.push(PreviewRow {
                kind: PreviewKind::Hunk,
                number: SharedString::default(),
                text: header.into(),
                spans: Spans::new(),
            });
            for (l, line) in hunk.lines.iter().enumerate() {
                if rows.len() >= limit {
                    more += 1;
                    continue;
                }
                let (kind, number) = match line.kind {
                    LineKind::Context => (PreviewKind::Context, line.new_line),
                    LineKind::Added => (PreviewKind::Added, line.new_line),
                    LineKind::Removed => (PreviewKind::Removed, line.old_line),
                };
                let number = number.map(|n| n.to_string()).unwrap_or_default();
                let text = line.display_text(tab);
                digits = digits.max(number.len());
                widest = widest.max(columns(&text));
                rows.push(PreviewRow {
                    kind,
                    number: number.into(),
                    text: text.into(),
                    spans: syntax
                        .get(h)
                        .and_then(|lines| lines.get(l))
                        .cloned()
                        .unwrap_or_default(),
                });
            }
        }
        Self {
            rows,
            more,
            binary: diff.binary,
            digits,
            widest,
        }
    }
}

/// The start of a long commit body: its first lines, cut at a character limit ("…" where a
/// line is cut; "Show more" under it says the rest).
fn collapsed(body: &str) -> String {
    let mut text = body.lines().take(BODY_LINES).collect::<Vec<_>>().join("\n");
    if let Some((cut, _)) = text.char_indices().nth(BODY_CHARS) {
        text.truncate(cut);
        text.push('…');
    }
    text
}

/// A hunk header on panel2, or a code line: its number (in a `gutter` wide column), its
/// sign and its text in syntax colors, tinted. The text is scrolled sideways by `offset`, or
/// wrapped.
fn preview_row(
    row: &PreviewRow,
    gutter: Pixels,
    offset: Pixels,
    wrap: bool,
    p: &Palette,
    m: &Metrics,
) -> Div {
    let z = |value: f32| px(value * m.zoom);
    if row.kind == PreviewKind::Hunk {
        return code_scroll::code_row(wrap, z(28.))
            .items_center()
            .px(z(12.))
            .bg(p.panel2)
            .text_color(p.fg2)
            .child(code_scroll::code_text(row.text.clone(), wrap, offset));
    }
    let (bg, sign, sign_color) = match row.kind {
        PreviewKind::Added => (p.add_bg, "+", p.add),
        PreviewKind::Removed => (p.del_bg, "-", p.del),
        PreviewKind::Context | PreviewKind::Hunk => (transparent_black(), "", p.fg3),
    };
    let highlights = row.spans.iter().map(|(range, color)| {
        let style = HighlightStyle {
            color: Some(rgb(color).into()),
            ..HighlightStyle::default()
        };
        (range.clone(), style)
    });
    let text = StyledText::new(row.text.clone()).with_highlights(highlights);
    code_scroll::code_row(wrap, m.code_line)
        .w_full()
        .bg(bg)
        .child(
            div()
                .w(gutter)
                .flex_shrink_0()
                .pl(z(8.))
                .pr(z(10.))
                .text_right()
                .text_color(p.fg3)
                .child(row.number.clone()),
        )
        .child(
            div()
                .w(z(14.))
                .flex_shrink_0()
                .text_color(sign_color)
                .child(sign),
        )
        .child(code_scroll::code_text(text, wrap, offset).text_color(p.fg))
}

#[cfg(test)]
mod tests {
    // Not `super::*`: GPUI's `test` macro would shadow `#[test]`.
    use super::{
        BODY_CHARS, BODY_LINES, Preview, PreviewKind, authorship_line, collapsed, later_text,
        refs_label,
    };
    use gitweft_core::repo::{RefKind, RefLabel};
    use gitweft_git::read::Signature;

    #[test]
    fn the_refs_row_is_named_after_what_it_holds() {
        let label = |kind| RefLabel {
            name: "x".into(),
            kind,
        };
        let name = |refs: &[RefLabel]| refs_label(refs);
        assert_eq!(
            name(&[label(RefKind::Head), label(RefKind::Remote)]),
            "Branches"
        );
        assert_eq!(name(&[label(RefKind::Tag), label(RefKind::Tag)]), "Tags");
        assert_eq!(name(&[label(RefKind::Local), label(RefKind::Tag)]), "Refs");
    }

    #[test]
    fn author_and_committer_share_one_line() {
        let person = |name: &str, time: i64| Signature {
            name: name.into(),
            email: format!("{name}@example.com"),
            time,
        };
        let at = 1_700_000_000;
        // The values' own spaces are no-break spaces.
        let date = crate::util::format_time(at).replace(' ', "\u{a0}");
        let alex = person("Alex", at);
        assert_eq!(
            authorship_line(&alex, &person("Alex", at + 30)),
            format!("authored\u{a0}{date}")
        );
        assert_eq!(
            authorship_line(&alex, &person("Alex", at + 180)),
            format!("authored {date} · committed 3\u{a0}min\u{a0}later")
        );
        assert_eq!(
            authorship_line(&alex, &person("Sam", at)),
            format!("authored {date} · committed by\u{a0}Sam")
        );
        assert_eq!(
            authorship_line(&alex, &person("Sam", at + 3 * 3600)),
            format!("authored {date} · committed by Sam 3\u{a0}hours\u{a0}later")
        );
        assert_eq!(later_text(59 * 60), "59 min");
        assert_eq!(later_text(3600), "1 hour");
        assert_eq!(later_text(47 * 3600), "47 hours");
        assert_eq!(later_text(3 * 86_400), "3 days");
    }
    use gitweft_git::read::{DiffLine, FileDiff, Hunk, LineKind};
    use gitweft_syntax::Rgb;

    fn line(kind: LineKind, old: Option<u32>, new: Option<u32>, text: &str) -> DiffLine {
        DiffLine {
            kind,
            old_line: old,
            new_line: new,
            text: text.into(),
            no_newline: false,
        }
    }

    #[test]
    fn preview_rows_number_their_lines_and_stop_at_the_limit() {
        let hunk = |start: u32| Hunk {
            old_start: start,
            old_len: 2,
            new_start: start,
            new_len: 2,
            section: String::new(),
            lines: vec![
                line(LineKind::Context, Some(start), Some(start), "same"),
                line(LineKind::Removed, Some(start + 1), None, "old"),
                line(LineKind::Added, None, Some(start + 1), "new\tx"),
            ],
        };
        let diff = FileDiff {
            hunks: vec![hunk(1), hunk(40)],
            ..FileDiff::default()
        };

        let all = Preview::new(&diff, &[], 100, 4);
        let kinds: Vec<_> = all.rows.iter().map(|row| row.kind).collect();
        let (h, c, r, a) = (
            PreviewKind::Hunk,
            PreviewKind::Context,
            PreviewKind::Removed,
            PreviewKind::Added,
        );
        assert_eq!(kinds, [h, c, r, a, h, c, r, a]);
        // A removed line has the old side's number; tabs are expanded to the next tab stop
        // ("new" is three columns, so one space).
        assert_eq!(all.rows[2].number.as_ref(), "2");
        assert_eq!(all.rows[3].text.as_ref(), "new x");
        assert_eq!(all.rows[0].number.as_ref(), "");
        assert_eq!(all.more, 0);

        // The number column fits "41"; the widest row is a hunk header.
        assert_eq!(all.digits, 2);
        let widest = all.rows.iter().map(|row| row.text.chars().count()).max();
        assert_eq!(Some(all.widest), widest);

        // Cut after the second hunk's header: its three lines are left out.
        let cut = Preview::new(&diff, &[], 5, 4);
        assert_eq!(cut.rows.len(), 5);
        assert_eq!(cut.rows[4].kind, PreviewKind::Hunk);
        assert_eq!(cut.more, 3);

        // Syntax colors land on their lines (the first hunk's second line is row 2).
        let red = vec![(0..3, Rgb(200, 0, 0))];
        let syntax = vec![vec![Vec::new(), red.clone(), Vec::new()]];
        let colored = Preview::new(&diff, &syntax, 100, 4);
        assert_eq!(colored.rows[2].spans, red);
        assert!(colored.rows[5].spans.is_empty());
    }

    #[test]
    fn long_bodies_are_cut_to_their_first_lines() {
        let body = (1..=20)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let cut = collapsed(&body);
        assert_eq!(cut.lines().count(), BODY_LINES);
        assert!(
            cut.starts_with("line 1\n") && cut.ends_with("line 8"),
            "{cut}"
        );
        // One very long line is cut at the character limit, on a character boundary.
        let wide = "é".repeat(BODY_CHARS * 2);
        assert_eq!(collapsed(&wide).chars().count(), BODY_CHARS + 1);
    }
}
