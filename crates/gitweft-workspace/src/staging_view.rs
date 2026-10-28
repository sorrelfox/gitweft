//! Right panel while "Working changes" is selected: unstaged and staged files with
//! stage / unstage / discard actions, and the commit box. Actions are emitted as events;
//! the workspace runs them (in the write lane) and refreshes the status.

use std::collections::HashSet;
use std::rc::Rc;

use gitweft_git::gix::bstr::BString;
use gitweft_git::ops::{CommitContext, CommitOptions, ConfigOrigin, Identity, with_co_authors};
use gitweft_git::read::{ChangeKind, EntryKind, Person, Status, StatusEntry};
use gitweft_ui::truncated::truncated;
use gitweft_ui::widgets::{path_label, section_count, status_badge};
use gitweft_ui::{LookExt, Metrics, Palette};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{
    IndentInline, Input, InputEvent, InputState, OutdentInline, Textarea, TextareaState,
};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::{Disableable, Sizable, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::details_view::change_style;
use crate::list_keys::{self, Step};
use crate::split::{self, Split};
use gitweft_ui::current::mono_font;

/// Git's conventional limits for the summary line (a guide, not enforced).
const SUMMARY_SOFT_LIMIT: usize = 50;
const SUMMARY_HARD_LIMIT: usize = 72;
/// Co-author choices offered (the most frequent recent authors).
const MAX_CO_AUTHOR_CHOICES: usize = 30;

pub enum StagingEvent {
    Stage(Vec<StatusEntry>),
    Unstage(Vec<StatusEntry>),
    /// Already confirmed by the user.
    Discard(Vec<StatusEntry>),
    /// Show this file's diff (`staged` picks the index side).
    OpenDiff {
        entry: StatusEntry,
        staged: bool,
    },
    Commit {
        message: String,
        options: CommitOptions,
    },
    /// Show who last changed each line of the working-tree file.
    Blame(gitweft_git::gix::bstr::BString),
    /// List the commits that changed the file, up to HEAD.
    History(gitweft_git::gix::bstr::BString),
    /// Stash only these files (after asking for a message).
    Stash(Vec<gitweft_git::gix::bstr::BString>),
    /// Shift+F10 on a file: its row menu's actions in the palette (`selection`: the
    /// multi-file selection it's part of, else empty).
    RowMenu {
        entry: StatusEntry,
        staged: bool,
        selection: Vec<StatusEntry>,
    },
    /// Add an ignore rule for an untracked path.
    Ignore {
        path: gitweft_git::gix::bstr::BString,
        choice: IgnoreChoice,
    },
    /// The "Committing as …" line was clicked: set the name and email for commits.
    EditIdentity,
    /// Escape in the file lists with no multi-file selection to drop: close the
    /// working-changes diff they opened (it doesn't take the keys, so its own Escape can't).
    CloseDiff,
    /// Open the file in another program (`workspace/external.rs`).
    External {
        entry: StatusEntry,
        tool: FileTool,
    },
}

/// Another program for a changed file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileTool {
    /// Git's diff tool: HEAD against the working tree.
    DiffTool,
    /// Git's merge tool, for a conflicted file.
    MergeTool,
    Editor,
    /// The file manager, with the file selected.
    Folder,
    /// Lock the file on the LFS server (`git lfs lock`).
    LfsLock,
    /// Release its LFS lock (`git lfs unlock`).
    LfsUnlock,
}

/// Which ignore rule an untracked file's menu adds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoreChoice {
    /// The file (or folder) itself, in `.gitignore`.
    Path,
    /// Every file with its extension, in `.gitignore`.
    Extension,
    /// The folder it's in, in `.gitignore`.
    Folder,
    /// The file itself, in `.git/info/exclude` (only this clone).
    OnlyHere,
}

/// What a changed file's menu offers (besides the row's links).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileAction {
    Blame,
    History,
    Stash,
    Ignore(IgnoreChoice),
    External(FileTool),
}

impl FileAction {
    /// The event that does it, for the file of `entry`.
    pub(crate) fn event(self, entry: &StatusEntry) -> StagingEvent {
        let path = &entry.path;
        match self {
            FileAction::Blame => StagingEvent::Blame(path.clone()),
            FileAction::History => StagingEvent::History(path.clone()),
            FileAction::Stash => StagingEvent::Stash(vec![path.clone()]),
            FileAction::Ignore(choice) => StagingEvent::Ignore {
                path: path.clone(),
                choice,
            },
            FileAction::External(tool) => StagingEvent::External {
                entry: entry.clone(),
                tool,
            },
        }
    }
}

/// A changed file's menu, shared with the command palette: blame (the file in git and on
/// disk), history (in HEAD), stash it alone (not while conflicted; the conflict tool
/// handles those), an untracked file's ignore rules, then other programs: git's diff tool
/// (merge tool while conflicted), the editor and the file manager (while the file is on
/// disk), and with `lfs` (the repository uses Git LFS) Lock / Unlock on the LFS server for a
/// tracked file (git-lfs locks any path). `true`: a separator comes first.
pub(crate) fn file_actions(entry: &StatusEntry, lfs: bool) -> Vec<(FileAction, String, bool)> {
    let tracked = entry.kind == EntryKind::Tracked;
    let deleted = [entry.index, entry.worktree].contains(&Some(ChangeKind::Deleted));
    let mut actions = Vec::new();
    if tracked && !deleted {
        actions.push((FileAction::Blame, t!("blame.open").into_owned(), false));
    }
    if tracked && entry.index != Some(ChangeKind::Added) {
        actions.push((FileAction::History, t!("history.open").into_owned(), false));
    }
    if entry.kind != EntryKind::Conflicted {
        let separated = !actions.is_empty();
        actions.push((FileAction::Stash, t!("stash.file").into_owned(), separated));
    }
    if entry.kind == EntryKind::Untracked {
        for (ix, (choice, label)) in ignore_items(&entry.path).into_iter().enumerate() {
            actions.push((FileAction::Ignore(choice), label, ix == 0));
        }
    }
    let first = actions.len();
    let external = |tool, key: &str| (FileAction::External(tool), t!(key).into_owned(), false);
    if entry.kind == EntryKind::Conflicted {
        actions.push(external(FileTool::MergeTool, "external.merge_tool"));
    } else if tracked {
        actions.push(external(FileTool::DiffTool, "external.diff_tool"));
    }
    if !deleted {
        actions.push(external(FileTool::Editor, "external.open_editor"));
        actions.push(external(FileTool::Folder, "external.show_in_folder"));
    }
    if first > 0
        && let Some(item) = actions.get_mut(first)
    {
        item.2 = true;
    }
    if lfs && tracked && !deleted {
        actions.push((
            FileAction::External(FileTool::LfsLock),
            t!("lfs.lock_row").into_owned(),
            true,
        ));
        actions.push((
            FileAction::External(FileTool::LfsUnlock),
            t!("lfs.unlock_row").into_owned(),
            false,
        ));
    }
    actions
}

/// An untracked path's ignore items: the path, its extension and its folder (when it has
/// them), and the path for this clone only.
fn ignore_items(path: &[u8]) -> Vec<(IgnoreChoice, String)> {
    let is_dir = path.ends_with(b"/");
    let trimmed = path.strip_suffix(b"/").unwrap_or(path);
    let slash = trimmed.iter().rposition(|b| *b == b'/');
    let name = &trimmed[slash.map_or(0, |i| i + 1)..];
    let mut items = vec![(
        IgnoreChoice::Path,
        if is_dir {
            t!("files.ignore_folder_self")
        } else {
            t!("files.ignore_file")
        }
        .into_owned(),
    )];
    if !is_dir && let Some(dot) = name.iter().rposition(|b| *b == b'.').filter(|&i| i > 0) {
        let pattern = format!("*{}", String::from_utf8_lossy(&name[dot..]));
        items.push((
            IgnoreChoice::Extension,
            t!("files.ignore_extension", pattern = pattern).into_owned(),
        ));
    }
    if let Some(slash) = slash {
        let folder = format!("{}/", String::from_utf8_lossy(&trimmed[..slash]));
        items.push((
            IgnoreChoice::Folder,
            t!("files.ignore_parent", folder = folder).into_owned(),
        ));
    }
    items.push((
        IgnoreChoice::OnlyHere,
        t!("files.ignore_local").into_owned(),
    ));
    items
}

/// Rows picked in one of the two lists (by position in it), and the row a Shift+click
/// range starts from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Pick {
    staged: bool,
    /// Sorted, without duplicates.
    rows: Vec<usize>,
    anchor: Option<usize>,
}

/// The pick after a click on row `clicked` of the `staged` list. A plain click picks just
/// that row; Ctrl toggles it; Shift picks the range from the anchor to it (Ctrl+Shift adds
/// the range). A Ctrl or Shift click in the other list, or Shift without an anchor, starts
/// over like a plain click. The rows can come back empty (Ctrl took the last one off).
fn select(current: Option<&Pick>, staged: bool, clicked: usize, ctrl: bool, shift: bool) -> Pick {
    match current.filter(|pick| pick.staged == staged) {
        Some(Pick {
            rows,
            anchor: Some(anchor),
            ..
        }) if shift => {
            let anchor = *anchor;
            let mut picked = if ctrl { rows.clone() } else { Vec::new() };
            picked.extend(anchor.min(clicked)..=anchor.max(clicked));
            picked.sort_unstable();
            picked.dedup();
            Pick {
                staged,
                rows: picked,
                anchor: Some(anchor),
            }
        }
        Some(Pick { rows, .. }) if ctrl && !shift => {
            let mut picked = rows.clone();
            match picked.binary_search(&clicked) {
                Ok(at) => {
                    picked.remove(at);
                }
                Err(at) => picked.insert(at, clicked),
            }
            Pick {
                staged,
                rows: picked,
                anchor: Some(clicked),
            }
        }
        _ => Pick {
            staged,
            rows: vec![clicked],
            anchor: Some(clicked),
        },
    }
}

