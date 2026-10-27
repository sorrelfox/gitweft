//! Keyboard navigation: F6 / Shift+F6 move the focus between the sidebar, the centre and the
//! right panel; the sidebar's rows have a cursor (arrows, Home/End, Enter), scrolled into
//! view as it moves; Shift+F10 (or the
//! Menu key) opens the actions of the row under the cursor in the palette, for the sidebar,
//! the staging list and the graph's selected commit.

use super::commands::{entry_commands, heading, row_action_commands};
use super::row_actions::*;
use super::sidebar::Section;
use super::*;
use crate::list_keys::{self, Step};
use crate::palette::{self, PaletteCommand};

/// A sidebar row the cursor can be on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SidebarRow {
    /// A local branch (its short name).
    Local(String),
    /// A remote's header row (its name).
    RemoteHeader(String),
    /// A remote-tracking branch (its short name, `origin/main`).
    Remote(String),
    /// A tag (its short name).
    Tag(String),
    /// A stash (its position in the list).
    Stash(usize),
    Submodule(PathBuf),
    Worktree(PathBuf),
}

impl Workspace {
    /// The sidebar's rows in the order they're drawn, with its filter, collapsed remotes and
    /// open lists. `all`: every row, as if nothing were filtered, collapsed or closed (the
    /// palette lists them all).
    pub(super) fn sidebar_rows(&self, all: bool, cx: &App) -> Vec<SidebarRow> {
        let RepoState::Loaded(repo) = &self.state else {
            return Vec::new();
        };
        let refs = &repo.refs;
        let filter = if all {
            String::new()
        } else {
            self.sidebar_filter.read(cx).value().to_lowercase()
        };
        let matches = |name: &str| filter.is_empty() || name.to_lowercase().contains(&filter);
        let expanded = &SettingsStore::settings(cx).layout.sidebar_expanded;
        // A search looks through the tags too, open or not (as the sidebar does).
        let is_open = |section: Section| {
            all || expanded.contains(section.key())
                || (section == Section::Tags && !filter.is_empty())
        };
        let mut rows: Vec<SidebarRow> = refs
            .local_branches
            .iter()
            .filter(|entry| matches(&entry.short_name))
            .map(|entry| SidebarRow::Local(entry.short_name.clone()))
            .collect();
        // The configured remotes in git's order, then those that aren't configured any more.
        let configured: Vec<&str> = self.remotes.iter().map(|r| r.name.as_str()).collect();
        let mut remotes: BTreeMap<&str, Vec<&RefEntry>> = BTreeMap::new();
        for entry in &refs.remote_branches {
            let remote = remote_of(&configured, &entry.short_name);
            remotes.entry(remote).or_default().push(entry);
        }
        let gone: Vec<&str> = remotes
            .keys()
            .copied()
            .filter(|name| !configured.contains(name))
            .collect();
        for remote in configured.iter().copied().chain(gone) {
            let entries = remotes.remove(remote).unwrap_or_default();
            rows.push(SidebarRow::RemoteHeader(remote.to_owned()));
            if !all && self.collapsed_remotes.contains(remote) && filter.is_empty() {
                continue;
            }
            rows.extend(
                entries
                    .into_iter()
                    .filter(|entry| matches(&entry.short_name))
                    .map(|entry| SidebarRow::Remote(entry.short_name.clone())),
            );
        }
        if is_open(Section::Tags) {
            rows.extend(
                refs.tags
                    .iter()
                    .filter(|entry| matches(&entry.short_name))
                    .map(|entry| SidebarRow::Tag(entry.short_name.clone())),
            );
        }
        if is_open(Section::Stashes) {
            rows.extend((0..self.stashes.len()).map(SidebarRow::Stash));
        }
        if !self.submodules.is_empty() && is_open(Section::Submodules) {
            rows.extend(
                self.submodules
                    .iter()
                    .map(|info| SidebarRow::Submodule(info.path.clone())),
            );
        }
        if is_open(Section::Worktrees) {
            rows.extend(
                self.worktrees
                    .iter()
                    .map(|info| SidebarRow::Worktree(info.path.clone())),
            );
        }
        rows
    }