/// The selected files of one list, by path (so they outlast a status reload), and the file
/// a Shift+click range starts from.
struct Selection {
    staged: bool,
    paths: HashSet<BString>,
    anchor: Option<BString>,
}

/// The files of one list, in the order shown.
fn list_entries(status: &Status, staged: bool) -> Vec<&StatusEntry> {
    if staged {
        status.staged().collect()
    } else {
        status.unstaged().collect()
    }
}

/// "Stage 3 files" / "Unstage 3 files" (the header link and menu item for a selection).
fn stage_count_label(staged: bool, count: usize) -> String {
    if staged {
        tn!("staging.unstage_count", count = count)
    } else {
        tn!("staging.stage_count", count = count)
    }
    .into_owned()
}

/// The selection's files that Discard and Stash take (not the conflicted ones).
fn not_conflicted(entries: &[StatusEntry]) -> Vec<StatusEntry> {
    entries
        .iter()
        .filter(|e| e.kind != EntryKind::Conflicted)
        .cloned()
        .collect()
}

/// What a file row's menu does first, to the file or to the selection it's part of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryAction {
    /// Stage (mark resolved, for a conflicted file).
    Stage,
    Unstage,
    /// Throw the changes away (it asks first).
    Discard,
    /// Stash just these files (it asks for a message).
    Stash,
}

/// A file row's first menu items, shared with the command palette (Shift+F10), each with the
/// files it takes. A row of a multi-file `selection` offers the selection's: stage or
/// unstage them all, then discard (unstaged only) and stash the ones not conflicted.
/// Otherwise the row's links: Stage (Mark resolved) and Discard changes…, or Unstage.
/// [`file_actions`] come after them.
pub(crate) fn entry_actions(
    entry: &StatusEntry,
    staged: bool,
    selection: &[StatusEntry],
) -> Vec<(EntryAction, String, Vec<StatusEntry>)> {
    let take = if staged {
        EntryAction::Unstage
    } else {
        EntryAction::Stage
    };
    if selection.len() > 1 {
        let label = stage_count_label(staged, selection.len());
        let mut actions = vec![(take, label, selection.to_vec())];
        let others = not_conflicted(selection);
        let count = others.len();
        if count > 0 {
            if !staged {
                let label = tn!("staging.discard_count_menu", count = count).into_owned();
                actions.push((EntryAction::Discard, label, others.clone()));
            }
            let label = tn!("staging.stash_count", count = count).into_owned();
            actions.push((EntryAction::Stash, label, others));
        }
        return actions;
    }
    let conflicted = entry.kind == EntryKind::Conflicted;
    let label = if staged {
        t!("staging.unstage")
    } else if conflicted {
        t!("staging.mark_resolved")
    } else {
        t!("staging.stage")
    };
    let mut actions = vec![(take, label.into_owned(), vec![entry.clone()])];
    if !staged && !conflicted {
        let label = t!("staging.discard_file").into_owned();
        actions.push((EntryAction::Discard, label, vec![entry.clone()]));
    }
    actions
}

/// The selection as rows of `entries` (the `staged` list's files, in order), when it's in
/// that list.
fn pick_of(selection: Option<&Selection>, staged: bool, entries: &[&StatusEntry]) -> Option<Pick> {
    let selection = selection.filter(|s| s.staged == staged)?;
    Some(Pick {
        staged,
        rows: (0..entries.len())
            .filter(|&row| selection.paths.contains(&entries[row].path))
            .collect(),
        anchor: selection
            .anchor
            .as_ref()
            .and_then(|anchor| entries.iter().position(|e| &e.path == anchor)),
    })
}

/// The selection holding `pick`'s rows of `entries`, by path (`None`: no rows).
fn selection_of(pick: &Pick, entries: &[&StatusEntry]) -> Option<Selection> {
    let path_at = |row: usize| entries.get(row).map(|e| e.path.clone());
    (!pick.rows.is_empty()).then(|| Selection {
        staged: pick.staged,
        paths: pick.rows.iter().filter_map(|&row| path_at(row)).collect(),
        anchor: pick.anchor.and_then(path_at),
    })
}

/// The pick after Shift+arrow moved the cursor from row `from` to row `to` of the `staged`
/// list: the rows from the anchor to `to`. The anchor is the selection's own while the
/// selection holds `from` (the extension goes on, or it started at a click there); else it
/// is `from`, the row the cursor left.
fn extend(current: Option<&Pick>, staged: bool, from: usize, to: usize) -> Pick {
    let anchor = current
        .filter(|pick| pick.staged == staged && pick.rows.binary_search(&from).is_ok())
        .and_then(|pick| pick.anchor)
        .unwrap_or(from);
    Pick {
        staged,
        rows: (anchor.min(to)..=anchor.max(to)).collect(),
        anchor: Some(anchor),
    }
}

/// Where the file at `path`, shown from the `staged` list, went after a status read: its
/// entry in the other list and that list (`true`: staged) when it left its own for it
/// (staged or unstaged whole). `None` while it's still in its list, when it's in neither,
/// or conflicted (the conflict tool takes those).
pub(crate) fn moved_to(
    status: &Status,
    path: &BString,
    staged: bool,
) -> Option<(StatusEntry, bool)> {
    let find = |staged: bool| {
        list_entries(status, staged)
            .into_iter()
            .find(|e| e.path == *path)
    };
    if find(staged).is_some() {
        return None;
    }
    find(!staged)
        .filter(|entry| entry.kind != EntryKind::Conflicted)
        .map(|entry| (entry.clone(), !staged))
}

impl EventEmitter<StagingEvent> for StagingView {}

impl Focusable for StagingView {
    /// The file lists' focus, not the commit box's.
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// The file lists by keyboard: the arrows move a cursor over the unstaged, then the staged
/// files; Enter opens the diff, Space stages or unstages, Delete discards (it asks), and
/// Shift+F10 shows the file's actions in the palette.
impl StagingView {
    /// The status the lists show (the headless UI tests).
    #[cfg(test)]
    pub(crate) fn status(&self) -> &Status {
        &self.status
    }

    /// The file lists' focus, which F6 gives the keys (the headless UI tests).
    #[cfg(test)]
    pub(crate) fn file_lists_focus(&self) -> FocusHandle {
        self.focus.clone()
    }

    fn cursor_row(&self) -> Option<(bool, StatusEntry)> {
        let (staged, at) = self.cursor?;
        let entry = if staged {
            self.status.staged().nth(at)
        } else {
            self.status.unstaged().nth(at)
        };
        entry.map(|entry| (staged, entry.clone()))
    }

    fn move_cursor(&mut self, to: Step, cx: &mut Context<Self>) {
        let unstaged = self.status.unstaged().count();
        let len = unstaged + self.status.staged().count();
        let current = self
            .cursor
            .map(|(staged, at)| if staged { unstaged + at } else { at });
        self.cursor = list_keys::step(len, current, to).map(|at| {
            if at < unstaged {
                (false, at)
            } else {
                (true, at - unstaged)
            }
        });
        self.reveal_cursor();
        cx.notify();
    }

    /// Scrolls the cursor's list to the cursor: the lists draw only the rows on screen, so
    /// a row the keys moved to must be brought there.
    fn reveal_cursor(&self) {
        if let Some((staged, at)) = self.cursor {
            let scroll = if staged {
                &self.staged_scroll
            } else {
                &self.unstaged_scroll
            };
            scroll.scroll_to_item(at, ScrollStrategy::Nearest);
        }
    }

    /// The rows `range` of one list, the only ones `uniform_list` draws.
    fn list_rows(
        &mut self,
        staged: bool,
        range: std::ops::Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        // A multi-file selection: its rows' menus take it.
        let selected = self.selected_entries(staged);
        let group = (selected.len() > 1).then(|| Rc::new(selected));
        let entries: Vec<StatusEntry> = if staged {
            let rows = self.status.staged().skip(range.start).take(range.len());
            rows.cloned().collect()
        } else {
            let rows = self.status.unstaged().skip(range.start).take(range.len());
            rows.cloned().collect()
        };
        entries
            .iter()
            .enumerate()
            .map(|(offset, entry)| {
                let group = group
                    .as_ref()
                    .filter(|_| self.is_selected(staged, &entry.path))
                    .cloned();
                // As wide as the list: `uniform_list` lays a row out at its own width, and
                // the old column stretched it (the Stage link sits at the right edge).
                v_flex()
                    .w_full()
                    .child(self.file_row(range.start + offset, entry, staged, group, cx))
                    .into_any_element()
            })
            .collect()
    }

    /// One list as a `uniform_list`: as tall as its rows, sharing what's left with
    /// the other list when they don't fit.
    /// `grow`: its share of the room the two lists divide.
    fn virtual_list(
        &self,
        staged: bool,
        count: usize,
        grow: f32,
        m: &Metrics,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let z = |value: f32| px(value * m.zoom);
        let (id, scroll) = if staged {
            ("staging-staged", &self.staged_scroll)
        } else {
            ("staging-unstaged", &self.unstaged_scroll)
        };
        let rows = cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
            this.list_rows(staged, range, cx)
        });
        div()
            // How the headless UI tests find it (a no-op outside them).
            .debug_selector(move || format!("{id}-list"))
            .flex_1()
            .flex_grow(grow)
            .min_h_0()
            .max_h(m.file_row * count as f32 + z(12.))
            .p(z(6.))
            .child(
                uniform_list(id, count, rows)
                    .track_scroll(scroll)
                    .size_full(),
            )
            .into_any_element()
    }

    fn open_cursor(&mut self, cx: &mut Context<Self>) {
        if let Some((staged, entry)) = self.cursor_row() {
            cx.emit(StagingEvent::OpenDiff { entry, staged });
        }
    }

    fn toggle_cursor(&mut self, cx: &mut Context<Self>) {
        if let Some((staged, entry)) = self.cursor_row() {
            let entries = self.keyed_entries(staged, entry);
            cx.emit(if staged {
                StagingEvent::Unstage(entries)
            } else {
                StagingEvent::Stage(entries)
            });
        }
    }

    fn discard_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((false, entry)) = self.cursor_row() {
            let entries = self.keyed_entries(false, entry);
            self.confirm_discard(entries, window, cx);
        }
    }

    /// What Space and Delete on the cursor row take: the selection when the row is part of
    /// a multi-file one, else the row alone.
    fn keyed_entries(&self, staged: bool, entry: StatusEntry) -> Vec<StatusEntry> {
        let group = self.selected_entries(staged);
        if group.len() > 1 && group.iter().any(|e| e.path == entry.path) {
            group
        } else {
            vec![entry]
        }
    }

    /// Escape: drops a multi-file selection (`false`: there was none, the key goes on).
    fn clear_multi_selection(&mut self, cx: &mut Context<Self>) -> bool {
        if self.selection.as_ref().is_some_and(|s| s.paths.len() > 1) {
            self.selection = None;
            cx.notify();
            true
        } else {
            false
        }
    }

    fn cursor_menu(&mut self, cx: &mut Context<Self>) {
        if let Some((staged, entry)) = self.cursor_row() {
            let keyed = self.keyed_entries(staged, entry.clone());
            let selection = if keyed.len() > 1 { keyed } else { Vec::new() };
            cx.emit(StagingEvent::RowMenu {
                entry,
                staged,
                selection,
            });
        }
    }

    /// Shift+arrows: the cursor moves within its list and the selection runs from the
    /// anchor to it (see [`extend`]). With no cursor yet (or in an empty list) the key
    /// places it as the plain key would, and selects that file alone.
    fn extend_cursor(&mut self, to: Step, cx: &mut Context<Self>) {
        let placed = self
            .cursor
            .filter(|&(staged, _)| !list_entries(&self.status, staged).is_empty());
        let Some((staged, from)) = placed else {
            self.move_cursor(to, cx);
            if let Some((staged, at)) = self.cursor {
                let entries = list_entries(&self.status, staged);
                let pick = Pick {
                    staged,
                    rows: vec![at],
                    anchor: Some(at),
                };
                self.selection = selection_of(&pick, &entries);
            }
            return;
        };
        let entries = list_entries(&self.status, staged);
        let from = from.min(entries.len() - 1);
        let Some(to) = list_keys::step(entries.len(), Some(from), to) else {
            return;
        };
        let current = pick_of(self.selection.as_ref(), staged, &entries);
        self.selection = selection_of(&extend(current.as_ref(), staged, from, to), &entries);
        self.cursor = Some((staged, to));
        self.reveal_cursor();
        cx.notify();
    }

    /// Ctrl+A: every file of the cursor's list (without a cursor, or with one left in a list
    /// that emptied, the unstaged list unless it's empty). A missing cursor is placed on
    /// the anchor, as Shift+arrows do: Space and Delete act through it.
    fn select_all_rows(&mut self, cx: &mut Context<Self>) {
        let staged = self
            .cursor
            .map(|(staged, _)| staged)
            .filter(|&staged| !list_entries(&self.status, staged).is_empty())
            .unwrap_or_else(|| self.status.unstaged().next().is_none());
        let entries = list_entries(&self.status, staged);
        let Some(last) = entries.len().checked_sub(1) else {
            return;
        };
        let anchor = match self.cursor {
            Some((cursor_staged, at)) if cursor_staged == staged => at.min(last),
            _ => 0,
        };
        let pick = Pick {
            staged,
            rows: (0..=last).collect(),
            anchor: Some(anchor),
        };
        self.selection = selection_of(&pick, &entries);
        if self
            .cursor
            .is_none_or(|(cursor_staged, _)| cursor_staged != staged)
        {
            self.cursor = Some((staged, anchor));
            self.reveal_cursor();
        }
        cx.notify();
    }
}

/// The selection (Ctrl+click and Shift+click in either list).
impl StagingView {
    /// The selected files of the `staged` list, in list order (empty: it has none).
    fn selected_entries(&self, staged: bool) -> Vec<StatusEntry> {
        match &self.selection {
            Some(selection) if selection.staged == staged => list_entries(&self.status, staged)
                .into_iter()
                .filter(|e| selection.paths.contains(&e.path))
                .cloned()
                .collect(),
            _ => Vec::new(),
        }
    }

    fn is_selected(&self, staged: bool, path: &BString) -> bool {
        self.selection
            .as_ref()
            .is_some_and(|s| s.staged == staged && s.paths.contains(path))
    }

    /// A click on row `ix` of a list updates the selection (see [`select`]); `true` for a
    /// plain click, which also opens the file's diff.
    fn click_row(&mut self, staged: bool, ix: usize, modifiers: Modifiers) -> bool {
        let (ctrl, shift) = (modifiers.secondary(), modifiers.shift);
        let entries = list_entries(&self.status, staged);
        let current = pick_of(self.selection.as_ref(), staged, &entries);
        let pick = select(current.as_ref(), staged, ix, ctrl, shift);
        self.selection = selection_of(&pick, &entries);
        !(ctrl || shift)
    }

    /// Selects the file at `path` alone, in the `staged` list: the diff pane followed it
    /// there after it was staged or unstaged whole.
    pub(crate) fn select_file(&mut self, path: &BString, staged: bool, cx: &mut Context<Self>) {
        self.selection = Some(Selection {
            staged,
            paths: HashSet::from([path.clone()]),
            anchor: Some(path.clone()),
        });
        cx.notify();
    }

    /// Runs a row menu's [`EntryAction`] on `entries`.
    pub(crate) fn run_entry_action(
        &mut self,
        action: EntryAction,
        entries: Vec<StatusEntry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            EntryAction::Stage => cx.emit(StagingEvent::Stage(entries)),
            EntryAction::Unstage => cx.emit(StagingEvent::Unstage(entries)),
            EntryAction::Discard => self.confirm_discard(entries, window, cx),
            EntryAction::Stash => {
                let paths = entries.into_iter().map(|e| e.path).collect();
                cx.emit(StagingEvent::Stash(paths))
            }
        }
    }

    /// After a reload: keep the selected paths still in their list.
    fn prune_selection(&mut self) {
        let Some(selection) = self.selection.as_mut() else {
            return;
        };
        let present: HashSet<&BString> = list_entries(&self.status, selection.staged)
            .into_iter()
            .map(|e| &e.path)
            .collect();
        selection.paths.retain(|path| present.contains(path));
        if selection
            .anchor
            .as_ref()
            .is_some_and(|anchor| !present.contains(anchor))
        {
            selection.anchor = None;
        }
        if selection.paths.is_empty() {
            self.selection = None;
        }
    }
}

pub struct StagingView {
    status: Status,
    /// `git status` failed since these lists were read: they may be out of date.
    stale: bool,
    /// Where the two lists are scrolled: they draw only the rows on screen.
    unstaged_scroll: UniformListScrollHandle,
    /// A section divider being dragged, and where to; saved when it's released.
    split_drag: Option<(Split, f32)>,
    staged_scroll: UniformListScrollHandle,
    /// The repository uses Git LFS and git-lfs is installed: files get Lock / Unlock.
    lfs_locks: bool,
    summary: Entity<InputState>,
    description: Entity<TextareaState>,
    options: CommitOptions,
    committing: bool,
    /// Output of a failed commit (e.g. a pre-commit hook's complaint).
    commit_error: Option<String>,
    /// HEAD's message, to prefill an amend.
    head_message: Option<String>,
    identity: Identity,
    /// `commit.template` as (summary, description), comments removed.
    template: Option<(String, String)>,
    /// Recent authors of the repository, most frequent first.
    co_author_candidates: Vec<Person>,
    /// People credited with `Co-authored-by:` trailers in the next commit.
    co_authors: Vec<Person>,
    /// Set after a successful commit. The inputs are cleared on the next render, because
    /// clearing them needs the window.
    clear_inputs: bool,
    /// Fill an empty box with the template on the next render (same reason).
    prefill_template: bool,
    /// The file lists' focus (F6, the list keys), their keyboard cursor (staged or not, and
    /// the position in that list), and whether they had the focus when last drawn.
    focus: FocusHandle,
    cursor: Option<(bool, usize)>,
    list_focused: bool,
    /// The selected files (one list at a time); a plain click selects one.
    selection: Option<Selection>,
    _subscriptions: Vec<Subscription>,
}