    /// A sidebar row's menu: the palette group it's listed under (a locale key), what it
    /// acts on (the commands' detail), and its actions.
    pub(super) fn sidebar_row_actions(
        &self,
        row: &SidebarRow,
    ) -> Option<(&'static str, String, Vec<RowAction>)> {
        let RepoState::Loaded(repo) = &self.state else {
            return None;
        };
        let refs = &repo.refs;
        Some(match row {
            SidebarRow::Local(name) => {
                let entry = refs.local_branches.iter().find(|e| &e.short_name == name)?;
                let is_head = refs.head.branch_name() == Some(name.as_str());
                let full_name = entry.full_name.to_string();
                let mut actions = local_branch_actions(name, &full_name, is_head);
                actions.extend(tip_actions(&full_name, self.tip_state(&full_name), is_head));
                (
                    "palette.group.branches",
                    t!("palette.detail.branch", name = name).into_owned(),
                    actions,
                )
            }
            SidebarRow::RemoteHeader(name) => {
                let info = self.remotes.iter().find(|remote| &remote.name == name)?;
                (
                    "palette.group.remotes",
                    t!("palette.detail.remote", name = name).into_owned(),
                    remote_actions(info, self.tip_state(&remote_prefix(name)).hidden),
                )
            }
            SidebarRow::Remote(name) => {
                let entry = refs
                    .remote_branches
                    .iter()
                    .find(|e| &e.short_name == name)?;
                let full_name = entry.full_name.to_string();
                let configured: Vec<&str> = self.remotes.iter().map(|r| r.name.as_str()).collect();
                let remote = remote_of(&configured, name);
                let branch = branch_in(remote, name);
                let mut actions = remote_branch_actions(remote, branch, &full_name);
                actions.extend(tip_actions(&full_name, self.tip_state(&full_name), false));
                (
                    "palette.group.remote_branches",
                    t!("palette.detail.remote_branch", name = name).into_owned(),
                    actions,
                )
            }
            SidebarRow::Tag(name) => {
                let entry = refs.tags.iter().find(|e| &e.short_name == name)?;
                let remotes: Vec<String> = self.remotes.iter().map(|r| r.name.clone()).collect();
                let full_name = entry.full_name.to_string();
                let mut actions = tag_actions(&full_name, name, entry.target, &remotes);
                actions.extend(tip_actions(&full_name, self.tip_state(&full_name), false));
                (
                    "palette.group.tags",
                    t!("palette.detail.tag", name = name).into_owned(),
                    actions,
                )
            }
            SidebarRow::Stash(ix) => {
                let entry = self.stashes.get(*ix)?;
                (
                    "palette.group.stashes",
                    t!("palette.detail.stash", name = stash_label(entry)).into_owned(),
                    stash_actions(entry),
                )
            }
            SidebarRow::Submodule(path) => {
                let info = self.submodules.iter().find(|info| &info.path == path)?;
                let root = repo.workdir.clone().unwrap_or_default();
                let name = path.display().to_string();
                (
                    "palette.group.submodules",
                    t!("palette.detail.submodule", name = name).into_owned(),
                    submodule_actions(info, root.join(path)),
                )
            }
            SidebarRow::Worktree(path) => {
                let info = self.worktrees.iter().find(|info| &info.path == path)?;
                let is_current = repo.workdir.as_ref() == Some(path);
                (
                    "palette.group.worktrees",
                    t!("palette.detail.worktree", name = worktree_label(info)).into_owned(),
                    worktree_actions(info, is_current),
                )
            }
        })
    }

    /// The sidebar cursor's place among `rows` (the rows drawn now): see [`cursor_at`].
    pub(super) fn sidebar_cursor_now(&self, rows: &[SidebarRow]) -> Option<usize> {
        cursor_at(rows, self.sidebar_cursor_row.as_ref(), self.sidebar_cursor)
    }

    pub(super) fn move_sidebar_cursor(&mut self, to: Step, cx: &mut Context<Self>) {
        let rows = self.sidebar_rows(false, cx);
        let at = self.sidebar_cursor_now(&rows);
        self.sidebar_cursor = list_keys::step(rows.len(), at, to);
        self.sidebar_cursor_row = self.sidebar_cursor.and_then(|at| rows.get(at).cloned());
        if self.sidebar_cursor == Some(0) {
            // The first row: the LOCAL header above it comes back too.
            self.sidebar_scroll.set_offset(point(px(0.), px(0.)));
        } else {
            self.sidebar_reveal.set(true);
        }
        cx.notify();
    }

    fn sidebar_cursor_row(&self, cx: &App) -> Option<SidebarRow> {
        let rows = self.sidebar_rows(false, cx);
        let at = self.sidebar_cursor_now(&rows)?;
        rows.into_iter().nth(at)
    }