impl StagingView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let summary = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("commit.summary_placeholder").into_owned())
        });
        let description = cx.new(|cx| {
            // Drawn at a fixed height (`render_commit_box`): a plain textarea's text fills
            // its box and scrolls in it.
            TextareaState::new(window, cx)
                .placeholder(t!("commit.description_placeholder").into_owned())
        });
        // Ctrl+Enter in the summary commits; any edit updates the counter and button.
        let subscriptions =
            vec![
                cx.subscribe(&summary, |this, _, event: &InputEvent, cx| match event {
                    InputEvent::PressEnter {
                        secondary: true, ..
                    } => this.commit(cx),
                    _ => cx.notify(),
                }),
            ];
        Self {
            status: Status::default(),
            stale: false,
            unstaged_scroll: UniformListScrollHandle::new(),
            split_drag: None,
            staged_scroll: UniformListScrollHandle::new(),
            lfs_locks: false,
            summary,
            description,
            options: CommitOptions::default(),
            committing: false,
            commit_error: None,
            head_message: None,
            identity: Identity::default(),
            template: None,
            co_author_candidates: Vec::new(),
            co_authors: Vec::new(),
            clear_inputs: false,
            prefill_template: false,
            focus: cx.focus_handle(),
            cursor: None,
            list_focused: false,
            selection: None,
            _subscriptions: subscriptions,
        }
    }

    /// Whether the rows offer Lock / Unlock on the LFS server.
    pub fn set_lfs_locks(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.lfs_locks != on {
            self.lfs_locks = on;
            cx.notify();
        }
    }

    pub fn set_status(&mut self, status: Status, cx: &mut Context<Self>) {
        self.status = status;
        self.stale = false;
        self.prune_selection();
        cx.notify();
    }

    /// `git status` failed: the lists stay, marked out of date until a status reads again.
    pub fn set_stale(&mut self, cx: &mut Context<Self>) {
        if !self.stale {
            self.stale = true;
            cx.notify();
        }
    }

    pub fn set_commit_context(&mut self, context: CommitContext, cx: &mut Context<Self>) {
        // A new (or the first) template fills an empty box; an unchanged one doesn't come
        // back after the user cleared it.
        if context.template != self.template {
            self.prefill_template = true;
        }
        self.head_message = context.head_message;
        self.identity = context.identity;
        self.template = context.template;
        cx.notify();
    }

    pub fn set_co_author_candidates(&mut self, people: Vec<Person>, cx: &mut Context<Self>) {
        self.co_author_candidates = people;
        cx.notify();
    }

    fn toggle_co_author(&mut self, person: Person, cx: &mut Context<Self>) {
        if let Some(ix) = self.co_authors.iter().position(|p| *p == person) {
            self.co_authors.remove(ix);
        } else {
            self.co_authors.push(person);
        }
        cx.notify();
    }

    /// The box holds nothing the user typed: it is empty or shows just the template.
    fn untouched(&self, cx: &App) -> bool {
        let summary = self.summary.read(cx).value().trim().to_owned();
        let description = self.description.read(cx).value().trim().to_owned();
        (summary.is_empty() && description.is_empty())
            || self
                .template
                .as_ref()
                .is_some_and(|(s, d)| summary == s.trim() && description == d.trim())
    }

    /// Called by the workspace when the commit it ran for us has finished.
    pub fn commit_finished(&mut self, result: Result<(), String>, cx: &mut Context<Self>) {
        self.committing = false;
        match result {
            Ok(()) => {
                self.clear_inputs = true;
                self.prefill_template = true;
                self.co_authors.clear();
                self.options = CommitOptions::default();
                self.commit_error = None;
            }
            Err(output) => self.commit_error = Some(output),
        }
        cx.notify();
    }

    /// Called by the workspace when the user called the commit off (nothing ran).
    /// Who commits are made as (from the last status read).
    pub(crate) fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn commit_cancelled(&mut self, cx: &mut Context<Self>) {
        self.committing = false;
        cx.notify();
    }

    pub(crate) fn commit(&mut self, cx: &mut Context<Self>) {
        if !self.can_commit(cx) {
            return;
        }
        let summary = self.summary.read(cx).value().trim().to_owned();
        let body = self.description.read(cx).value().trim().to_owned();
        let message = if body.is_empty() {
            summary
        } else {
            format!("{summary}\n\n{body}")
        };
        let message = with_co_authors(&message, &self.co_authors);
        self.committing = true;
        self.commit_error = None;
        cx.emit(StagingEvent::Commit {
            message,
            options: self.options,
        });
        cx.notify();
    }

    pub(crate) fn can_commit(&self, cx: &App) -> bool {
        let has_summary = !self.summary.read(cx).value().trim().is_empty();
        let has_changes = self.status.staged().next().is_some() || self.options.amend;
        !self.committing && has_summary && has_changes
    }

    fn toggle_amend(&mut self, amend: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.options.amend = amend;
        // Prefill the message of the commit being amended unless the user typed one.
        if amend
            && self.untouched(cx)
            && let Some(message) = self.head_message.clone()
        {
            let (summary, body) = match message.split_once('\n') {
                Some((summary, body)) => (summary.to_owned(), body.trim().to_owned()),
                None => (message, String::new()),
            };
            self.summary
                .update(cx, |input, cx| input.set_value(summary, window, cx));
            self.description
                .update(cx, |input, cx| input.set_value(body, window, cx));
        }
        cx.notify();
    }

    /// Asks before throwing changes away; emits [`StagingEvent::Discard`] on "Discard".
    pub(crate) fn confirm_discard(
        &mut self,
        entries: Vec<StatusEntry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entries: Vec<StatusEntry> = entries
            .into_iter()
            .filter(|e| e.kind != EntryKind::Conflicted)
            .collect();
        let message = match entries.as_slice() {
            [] => return,
            [one] => t!("staging.discard_single", path = one.display_path()).into_owned(),
            many => tn!("staging.discard_multiple", count = many.len()).into_owned(),
        };
        let detail = t!("staging.discard_detail").into_owned();
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
                this.update(cx, |_, cx| cx.emit(StagingEvent::Discard(entries)))
                    .ok();
            }
        })
        .detach();
    }

    /// `group`: the selection's files, when this row is part of a multi-file selection.
    fn file_row(
        &self,
        ix: usize,
        entry: &StatusEntry,
        staged: bool,
        group: Option<Rc<Vec<StatusEntry>>>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);
        let z = |value: f32| px(value * m.zoom);
        let (letter, color) = match entry.kind {
            EntryKind::Conflicted => ("!", p.del),
            EntryKind::Untracked => ("?", p.add),
            EntryKind::Tracked => {
                let kind = if staged { entry.index } else { entry.worktree };
                change_style(kind.unwrap_or(ChangeKind::Other), &p)
            }
        };
        let path = match (&entry.orig_path, staged) {
            (Some(old), true) => format!("{old} → {}", entry.display_path()),
            _ => entry.display_path(),
        };
        let conflicted = entry.kind == EntryKind::Conflicted;
        let for_open = entry.clone();
        let side = if staged { "staged-row" } else { "unstaged-row" };
        let hover_group = SharedString::from(format!("{side}-{ix}"));
        let selected = self.is_selected(staged, &entry.path);

        let row = h_flex()
            .id((side, ix))
            .group(hover_group.clone())
            .h(m.file_row)
            .flex_shrink_0()
            .px(z(6.))
            .gap(z(8.))
            .rounded(m.radius)
            .cursor_pointer()
            .when(selected, |row| row.bg(p.sel))
            .when(!selected, |row| row.hover(move |style| style.bg(p.hover)))
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                // The keys go on from the clicked file.
                this.cursor = Some((staged, ix));
                this.focus.focus(window, cx);
                // Ctrl and Shift only change the selection; a plain click opens the diff.
                if this.click_row(staged, ix, event.modifiers()) {
                    cx.emit(StagingEvent::OpenDiff {
                        entry: for_open.clone(),
                        staged,
                    })
                }
                cx.notify();
            }))
            .child(status_badge(letter, color, &m))
            .child(path_label(&path, &p, &m))
            .when(
                self.list_focused && self.cursor == Some((staged, ix)),
                |row| row.relative().child(list_keys::cursor_bar(p.acc, &m)),
            );

        // The row opens the diff on click; its links stop the click (see `link`).
        let for_action = entry.clone();
        let row = if staged {
            row.child(link(
                ("unstage", ix),
                t!("staging.unstage").into_owned(),
                p.acc,
                m.text.xs,
                cx.listener(move |_, _, _, cx| {
                    cx.emit(StagingEvent::Unstage(vec![for_action.clone()]))
                }),
            ))
        } else {
            let for_discard = entry.clone();
            let label = if conflicted {
                t!("staging.mark_resolved")
            } else {
                t!("staging.stage")
            };
            // Discard shows while the row is hovered, selected or under the keyboard's cursor,
            // and when Tab reaches it; a conflicted file can't be discarded.
            let shown = selected || (self.list_focused && self.cursor == Some((staged, ix)));
            row.when(!conflicted, |row| {
                row.child(
                    link(
                        ("discard", ix),
                        t!("staging.discard").into_owned(),
                        p.fg3,
                        m.text.xs,
                        cx.listener(move |this, _, window, cx| {
                            this.confirm_discard(vec![for_discard.clone()], window, cx)
                        }),
                    )
                    .reveal_on(hover_group, shown),
                )
            })
            .child(link(
                ("stage", ix),
                label.into_owned(),
                p.acc,
                m.text.xs,
                cx.listener(move |_, _, _, cx| {
                    cx.emit(StagingEvent::Stage(vec![for_action.clone()]))
                }),
            ))
        };

        use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
        // The row's links (or its multi-file selection's actions), then the file's own.
        let selection = group.as_deref().map_or(&[][..], Vec::as_slice);
        let first = entry_actions(entry, staged, selection);
        let actions = file_actions(entry, self.lfs_locks);
        let (view, entry) = (cx.entity().downgrade(), entry.clone());
        row.context_menu(move |menu, _, _| {
            let menu = first.iter().fold(menu, |menu, (action, label, entries)| {
                let (view, action, entries) = (view.clone(), *action, entries.clone());
                menu.item(
                    PopupMenuItem::new(label.clone()).on_click(move |_, window, cx| {
                        let entries = entries.clone();
                        view.update(cx, |this, cx| {
                            this.run_entry_action(action, entries, window, cx)
                        })
                        .ok();
                    }),
                )
            });
            actions
                .iter()
                .enumerate()
                .fold(menu, |menu, (ix, (action, label, separated))| {
                    let menu = if ix == 0 || *separated {
                        menu.separator()
                    } else {
                        menu
                    };
                    let (view, entry, action) = (view.clone(), entry.clone(), *action);
                    menu.item(PopupMenuItem::new(label.clone()).on_click(move |_, _, cx| {
                        view.update(cx, |_, cx| cx.emit(action.event(&entry))).ok();
                    }))
                })
        })
        .into_any_element()
    }

    fn render_files(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);
        let z = |value: f32| px(value * m.zoom);
        let unstaged: Vec<StatusEntry> = self.status.unstaged().cloned().collect();
        let staged: Vec<StatusEntry> = self.status.staged().cloned().collect();
        // A list's multi-file selection: its header links and its rows' menus take it.
        let multi = |entries: Vec<StatusEntry>| (entries.len() > 1).then(|| Rc::new(entries));
        let unstaged_group = multi(self.selected_entries(false));
        let staged_group = multi(self.selected_entries(true));

        let mut unstaged_actions = Vec::new();
        if !unstaged.is_empty() {
            let (take, label, discard_label) = match &unstaged_group {
                Some(group) => {
                    let discard = not_conflicted(group);
                    let count = discard.len();
                    let discard_label =
                        (count > 0).then(|| tn!("staging.discard_count", count = count));
                    (
                        group.to_vec(),
                        stage_count_label(false, group.len()),
                        discard_label,
                    )
                }
                None => (
                    unstaged.clone(),
                    t!("staging.stage_all").into_owned(),
                    Some(t!("staging.discard_all")),
                ),
            };
            let for_discard = take.clone();
            unstaged_actions.push(
                link(
                    "stage-all",
                    label,
                    p.acc,
                    m.text.small,
                    cx.listener(move |_, _, _, cx| cx.emit(StagingEvent::Stage(take.clone()))),
                )
                .into_any_element(),
            );
            if let Some(discard_label) = discard_label {
                unstaged_actions.push(
                    link(
                        "discard-all",
                        discard_label.into_owned(),
                        p.fg3,
                        m.text.small,
                        cx.listener(move |this, _, window, cx| {
                            this.confirm_discard(for_discard.clone(), window, cx)
                        }),
                    )
                    .into_any_element(),
                );
            }
        }
        let mut staged_actions = Vec::new();
        if !staged.is_empty() {
            let (take, label) = match &staged_group {
                Some(group) => (group.to_vec(), stage_count_label(true, group.len())),
                None => (staged.clone(), t!("staging.unstage_all").into_owned()),
            };
            staged_actions.push(
                link(
                    "unstage-all",
                    label,
                    p.acc,
                    m.text.small,
                    cx.listener(move |_, _, _, cx| cx.emit(StagingEvent::Unstage(take.clone()))),
                )
                .into_any_element(),
            );
        }
        let empty = if self.status.is_clean() {
            t!("details.working_clean")
        } else {
            t!("staging.nothing_staged")
        };
        let has_unstaged = !unstaged.is_empty();
        let has_staged = !staged.is_empty();
        // Only the rows on screen are built: 20,000 untracked files took ~1.8 s a frame.
        // As dragged: Unstaged's share of the room; a list shorter than its share still
        // leaves the rest to the other.
        let share = split::current(Split::Lists, self.split_drag, cx);
        let (unstaged_grow, staged_grow) = share.map_or((1., 1.), |share| (share, 1. - share));
        let unstaged_list =
            has_unstaged.then(|| self.virtual_list(false, unstaged.len(), unstaged_grow, &m, cx));
        let staged_list =
            has_staged.then(|| self.virtual_list(true, staged.len(), staged_grow, &m, cx));

        v_flex()
            .id("staging-files")
            .key_context(list_keys::STAGING)
            .track_focus(&self.focus)
            .on_action(
                cx.listener(|this, _: &list_keys::NextRow, _, cx| this.move_cursor(Step::Next, cx)),
            )
            .on_action(cx.listener(|this, _: &list_keys::PreviousRow, _, cx| {
                this.move_cursor(Step::Previous, cx)
            }))
            .on_action(
                cx.listener(|this, _: &list_keys::FirstRow, _, cx| {
                    this.move_cursor(Step::First, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &list_keys::LastRow, _, cx| this.move_cursor(Step::Last, cx)),
            )
            .on_action(cx.listener(|this, _: &list_keys::OpenRow, _, cx| this.open_cursor(cx)))
            .on_action(cx.listener(|this, _: &list_keys::ToggleRow, _, cx| this.toggle_cursor(cx)))
            .on_action(cx.listener(|this, _: &list_keys::DeleteRow, window, cx| {
                this.discard_cursor(window, cx)
            }))
            .on_action(cx.listener(|this, _: &list_keys::RowMenu, _, cx| this.cursor_menu(cx)))
            .on_action(cx.listener(|this, _: &list_keys::ExtendToNextRow, _, cx| {
                this.extend_cursor(Step::Next, cx)
            }))
            .on_action(
                cx.listener(|this, _: &list_keys::ExtendToPreviousRow, _, cx| {
                    this.extend_cursor(Step::Previous, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &list_keys::ExtendToFirstRow, _, cx| {
                this.extend_cursor(Step::First, cx)
            }))
            .on_action(cx.listener(|this, _: &list_keys::ExtendToLastRow, _, cx| {
                this.extend_cursor(Step::Last, cx)
            }))
            .on_action(
                cx.listener(|this, _: &list_keys::SelectAllRows, _, cx| this.select_all_rows(cx)),
            )
            // Not a key binding (key listeners run only after no binding took the key):
            // Escape drops a multi-file selection first, then closes the diff the list opened.
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                let key = &event.keystroke;
                if key.key == "escape" && !key.modifiers.modified() {
                    if !this.clear_multi_selection(cx) {
                        cx.emit(StagingEvent::CloseDiff);
                    }
                    cx.stop_propagation();
                }
            }))
            .flex_1()
            .min_h_0()
            .on_drag_move(cx.listener(Self::on_lists_drag))
            .child(section_header(
                t!("staging.unstaged").into_owned(),
                unstaged.len(),
                false,
                unstaged_actions,
                &p,
                &m,
            ))
            .when(has_unstaged, |this| this.children(unstaged_list))
            .when(has_unstaged, |this| {
                this.child(split::divider(Split::Lists, &p, &m))
            })
            .child(
                section_header(
                    t!("staging.staged").into_owned(),
                    staged.len(),
                    has_unstaged,
                    staged_actions,
                    &p,
                    &m,
                )
                .when(!has_staged, |this| this.text_color(p.fg3)),
            )
            .when(has_staged, |this| this.children(staged_list))
            .when(!has_staged, |this| {
                // Below a long unstaged list the note takes its own height, not half the
                // panel: both would be `flex_1` and split it.
                let long_unstaged = unstaged.len() > 12;
                this.child(
                    div()
                        .when(long_unstaged, |note| note.flex_none().h(z(80.)))
                        .when(!long_unstaged, |note| {
                            note.flex_1().flex_grow(staged_grow).min_h(z(80.))
                        })
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(m.text.small)
                        .text_color(p.fg3)
                        .child(empty.into_owned()),
                )
            })
    }

    /// "Co-authors" button; its popover lists recent authors to credit in the commit.
    fn render_co_authors(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let own = self.identity.author().1.map(str::to_lowercase);
        let candidates: Vec<Person> = self
            .co_author_candidates
            .iter()
            .filter(|person| Some(person.email.to_lowercase()) != own)
            .take(MAX_CO_AUTHOR_CHOICES)
            .cloned()
            .collect();
        let label = if self.co_authors.is_empty() {
            t!("commit.co_authors").into_owned()
        } else {
            t!("commit.co_authors_count", count = self.co_authors.len()).into_owned()
        };
        let names = crate::util::join_list(self.co_authors.iter().map(|person| &person.name));
        let (muted, xs) = (cx.palette().fg3, cx.metrics().text.xs);
        let view = cx.entity().downgrade();

        let popover = Popover::new("co-authors")
            .anchor(Anchor::BottomLeft)
            .default_open(debug_open_co_authors())
            .trigger(
                Button::new("co-authors-trigger")
                    .xsmall()
                    .ghost()
                    .label(label),
            )
            .content(move |_, _, cx| {
                let m = *cx.metrics();
                let selected: Vec<Person> = view
                    .upgrade()
                    .map(|view| view.read(cx).co_authors.clone())
                    .unwrap_or_default();
                let mut list = v_flex()
                    .id("co-author-list")
                    .gap_1()
                    .min_w(m.dp(260.))
                    .max_h(m.dp(280.))
                    .overflow_y_scroll();
                if candidates.is_empty() {
                    list = list.child(
                        div()
                            .text_size(m.text.small)
                            .child(t!("commit.co_authors_empty").into_owned()),
                    );
                }
                for (ix, person) in candidates.iter().enumerate() {
                    let (view, for_click) = (view.clone(), person.clone());
                    list = list.child(
                        Checkbox::new(("co-author", ix))
                            .label(person.ident())
                            .checked(selected.contains(person))
                            .on_click(move |_, _, cx| {
                                let person = for_click.clone();
                                view.update(cx, |this, cx| this.toggle_co_author(person, cx))
                                    .ok();
                            }),
                    );
                }
                list
            });

        h_flex()
            .gap_2()
            .child(popover)
            .when(!names.is_empty(), |this| {
                this.child(
                    truncated(
                        "co-authors",
                        t!("commit.co_authors_with", names = names).into_owned(),
                    )
                    .flex_1()
                    .text_size(xs)
                    .text_color(muted),
                )
            })
    }

    /// `height`: as the user dragged it (design pixels); the description takes what the
    /// rest leaves. `None`: as tall as it needs, the description 76 px.
    fn render_commit_box(
        &mut self,
        height: Option<f32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        if std::mem::take(&mut self.clear_inputs) {
            self.summary
                .update(cx, |input, cx| input.set_value("", window, cx));
            self.description
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
        if std::mem::take(&mut self.prefill_template)
            && !self.options.amend
            && let Some((summary, description)) = self.template.clone()
            && self.summary.read(cx).value().trim().is_empty()
            && self.description.read(cx).value().trim().is_empty()
        {
            self.summary
                .update(cx, |input, cx| input.set_value(summary, window, cx));
            self.description
                .update(cx, |input, cx| input.set_value(description, window, cx));
        }
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);
        let z = |value: f32| px(value * m.zoom);
        let summary_len = self.summary.read(cx).value().trim().chars().count();
        let counter_color = if summary_len > SUMMARY_HARD_LIMIT {
            p.del
        } else if summary_len > SUMMARY_SOFT_LIMIT {
            p.modified
        } else {
            p.fg3
        };
        let staged = self.status.staged().count();
        let label = if self.committing {
            t!("commit.committing").into_owned()
        } else if self.options.amend {
            t!("commit.amend_button").into_owned()
        } else {
            tn!("commit.button", count = staged).into_owned()
        };
        // Clicking it sets the name and email; a guessed or missing one says so in the
        // modified color.
        let identity = match self.identity.author() {
            (Some(name), Some(email)) if self.identity.guessed => (
                t!("commit.identity_guessed", name = name, email = email).into_owned(),
                p.modified,
            ),
            (Some(name), Some(email)) => (
                t!("commit.identity", name = name, email = email).into_owned(),
                p.fg3,
            ),
            _ => (t!("commit.no_identity").into_owned(), p.modified),
        };
        let identity_hover = p.acc;
        let options = self.options;
        let counter = div()
            .font_family(mono_font())
            .text_size(m.text.mono_small)
            .text_color(counter_color)
            .child(format!("{summary_len}/{SUMMARY_HARD_LIMIT}"));
        let can_commit = self.can_commit(cx);

        v_flex()
            .debug_selector(|| "commit-box".into())
            .flex_shrink_0()
            .when_some(height, |this, height| this.h(m.dp(height)))
            .p(z(12.))
            .gap(z(8.))
            .border_t_1()
            .border_color(p.line)
            .child(
                gitweft_ui::widgets::LinkButton::new(
                    "commit-identity",
                    identity.0.clone(),
                    identity.1,
                )
                .size(m.text.xs)
                .truncate() // untooltipped: its tooltip starts with the whole text
                .hover_color(identity_hover)
                // Cut short, the link's text is whole here.
                .tooltip(format!(
                    "{}\n{}",
                    identity.0,
                    identity_tooltip(&self.identity)
                ))
                .on_click(cx.listener(|_, _, _, cx| cx.emit(StagingEvent::EditIdentity))),
            )
            // The kit's own `Input::h` sizes only multi-line inputs.
            .child(Styled::h(Input::new(&self.summary).suffix(counter), z(32.)).bg(p.bg))
            // The design's 76 px, whatever the text: a longer description scrolls in it.
            // The multi-line input takes Tab for itself (gpui-base binds it to IndentInline),
            // so the keys were trapped in the Description: Tab couldn't reach the options or
            // Commit, and Shift+Tab couldn't go back to the Summary. Caught before the
            // input sees it, Tab moves on as it does in every other field; Ctrl+] and Ctrl+[
            // still indent, for the rare tab in a commit message.
            .child(
                div()
                    .w_full()
                    .when(height.is_some(), |this| this.flex_1().min_h(z(40.)))
                    .capture_action(cx.listener(|_, _: &IndentInline, window, cx| {
                        cx.stop_propagation();
                        window.focus_next(cx);
                    }))
                    .capture_action(cx.listener(|_, _: &OutdentInline, window, cx| {
                        cx.stop_propagation();
                        window.focus_prev(cx);
                    }))
                    .child(
                        Textarea::new(&self.description)
                            .map(|area| match height {
                                Some(_) => area.h(relative(1.)),
                                None => area.h(z(76.)),
                            })
                            .bg(p.bg),
                    ),
            )
            .child(
                h_flex()
                    .gap(z(14.))
                    .flex_wrap()
                    .child(option_checkbox(
                        "amend",
                        t!("commit.amend"),
                        options.amend,
                        &p,
                        &m,
                        cx.listener(|this, checked: &bool, window, cx| {
                            this.toggle_amend(*checked, window, cx)
                        }),
                    ))
                    .child(option_checkbox(
                        "signoff",
                        t!("commit.signoff"),
                        options.signoff,
                        &p,
                        &m,
                        cx.listener(|this, checked: &bool, _, cx| {
                            this.options.signoff = *checked;
                            cx.notify();
                        }),
                    ))
                    .child(option_checkbox(
                        "no-verify",
                        t!("commit.no_verify"),
                        options.no_verify,
                        &p,
                        &m,
                        cx.listener(|this, checked: &bool, _, cx| {
                            this.options.no_verify = *checked;
                            cx.notify();
                        }),
                    )),
            )
            .child(self.render_co_authors(cx))
            .when_some(self.commit_error.clone(), |this, error| {
                this.child(
                    div()
                        .id("commit-error")
                        .max_h(z(140.))
                        .overflow_y_scroll()
                        .p(z(8.))
                        .rounded(m.radius)
                        .bg(p.del_bg)
                        .text_color(p.del)
                        .font_family(mono_font())
                        .text_size(m.text.mono_small)
                        .child(error),
                )
            })
            .child(
                Button::new("commit")
                    .primary()
                    .h(z(34.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .label(label)
                    .disabled(!can_commit)
                    // Disabled as designed: the accent at 45 % (the kit's is a pale tint;
                    // our style is laid over its disabled one).
                    .when(!can_commit, |button| {
                        button.bg(p.acc).text_color(p.acc_fg).opacity(0.45)
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.commit(cx))),
            )
    }
}

/// A commit option's checkbox as designed: the label at the small text size in the
/// secondary color. The kit draws a `label` in the foreground color, so it's a child here.
fn option_checkbox(
    id: &'static str,
    label: std::borrow::Cow<'_, str>,
    checked: bool,
    p: &Palette,
    m: &Metrics,
    on_click: impl Fn(&bool, &mut Window, &mut App) + 'static,
) -> Div {
    let label = label.into_owned();
    let box_color = p.fg3;
    div()
        .relative()
        .child(
            Checkbox::new(id)
                .small()
                .checked(checked)
                .accessibility_label(label.clone())
                .text_color(p.fg2)
                .text_size(m.text.small)
                .child(label)
                .on_click(on_click),
        )
        // The kit draws the unchecked square in its fields' border colour (line); the
        // design's is fg3. A border laid over the kit's square (its small size, at the
        // checkbox's top left) takes no clicks, so the kit's focus and keys stay.
        .when(!checked, |this| {
            this.child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size(rems(0.875))
                    .rounded(m.dp(4.))
                    .border_1()
                    .border_color(box_color),
            )
        })
}

/// Debug builds: `GITWEFT_DEBUG_OPEN_CO_AUTHORS=1` opens the co-author list, for
/// screenshot checks without mouse input.
fn debug_open_co_authors() -> bool {
    cfg!(debug_assertions) && std::env::var_os("GITWEFT_DEBUG_OPEN_CO_AUTHORS").is_some()
}

impl Render for StagingView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (p, m) = (*cx.palette(), *cx.metrics());
        self.list_focused = self.focus.contains_focused(window, cx);
        let (del, del_bg) = (p.del, p.del_bg);
        let box_height = split::current(Split::CommitBox, self.split_drag, cx);
        v_flex()
            .size_full()
            .text_size(m.text.ui)
            .on_drag_move(cx.listener(Self::on_commit_box_drag))
            // A section divider's drag ends wherever the mouse is released.
            .on_mouse_up(MouseButton::Left, cx.listener(Self::release_split))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::release_split))
            .when(self.stale, |this| {
                this.child(
                    div()
                        .id("staging-stale")
                        .flex_shrink_0()
                        .px(px(12. * m.zoom))
                        .py(px(4. * m.zoom))
                        .bg(del_bg)
                        .text_color(del)
                        .child(t!("status.stale").into_owned()),
                )
            })
            .child(self.render_files(cx))
            .child(split::divider(Split::CommitBox, &p, &m))
            .child(self.render_commit_box(box_height, window, cx))
    }
}