    /// Enter on a sidebar row: a branch or tag is selected in the graph (as a click does), a
    /// remote's header opens or closes, a stash shows in the details, a submodule or worktree
    /// opens in a tab (its menu's first item).
    pub(super) fn open_sidebar_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.sidebar_cursor_row(cx) else {
            return;
        };
        let target = match &row {
            SidebarRow::RemoteHeader(name) => {
                if !self.collapsed_remotes.remove(name) {
                    self.collapsed_remotes.insert(name.clone());
                }
                cx.notify();
                return;
            }
            SidebarRow::Stash(ix) => {
                if let Some(entry) = self.stashes.get(*ix).cloned() {
                    self.show_stash(entry, cx);
                }
                return;
            }
            SidebarRow::Submodule(_) | SidebarRow::Worktree(_) => {
                let first = self
                    .sidebar_row_actions(&row)
                    .and_then(|(_, _, actions)| actions.into_iter().next())
                    .filter(|action| action.enabled);
                if let Some(action) = first {
                    (action.run)(self, window, cx);
                }
                return;
            }
            SidebarRow::Local(_) | SidebarRow::Remote(_) | SidebarRow::Tag(_) => {
                let RepoState::Loaded(repo) = &self.state else {
                    return;
                };
                let refs = &repo.refs;
                match &row {
                    SidebarRow::Local(name) => refs
                        .local_branches
                        .iter()
                        .find(|e| &e.short_name == name)
                        .map(|e| e.target),
                    SidebarRow::Remote(name) => refs
                        .remote_branches
                        .iter()
                        .find(|e| &e.short_name == name)
                        .map(|e| e.target),
                    SidebarRow::Tag(name) => refs
                        .tags
                        .iter()
                        .find(|e| &e.short_name == name)
                        .map(|e| e.target),
                    _ => None,
                }
            }
        };
        if let (Some(target), Some(graph)) = (target, self.graph_view.clone()) {
            graph.update(cx, |view, cx| view.reveal(target, cx));
        }
    }

    /// Shift+F10 on a sidebar row: its menu's actions in the palette.
    pub(super) fn sidebar_row_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.sidebar_cursor_row(cx) else {
            return;
        };
        let Some((group, detail, actions)) = self.sidebar_row_actions(&row) else {
            return;
        };
        let this = cx.entity().downgrade();
        let commands = row_action_commands(&this, &heading(group), &detail, actions);
        if !commands.is_empty() {
            palette::open(commands, window, cx);
        }
    }

    /// Shift+F10 on the graph: the selected commit's row menu in the palette.
    pub(super) fn open_commit_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let head = match &repo.refs.head {
            HeadInfo::Branch { oid, .. } | HeadInfo::Detached { oid } => Some(*oid),
            HeadInfo::Unborn { .. } => None,
        };
        let this = cx.entity().downgrade();
        let mut commands = Vec::new();
        self.commit_commands(&this, head, &mut commands, cx);
        if !commands.is_empty() {
            palette::open(commands, window, cx);
        }
    }

    /// Shift+F10 in the staging list: the file's row menu in the palette (a multi-file
    /// selection's actions first, when the file is part of one).
    pub(super) fn open_file_menu(
        &mut self,
        entry: StatusEntry,
        staged: bool,
        selection: Vec<StatusEntry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity().downgrade();
        let mut commands = Vec::new();
        entry_commands(
            &this,
            &heading("palette.group.files"),
            &entry,
            staged,
            &selection,
            true,
            &mut commands,
            self.sync.lfs_locking(),
        );
        if !commands.is_empty() {
            palette::open(commands, window, cx);
        }
    }

    /// Shift+F10 in the details' files: that file's row menu (blame, history) in the palette.
    pub(super) fn open_details_file_menu(
        &mut self,
        commit: ObjectId,
        parent: Option<ObjectId>,
        change: gitweft_git::read::FileChange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity().downgrade();
        let group = heading("palette.group.files");
        let details = self.details_view.clone();
        let path = change.path.to_string();
        let items = crate::details_view::file_menu_items(commit, parent, &change);
        let commands: Vec<PaletteCommand> = items
            .into_iter()
            .map(|(label, event)| {
                let (this, details) = (this.clone(), details.clone());
                PaletteCommand::new(group.clone(), label, move |_, cx| {
                    this.update(cx, |w, cx| w.on_details_event(details.clone(), &event, cx))
                        .ok();
                })
                .detail(path.clone())
            })
            .collect();
        if !commands.is_empty() {
            palette::open(commands, window, cx);
        }
    }

    /// The view shown in the centre: the conflict tool, an open view, else the graph.
    fn centre_focus(&self, cx: &App) -> Option<FocusHandle> {
        if let Some(view) = &self.conflict_view {
            return Some(view.read(cx).focus_handle(cx));
        }
        macro_rules! top_focus {
            ($this:expr; $($field:ident $close:ident),*) => {
                $(if let Some(view) = &$this.$field {
                    return Some(view.read(cx).focus_handle(cx));
                })*
            };
        }
        centre_views!(top_focus!(self));
        self.graph_view
            .as_ref()
            .map(|v| v.read(cx).focus_handle(cx))
    }

    /// The right panel: the staging list while the working changes are shown, else the
    /// details' files.
    fn right_focus(&self, cx: &App) -> FocusHandle {
        if self.showing_wip {
            self.staging_view.read(cx).focus_handle(cx)
        } else {
            self.details_view.read(cx).focus_handle(cx)
        }
    }

    /// F6 (Shift+F6): the focus goes to the next (previous) of the sidebar, the centre and
    /// the right panel. The sidebar's cursor starts on the checked-out branch.
    pub(super) fn cycle_focus(
        &mut self,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !matches!(self.state, RepoState::Loaded(_)) {
            return;
        }
        let panels: Vec<FocusHandle> = [
            Some(self.sidebar_focus.clone()),
            self.centre_focus(cx),
            Some(self.right_focus(cx)),
        ]
        .into_iter()
        .flatten()
        .collect();
        let len = panels.len();
        let next = match panels
            .iter()
            .position(|panel| panel.contains_focused(window, cx))
        {
            Some(at) if forward => (at + 1) % len,
            Some(at) => (at + len - 1) % len,
            None => 0,
        };
        if next == 0 && self.sidebar_cursor.is_none() {
            let head = match &self.state {
                RepoState::Loaded(repo) => repo.refs.head.branch_name().map(str::to_owned),
                _ => None,
            };
            let rows = self.sidebar_rows(false, cx);
            self.sidebar_cursor = head
                .and_then(|name| {
                    rows.iter()
                        .position(|row| *row == SidebarRow::Local(name.clone()))
                })
                .or((!rows.is_empty()).then_some(0));
            self.sidebar_cursor_row = self.sidebar_cursor.and_then(|at| rows.get(at).cloned());
            self.sidebar_reveal.set(true);
        }
        panels[next].focus(window, cx);
        cx.notify();
    }
}