/// The commit box's least height when dragged (design pixels): its fields and buttons with
/// a few lines of description.
const COMMIT_BOX_MIN: f32 = 240.;

impl StagingView {
    /// The divider above the commit box: its height, leaving the lists' two headers and
    /// a little of them.
    fn on_commit_box_drag(
        &mut self,
        event: &DragMoveEvent<Split>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if *event.drag(cx) != Split::CommitBox {
            return;
        }
        let m = *cx.metrics();
        let bounds = event.bounds;
        let most = f32::from(bounds.size.height - m.section * 2.) / m.zoom - 60.;
        let height = f32::from(bounds.bottom() - event.event.position.y) / m.zoom;
        let height = height.min(most).max(COMMIT_BOX_MIN);
        self.split_drag = Some((Split::CommitBox, height));
        cx.notify();
    }

    /// The divider between Unstaged and Staged: Unstaged's share of what the two lists have
    /// under their headers.
    fn on_lists_drag(
        &mut self,
        event: &DragMoveEvent<Split>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if *event.drag(cx) != Split::Lists {
            return;
        }
        let section = cx.metrics().section;
        let bounds = event.bounds;
        let at = event.event.position.y;
        let share = split::share(at, bounds.top() + section, bounds.bottom() - section);
        self.split_drag = Some((Split::Lists, share));
        cx.notify();
    }

    fn release_split(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if split::release(&mut self.split_drag, cx) {
            cx.notify();
        }
    }
}

/// A 40 px section header: the title and its count, then text actions on the right.
fn section_header(
    title: String,
    count: usize,
    top_border: bool,
    actions: Vec<AnyElement>,
    p: &Palette,
    m: &Metrics,
) -> Div {
    let z = |value: f32| px(value * m.zoom);
    h_flex()
        .h(m.section)
        .flex_shrink_0()
        .px(z(12.))
        .gap(z(8.))
        .border_b_1()
        .when(top_border, |this| this.border_t_1())
        .border_color(p.line)
        .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
        .child(section_count(count, p, m))
        .child(div().flex_1())
        .children(actions)
}

/// The identity link's tooltip: where git's configuration sets the name and the email,
/// or which environment variables override it, then what a click does.
fn identity_tooltip(identity: &Identity) -> String {
    let overriding: Vec<&str> = [
        ("GIT_AUTHOR_NAME", &identity.env_name),
        ("GIT_AUTHOR_EMAIL", &identity.env_email),
    ]
    .into_iter()
    .filter_map(|(key, value)| value.as_ref().map(|_| key))
    .collect();
    if !overriding.is_empty() {
        return t!(
            "commit.identity_from_env",
            variables = crate::util::join_list(overriding),
            name = identity.name.as_deref().unwrap_or_default(),
            email = identity.email.as_deref().unwrap_or_default()
        )
        .into_owned();
    }
    let place = |origin: &Option<ConfigOrigin>| origin.as_ref().map(config_place);
    match (place(&identity.name_origin), place(&identity.email_origin)) {
        (None, None) => t!("commit.identity_edit"),
        (Some(name), Some(email)) if name == email => t!("commit.identity_from", place = name),
        (name, email) => {
            let elsewhere = || t!("commit.identity_not_in_config").into_owned();
            t!(
                "commit.identity_from_each",
                name_place = name.unwrap_or_else(elsewhere),
                email_place = email.unwrap_or_else(elsewhere)
            )
        }
    }
    .into_owned()
}

/// A configuration file and its scope, for people: `~/.gitconfig, your global config`.
fn config_place(origin: &ConfigOrigin) -> String {
    let scope = match origin.scope.as_str() {
        "system" => t!("config_scope.system"),
        "global" => t!("config_scope.global"),
        "local" => t!("config_scope.local"),
        "worktree" => t!("config_scope.worktree"),
        "command" => t!("config_scope.command"),
        other => other.to_owned().into(),
    };
    if origin.file.is_empty() {
        return scope.into_owned();
    }
    // git writes the file with `/`, on Windows too.
    let home = gitweft_platform::dirs::home().and_then(|home| {
        home.to_str()
            .map(|home| home.replace(std::path::MAIN_SEPARATOR, "/"))
    });
    let file = home_relative(&origin.file, home.as_deref());
    t!("commit.config_place", file = file, scope = scope).into_owned()
}

/// `path` with the home folder written `~`.
fn home_relative(path: &str, home: Option<&str>) -> String {
    match home.map(|home| home.trim_end_matches('/')) {
        Some(home) if !home.is_empty() => match path.strip_prefix(home) {
            Some(rest) if rest.starts_with('/') => format!("~{rest}"),
            _ => path.to_owned(),
        },
        _ => path.to_owned(),
    }
}

#[cfg(test)]
mod identity_tests {
    use super::{ConfigOrigin, Identity, home_relative, identity_tooltip};

    fn origin(scope: &str, file: &str) -> Option<ConfigOrigin> {
        Some(ConfigOrigin {
            scope: scope.to_owned(),
            file: file.to_owned(),
        })
    }

    #[test]
    fn the_tooltip_says_where_the_identity_is_set() {
        let mut id = Identity::default();
        assert_eq!(
            identity_tooltip(&id),
            "Change your name and email for commits"
        );
        id.name_origin = origin("local", ".git/config");
        id.email_origin = origin("local", ".git/config");
        assert_eq!(
            identity_tooltip(&id),
            "Name and email from .git/config, this repository's config · click to change them"
        );
        id.email_origin = origin("command", "");
        assert_eq!(
            identity_tooltip(&id),
            "Name from .git/config, this repository's config; email from the command line (-c) · click to change them"
        );
        id.email_origin = None;
        assert_eq!(
            identity_tooltip(&id),
            "Name from .git/config, this repository's config; email from the environment or git's guess · click to change them"
        );
        id.name = Some("Ada".into());
        id.email = Some("ada@example.invalid".into());
        id.env_email = Some("ada@work.example.invalid".into());
        assert_eq!(
            identity_tooltip(&id),
            "From the environment (GIT_AUTHOR_EMAIL), over the configured Ada <ada@example.invalid> · click to change the configuration"
        );
        id.env_name = Some("Ada at Work".into());
        assert_eq!(
            identity_tooltip(&id),
            "From the environment (GIT_AUTHOR_NAME, GIT_AUTHOR_EMAIL), over the configured Ada <ada@example.invalid> · click to change the configuration"
        );
        assert_eq!(
            id.author(),
            (Some("Ada at Work"), Some("ada@work.example.invalid"))
        );
    }

    #[test]
    fn the_home_folder_is_written_as_a_tilde() {
        let home = Some("/home/ada");
        assert_eq!(home_relative("/home/ada/.gitconfig", home), "~/.gitconfig");
        assert_eq!(
            home_relative("/home/adam/.gitconfig", home),
            "/home/adam/.gitconfig"
        );
        assert_eq!(home_relative(".git/config", home), ".git/config");
        assert_eq!(home_relative("/home/ada/x", Some("/home/ada/")), "~/x");
        assert_eq!(home_relative("/etc/gitconfig", None), "/etc/gitconfig");
    }
}

/// A text button ("Stage all"). It stops the click: the rows it sits in open a diff when
/// clicked (and kit buttons stop propagation only while loading).
pub(crate) fn link(
    id: impl Into<ElementId>,
    label: String,
    color: Hsla,
    size: Pixels,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gitweft_ui::widgets::LinkButton {
    // A tab stop with a focus outline; Enter and Space click it.
    gitweft_ui::widgets::LinkButton::new(id, label, color)
        .size(size)
        .on_click(on_click)
}

/// The staging lists in a GPUI test window: selecting several files by keyboard and
/// staging them together, through the app's own list keys.
#[cfg(test)]
mod view_tests {
    use std::cell::RefCell;
    use std::path::Path;

    use gitweft_ui::SettingsStore;
    // Rust's own `#[test]`: `gpui_kit::*` (through `super::*`) brings GPUI's `test` macro.
    use std::prelude::v1::test;

    use super::*;

    fn modified(path: &str) -> StatusEntry {
        StatusEntry {
            path: path.into(),
            orig_path: None,
            index: None,
            worktree: Some(ChangeKind::Modified),
            kind: EntryKind::Tracked,
            code: *b".M",
            submodule: false,
        }
    }

    /// The staging view with a.txt, b.txt and c.txt modified, its file lists focused and
    /// drawn; returns its window and the files of each Stage it asked for.
    fn staging<'a>(
        cx: &'a mut TestAppContext,
        dir: &Path,
    ) -> (&'a mut VisualTestContext, Rc<RefCell<Vec<Vec<String>>>>) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            SettingsStore::init_with_file(cx, &dir.join("settings.json"));
            list_keys::bind_keys(cx);
        });
        let (view, window) = cx.add_window_view(StagingView::new);
        let staged = Rc::new(RefCell::new(Vec::new()));
        let sink = staged.clone();
        window.update(|window, cx| {
            cx.subscribe(&view, move |_, event: &StagingEvent, _| {
                if let StagingEvent::Stage(entries) = event {
                    let paths = entries.iter().map(StatusEntry::display_path).collect();
                    sink.borrow_mut().push(paths);
                }
            })
            .detach();
            let entries = ["a.txt", "b.txt", "c.txt"].map(modified).into();
            let status = Status {
                entries,
                ..Status::default()
            };
            view.update(cx, |view, cx| view.set_status(status, cx));
            // The view doesn't take the focus itself; keys go by the last frame drawn.
            let focus = view.read(cx).focus.clone();
            focus.focus(window, cx);
            window.draw(cx).clear(cx);
        });
        window.run_until_parked();
        (window, staged)
    }

    #[test]
    fn shift_down_selects_files_and_space_stages_them_together() {
        let dir = tempfile::tempdir().unwrap();
        let mut cx = TestAppContext::single();
        let (window, staged) = staging(&mut cx, dir.path());
        window.simulate_keystrokes("shift-down shift-down space");
        assert_eq!(*staged.borrow(), [vec!["a.txt", "b.txt"]]);
    }

    #[test]
    fn ctrl_a_selects_every_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut cx = TestAppContext::single();
        let (window, staged) = staging(&mut cx, dir.path());
        window.simulate_keystrokes("secondary-a space");
        assert_eq!(*staged.borrow(), [vec!["a.txt", "b.txt", "c.txt"]]);
    }

    #[test]
    fn escape_drops_the_selection_and_space_takes_the_cursor_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut cx = TestAppContext::single();
        let (window, staged) = staging(&mut cx, dir.path());
        window.simulate_keystrokes("shift-down shift-down escape space");
        assert_eq!(*staged.borrow(), [vec!["b.txt"]]);
    }
}

#[cfg(test)]
mod selection_tests {
    use super::{Pick, extend, select};

    fn pick(staged: bool, rows: &[usize], anchor: usize) -> Pick {
        Pick {
            staged,
            rows: rows.to_vec(),
            anchor: Some(anchor),
        }
    }