/// Where the cursor is among `rows` now: on the row it was on (`row`, while that's still
/// listed), else at the same place (`at`, kept inside the list). So a reload, a new branch or
/// a filter that moves the rows leaves the cursor on its branch, not on whatever row took
/// its place.
fn cursor_at(rows: &[SidebarRow], row: Option<&SidebarRow>, at: Option<usize>) -> Option<usize> {
    if rows.is_empty() {
        return None;
    }
    row.and_then(|row| rows.iter().position(|other| other == row))
        .or(at.map(|at| at.min(rows.len() - 1)))
}

#[cfg(test)]
mod tests {
    // By name: the parent's glob brings gpui-kit's `test` attribute, which `#[test]` would
    // then expand into recursively.
    use super::{SidebarRow, cursor_at};

    fn local(names: &[&str]) -> Vec<SidebarRow> {
        names
            .iter()
            .map(|name| SidebarRow::Local(name.to_string()))
            .collect()
    }

    #[test]
    fn the_cursor_stays_on_its_row_when_the_rows_move() {
        let before = local(&["b", "c", "d"]);
        let on_c = before.get(1).cloned();
        // A branch sorted in front of it: the same row, one place down.
        let after = local(&["a", "b", "c", "d"]);
        assert_eq!(cursor_at(&after, on_c.as_ref(), Some(1)), Some(2));
        // Its row gone (deleted, or filtered out): the same place, kept inside the list.
        assert_eq!(
            cursor_at(&local(&["b", "d"]), on_c.as_ref(), Some(1)),
            Some(1)
        );
        assert_eq!(cursor_at(&local(&["b"]), on_c.as_ref(), Some(2)), Some(0));
        // No row known yet: the place alone; nothing at all: none.
        assert_eq!(cursor_at(&after, None, Some(3)), Some(3));
        assert_eq!(cursor_at(&after, None, None), None);
        assert_eq!(cursor_at(&[], on_c.as_ref(), Some(1)), None);
    }
}