    #[test]
    fn staging_plain_click_picks_one_row() {
        assert_eq!(select(None, false, 3, false, false), pick(false, &[3], 3));
        let before = pick(false, &[1, 2, 5], 1);
        assert_eq!(
            select(Some(&before), false, 4, false, false),
            pick(false, &[4], 4)
        );
    }

    #[test]
    fn staging_ctrl_click_toggles_a_row() {
        let one = select(None, false, 1, false, false);
        let two = select(Some(&one), false, 4, true, false);
        assert_eq!(two, pick(false, &[1, 4], 4));
        let three = select(Some(&two), false, 2, true, false);
        assert_eq!(three, pick(false, &[1, 2, 4], 2));
        // Off again, then the last one off: nothing is left.
        let off = select(Some(&three), false, 4, true, false);
        assert_eq!(off, pick(false, &[1, 2], 4));
        let alone = pick(false, &[1], 1);
        assert_eq!(
            select(Some(&alone), false, 1, true, false).rows,
            Vec::<usize>::new()
        );
    }

    #[test]
    fn staging_shift_click_picks_a_range_either_way() {
        let start = select(None, true, 2, false, false);
        assert_eq!(
            select(Some(&start), true, 5, false, true),
            pick(true, &[2, 3, 4, 5], 2)
        );
        assert_eq!(
            select(Some(&start), true, 0, false, true),
            pick(true, &[0, 1, 2], 2)
        );
        // A second Shift+click keeps the anchor and replaces the range.
        let down = select(Some(&start), true, 5, false, true);
        assert_eq!(
            select(Some(&down), true, 1, false, true),
            pick(true, &[1, 2], 2)
        );
    }

    #[test]
    fn staging_shift_click_after_ctrl_starts_at_the_ctrl_row() {
        let start = select(None, false, 0, false, false);
        let ctrl = select(Some(&start), false, 6, true, false);
        assert_eq!(ctrl, pick(false, &[0, 6], 6));
        // Shift: the range from the Ctrl-clicked row replaces the selection.
        assert_eq!(
            select(Some(&ctrl), false, 4, false, true),
            pick(false, &[4, 5, 6], 6)
        );
        // Ctrl+Shift: the range is added.
        assert_eq!(
            select(Some(&ctrl), false, 4, true, true),
            pick(false, &[0, 4, 5, 6], 6)
        );
    }

    #[test]
    fn staging_modified_click_in_the_other_list_starts_over() {
        let unstaged = pick(false, &[1, 2, 3], 1);
        assert_eq!(
            select(Some(&unstaged), true, 4, false, true),
            pick(true, &[4], 4)
        );
        assert_eq!(
            select(Some(&unstaged), true, 0, true, false),
            pick(true, &[0], 0)
        );
        // Shift with no anchor (it was reloaded away) acts like a plain click.
        let lost = Pick {
            staged: false,
            rows: vec![2],
            anchor: None,
        };
        assert_eq!(
            select(Some(&lost), false, 5, false, true),
            pick(false, &[5], 5)
        );
    }

    #[test]
    fn staging_shift_arrows_extend_from_the_row_the_cursor_left() {
        // No selection yet: from the cursor's row to the next one.
        assert_eq!(extend(None, false, 2, 3), pick(false, &[2, 3], 2));
        // Going on down, then back up.
        let down = extend(Some(&pick(false, &[2, 3], 2)), false, 3, 4);
        assert_eq!(down, pick(false, &[2, 3, 4], 2));
        assert_eq!(extend(Some(&down), false, 4, 3), pick(false, &[2, 3], 2));
        // Up past the anchor.
        assert_eq!(
            extend(Some(&pick(false, &[2], 2)), false, 2, 1),
            pick(false, &[1, 2], 2)
        );
        // A click selected row 1, then the plain keys took the cursor to row 4: the range
        // starts at the cursor.
        assert_eq!(
            extend(Some(&pick(false, &[1], 1)), false, 4, 5),
            pick(false, &[4, 5], 4)
        );
        // After Ctrl+clicks, the range from the last one replaces them.
        assert_eq!(
            extend(Some(&pick(false, &[0, 6], 6)), false, 6, 5),
            pick(false, &[5, 6], 6)
        );
        // A selection in the other list doesn't count; at a list's end nothing changes.
        assert_eq!(
            extend(Some(&pick(true, &[0, 1], 0)), false, 1, 2),
            pick(false, &[1, 2], 1)
        );
        assert_eq!(
            extend(Some(&pick(false, &[3, 4], 3)), false, 4, 4),
            pick(false, &[3, 4], 3)
        );
    }
}

#[cfg(test)]
mod menu_tests {
    use super::{EntryAction, entry_actions, moved_to};
    use gitweft_git::gix::bstr::BString;
    use gitweft_git::read::{ChangeKind, EntryKind, Status, StatusEntry};

    fn entry(
        path: &str,
        index: Option<ChangeKind>,
        worktree: Option<ChangeKind>,
        kind: EntryKind,
    ) -> StatusEntry {
        StatusEntry {
            path: BString::from(path),
            orig_path: None,
            index,
            worktree,
            kind,
            code: *b"  ",
            submodule: false,
        }
    }

    fn modified(path: &str) -> StatusEntry {
        entry(path, None, Some(ChangeKind::Modified), EntryKind::Tracked)
    }

    fn staged(path: &str) -> StatusEntry {
        entry(path, Some(ChangeKind::Modified), None, EntryKind::Tracked)
    }

    fn untracked(path: &str) -> StatusEntry {
        entry(path, None, None, EntryKind::Untracked)
    }

    fn conflicted(path: &str) -> StatusEntry {
        entry(path, None, None, EntryKind::Conflicted)
    }

    #[test]
    fn lfs_files_can_be_locked_where_the_repository_uses_lfs() {
        use super::{FileAction, FileTool, file_actions};
        let labels = |entry: &StatusEntry, lfs: bool| -> Vec<FileTool> {
            file_actions(entry, lfs)
                .into_iter()
                .filter_map(|(action, _, _)| match action {
                    FileAction::External(tool @ (FileTool::LfsLock | FileTool::LfsUnlock)) => {
                        Some(tool)
                    }
                    _ => None,
                })
                .collect()
        };
        assert_eq!(
            labels(&modified("a.bin"), true),
            [FileTool::LfsLock, FileTool::LfsUnlock]
        );
        assert!(labels(&modified("a.bin"), false).is_empty());
        // Not an untracked file (nothing on the server to lock against) or a deleted one.
        assert!(labels(&untracked("new.bin"), true).is_empty());
        let deleted = entry(
            "gone.bin",
            None,
            Some(ChangeKind::Deleted),
            EntryKind::Tracked,
        );
        assert!(labels(&deleted, true).is_empty());
    }

    /// Each first item's action and how many files it takes.
    fn items(
        entry: &StatusEntry,
        staged: bool,
        selection: &[StatusEntry],
    ) -> Vec<(EntryAction, usize)> {
        entry_actions(entry, staged, selection)
            .into_iter()
            .map(|(action, _, entries)| (action, entries.len()))
            .collect()
    }

    #[test]
    fn a_file_menu_starts_with_the_row_links() {
        use EntryAction::*;
        assert_eq!(
            items(&modified("a"), false, &[]),
            [(Stage, 1), (Discard, 1)]
        );
        assert_eq!(
            items(&untracked("n"), false, &[]),
            [(Stage, 1), (Discard, 1)]
        );
        // Mark resolved, and nothing to discard.
        assert_eq!(items(&conflicted("c"), false, &[]), [(Stage, 1)]);
        assert_eq!(items(&staged("s"), true, &[]), [(Unstage, 1)]);
        // A selection of one file is the row itself.
        let one = [modified("a")];
        assert_eq!(items(&one[0], false, &one), [(Stage, 1), (Discard, 1)]);
    }

    #[test]
    fn a_selected_row_offers_the_selection() {
        use EntryAction::*;
        let files = [modified("a"), conflicted("c"), untracked("n")];
        assert_eq!(
            items(&files[0], false, &files),
            [(Stage, 3), (Discard, 2), (Stash, 2)]
        );
        let both = [staged("s1"), staged("s2")];
        assert_eq!(items(&both[1], true, &both), [(Unstage, 2), (Stash, 2)]);
        // Conflicted files only: nothing to discard or stash.
        let conflicts = [conflicted("c1"), conflicted("c2")];
        assert_eq!(items(&conflicts[0], false, &conflicts), [(Stage, 2)]);
    }

    #[test]
    fn a_shown_file_is_followed_to_the_other_list() {
        let status = |entries: Vec<StatusEntry>| Status {
            entries,
            ..Status::default()
        };
        let path = BString::from("a.txt");
        let moved = |status: &Status, staged: bool| {
            moved_to(status, &path, staged).map(|(entry, staged)| (entry.path, staged))
        };
        // Staged whole: its diff goes to the staged side.
        let to_staged = moved(&status(vec![staged("a.txt")]), false);
        assert_eq!(to_staged, Some((path.clone(), true)));
        // Unstaged whole: a new file comes back untracked.
        let to_unstaged = moved(&status(vec![untracked("a.txt")]), true);
        assert_eq!(to_unstaged, Some((path.clone(), false)));
        // Partly staged (in both lists), gone from both, conflicted: it stays.
        let both = entry(
            "a.txt",
            Some(ChangeKind::Modified),
            Some(ChangeKind::Modified),
            EntryKind::Tracked,
        );
        assert_eq!(moved(&status(vec![both.clone()]), false), None);
        assert_eq!(moved(&status(vec![both]), true), None);
        assert_eq!(moved(&status(Vec::new()), false), None);
        assert_eq!(moved(&status(vec![conflicted("a.txt")]), true), None);
        // Another file moving doesn't count.
        let other = status(vec![modified("a.txt"), staged("b.txt")]);
        assert_eq!(moved(&other, false), None);
    }
}
