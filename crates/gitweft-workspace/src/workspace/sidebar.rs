//! The sidebar (Graphite): a filter; the local branches; the remotes with their branches;
//! and a bottom block whose rows (tags, stashes, submodules, worktrees) open their lists
//! in the scrolling part above it. Which lists are open is a setting, so it survives a
//! restart. Every row keeps the menu it had.

use gitweft_ui::current::mono_font;
use gpui_kit::component::menu::{ContextMenuExt, PopupMenu};

use super::keyboard::SidebarRow;
use super::row_actions::*;
use super::*;
use crate::list_keys::{self, Step};

/// The lists the bottom block opens; the key is kept in `settings.layout.sidebar_expanded`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Section {
    Tags,
    Stashes,
    Submodules,
    Worktrees,
}

impl Section {
    pub(super) fn key(self) -> &'static str {
        match self {
            Section::Tags => "tags",
            Section::Stashes => "stashes",
            Section::Submodules => "submodules",
            Section::Worktrees => "worktrees",
        }
    }

    fn icon(self) -> AppIcon {
        match self {
            Section::Tags => AppIcon::Tag,
            Section::Stashes => AppIcon::Stash,
            Section::Submodules => AppIcon::Box,
            Section::Worktrees => AppIcon::Folder,
        }
    }

    fn title(self) -> String {
        match self {
            Section::Tags => t!("sidebar.section_tags"),
            Section::Stashes => t!("sidebar.section_stashes"),
            Section::Submodules => t!("sidebar.section_submodules"),
            Section::Worktrees => t!("sidebar.section_worktrees"),
        }
        .into_owned()
    }
}

/// Our menu builders take an `App`; the kit's context menu hands them a
/// `Context<PopupMenu>`.
fn menu(
    build: impl Fn(PopupMenu, &mut Window, &mut App) -> PopupMenu + 'static,
) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
    move |menu, window, cx| build(menu, window, cx)
}

impl Workspace {
    pub(super) fn render_sidebar(&self, repo: &LoadedRepo, cx: &mut Context<Self>) -> AnyElement {
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);
        let refs = &repo.refs;
        let head_name = refs.head.branch_name();
        let this = cx.entity().downgrade();
        let graph = self.graph_view.clone();
        let filter = self.sidebar_filter.read(cx).value().to_lowercase();
        let matches = |name: &str| filter.is_empty() || name.to_lowercase().contains(&filter);
        let expanded = SettingsStore::settings(cx).layout.sidebar_expanded.clone();
        // A search looks through the tags too, open or not.
        let is_open = |section: Section| {
            expanded.contains(section.key()) || (section == Section::Tags && !filter.is_empty())
        };
        let reveal = |target: ObjectId| {
            let graph = graph.clone();
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                if let Some(graph) = &graph {
                    graph.update(cx, |view, cx| view.reveal(target, cx));
                }
            }
        };
        let mut lists: Vec<AnyElement> = Vec::new();
        // The keyboard's cursor, while the lists have the focus: an accent bar on its row.
        let cursor = if self.sidebar_focused {
            // On the row it was on, even when a reload or the filter moved it.
            let rows = self.sidebar_rows(false, cx);
            self.sidebar_cursor_now(&rows)
                .and_then(|at| rows.into_iter().nth(at))
        } else {
            None
        };
        let marked = |row: SidebarRow| cursor.as_ref() == Some(&row);
        // The rows sit in sections, so the cursor's row brings itself into view.
        let cursor_mark = || {
            list_keys::revealing_cursor_bar(p.acc, &m, &self.sidebar_scroll, &self.sidebar_reveal)
        };

        // Local branches.
        lists.push(
            section_header(
                t!("sidebar.section_local").into_owned(),
                refs.local_branches.len(),
                &p,
                &m,
                m.dp(10.),
            )
            .into_any_element(),
        );
        let locals = refs
            .local_branches
            .iter()
            .filter(|entry| matches(&entry.short_name));
        for (ix, entry) in locals.enumerate() {
            let is_head = Some(entry.short_name.as_str()) == head_name;
            let badge = self
                .tracking
                .get(&branch_key(&entry.full_name))
                .and_then(|tracking| tracking_badge(tracking, p.fg2, p.modified));
            let full_name = entry.full_name.to_string();
            let tip = self.tip_state(&full_name);
            let row = row(("local-branch", ix), m.sidebar_row, &p, &m, None)
                .text_color(if tip.hidden { p.fg3 } else { p.fg })
                .when(is_head, |row| row.bg(p.sel).font_weight(FontWeight::MEDIUM))
                .child(head_marker(is_head, p.lanes[self.head_lane(cx)], &m))
                .child(truncated("name", entry.short_name.clone()).flex_1())
                .children(tip_mark(tip, &p, &m))
                .when_some(badge, |row, (text, color)| {
                    row.child(
                        div()
                            .flex_shrink_0()
                            .font_family(mono_font())
                            .text_size(m.text.mono_small)
                            .text_color(color)
                            .child(text),
                    )
                })
                .when(marked(SidebarRow::Local(entry.short_name.clone())), |row| {
                    row.relative().child(cursor_mark())
                })
                .on_click(reveal(entry.target))
                .context_menu(menu(local_branch_menu(
                    this.clone(),
                    entry.short_name.clone(),
                    full_name,
                    is_head,
                    tip,
                )));
            lists.push(row.into_any_element());
        }

        // Remote branches grouped per remote ("origin" → "main", "feat/x", ...). Remote
        // names may contain slashes, so the longest configured name that fits wins.
        let configured: Vec<&str> = self.remotes.iter().map(|r| r.name.as_str()).collect();
        let mut remotes: BTreeMap<&str, Vec<&RefEntry>> = BTreeMap::new();
        for entry in &refs.remote_branches {
            let remote = remote_of(&configured, &entry.short_name);
            remotes.entry(remote).or_default().push(entry);
        }
        // The configured remotes in git's order (also those with nothing fetched yet), then
        // the refs of any remote that isn't configured any more.
        let gone: Vec<&str> = remotes
            .keys()
            .copied()
            .filter(|name| !configured.contains(name))
            .collect();
        let groups: Vec<(&str, Option<RemoteInfo>)> = self
            .remotes
            .iter()
            .map(|remote| (remote.name.as_str(), Some(remote.clone())))
            .chain(gone.into_iter().map(|name| (name, None)))
            .collect();
        lists.push(
            section_header(
                t!("sidebar.section_remote").into_owned(),
                refs.remote_branches.len(),
                &p,
                &m,
                m.dp(14.),
            )
            .into_any_element(),
        );
        for (group_ix, (remote, info)) in groups.into_iter().enumerate() {
            let entries = remotes.remove(remote).unwrap_or_default();
            let collapsed = self.collapsed_remotes.contains(remote);
            let name = remote.to_owned();
            let chevron = if collapsed {
                AppIcon::ChevronRight
            } else {
                AppIcon::ChevronDown
            };
            let remote_tip = self.tip_state(&remote_prefix(remote));
            let header = row(("remote", group_ix), m.sidebar_row, &p, &m, None)
                .text_color(if remote_tip.hidden { p.fg3 } else { p.fg })
                .child(Icon::new(chevron).text_color(p.fg3))
                .child(truncated("name", name.clone()).flex_1())
                .children(tip_mark(remote_tip, &p, &m))
                .when(marked(SidebarRow::RemoteHeader(remote.to_owned())), |row| {
                    row.relative().child(cursor_mark())
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    if !this.collapsed_remotes.remove(&name) {
                        this.collapsed_remotes.insert(name.clone());
                    }
                    cx.notify();
                }));
            lists.push(match info {
                Some(info) => header
                    .context_menu(menu(remote_menu(this.clone(), info, remote_tip.hidden)))
                    .into_any_element(),
                None => header.into_any_element(),
            });
            // A search shows the matches of a collapsed remote too.
            if collapsed && filter.is_empty() {
                continue;
            }
            let entries = entries
                .into_iter()
                .filter(|entry| matches(&entry.short_name));
            for (ix, entry) in entries.enumerate() {
                let branch = branch_in(remote, &entry.short_name);
                let full_name = entry.full_name.to_string();
                let tip = self.tip_state(&full_name);
                let row = row(
                    ("remote-branch", (group_ix << 20) | ix),
                    m.file_row,
                    &p,
                    &m,
                    Some(p.fg),
                )
                .pl(m.dp(28.))
                .text_size(m.text.small)
                .text_color(if tip.hidden { p.fg3 } else { p.fg2 })
                .child(truncated("name", branch.to_owned()).flex_1())
                .children(tip_mark(tip, &p, &m))
                .when(
                    marked(SidebarRow::Remote(entry.short_name.clone())),
                    |row| row.relative().child(cursor_mark()),
                )
                .on_click(reveal(entry.target))
                .context_menu(menu(remote_branch_menu(
                    this.clone(),
                    remote.to_owned(),
                    branch.to_owned(),
                    full_name,
                    tip,
                )));
                lists.push(row.into_any_element());
            }
        }
        lists.push({
            let this = this.clone();
            action_row("add-remote", t!("remote.add_item").into_owned(), &p, &m)
                .on_click(move |_, window, cx| {
                    this.update(cx, |this, cx| this.open_add_remote(window, cx))
                        .ok();
                })
                .into_any_element()
        });

        if is_open(Section::Tags) {
            lists.push(
                section_header(Section::Tags.title(), refs.tags.len(), &p, &m, m.dp(14.))
                    .into_any_element(),
            );
            // Tags can be published to any configured remote.
            let tag_remotes: Vec<String> = self.remotes.iter().map(|r| r.name.clone()).collect();
            let tags = refs.tags.iter().filter(|entry| matches(&entry.short_name));
            for (ix, entry) in tags.enumerate() {
                let full_name = entry.full_name.to_string();
                let tip = self.tip_state(&full_name);
                let row = row(("tag", ix), m.file_row, &p, &m, Some(p.fg))
                    .text_size(m.text.small)
                    .text_color(if tip.hidden { p.fg3 } else { p.fg2 })
                    .child(Icon::new(AppIcon::Tag).text_color(p.fg3))
                    .child(truncated("name", entry.short_name.clone()).flex_1())
                    .children(tip_mark(tip, &p, &m))
                    .when(marked(SidebarRow::Tag(entry.short_name.clone())), |row| {
                        row.relative().child(cursor_mark())
                    })
                    .on_click(reveal(entry.target))
                    .context_menu(menu(tag_menu(
                        this.clone(),
                        full_name,
                        entry.short_name.clone(),
                        entry.target,
                        tag_remotes.clone(),
                        tip,
                    )));
                lists.push(row.into_any_element());
            }
        }

        if is_open(Section::Stashes) {
            lists.push(
                section_header(
                    Section::Stashes.title(),
                    self.stashes.len(),
                    &p,
                    &m,
                    m.dp(14.),
                )
                .into_any_element(),
            );
            for (ix, entry) in self.stashes.iter().enumerate() {
                let (show, stash) = (this.clone(), entry.clone());
                let row = row(("stash", ix), m.file_row, &p, &m, Some(p.fg))
                    .text_size(m.text.small)
                    .text_color(p.fg2)
                    .child(Icon::new(AppIcon::Stash).text_color(p.fg3))
                    .child(truncated("name", stash_label(entry)).flex_1())
                    .on_click(move |_, _, cx| {
                        show.update(cx, |this, cx| this.show_stash(stash.clone(), cx))
                            .ok();
                    })
                    .when(marked(SidebarRow::Stash(ix)), |row| {
                        row.relative().child(cursor_mark())
                    })
                    .context_menu(menu(stash_menu(this.clone(), entry.clone())));
                lists.push(row.into_any_element());
            }
        }

        // Submodules only show up in a repository that has some.
        if !self.submodules.is_empty() && is_open(Section::Submodules) {
            lists.push(
                section_header(
                    Section::Submodules.title(),
                    self.submodules.len(),
                    &p,
                    &m,
                    m.dp(14.),
                )
                .into_any_element(),
            );
            let root = repo.workdir.clone().unwrap_or_default();
            for (ix, info) in self.submodules.iter().enumerate() {
                // The path alone fits the sidebar; the suffix says what's off, or which
                // commit it's at when all is well.
                let status = match info.state {
                    SubmoduleState::UpToDate => {
                        info.checked_out.map(|id| id.to_hex_with_len(7).to_string())
                    }
                    SubmoduleState::Changed => Some(t!("submodule.changed").into_owned()),
                    SubmoduleState::Uninitialized => {
                        Some(t!("submodule.uninitialized").into_owned())
                    }
                    SubmoduleState::Conflict => Some(t!("submodule.conflict").into_owned()),
                };
                // A nested submodule sits under the one holding it, indented, named by
                // its path inside it.
                let shown = info
                    .path
                    .strip_prefix(&info.superproject)
                    .unwrap_or(&info.path);
                let row = row(("submodule", ix), m.file_row, &p, &m, Some(p.fg))
                    .text_size(m.text.small)
                    .text_color(p.fg2)
                    .when(info.depth > 0, |row| {
                        row.pl(m.dp(8. + 14. * info.depth as f32))
                    })
                    .child(truncated("name", shown.display().to_string()).flex_1())
                    .children(status.map(|status| {
                        div()
                            .flex_shrink_0()
                            .font_family(mono_font())
                            .text_size(m.text.mono_small)
                            .text_color(p.fg3)
                            .child(status)
                    }))
                    .when(marked(SidebarRow::Submodule(info.path.clone())), |row| {
                        row.relative().child(cursor_mark())
                    })
                    .context_menu(menu(submodule_menu(
                        this.clone(),
                        info.clone(),
                        root.join(&info.path),
                    )));
                lists.push(row.into_any_element());
            }
            lists.push({
                let this = this.clone();
                action_row(
                    "update-submodules",
                    t!("submodule.update_all").into_owned(),
                    &p,
                    &m,
                )
                .on_click(move |_, window, cx| {
                    this.update(cx, |this, cx| {
                        this.run_submodule_update(PathBuf::new(), Vec::new(), window, cx)
                    })
                    .ok();
                })
                .into_any_element()
            });
            lists.push({
                let this = this.clone();
                action_row(
                    "add-submodule",
                    t!("submodule.add_menu").into_owned(),
                    &p,
                    &m,
                )
                .on_click(move |_, window, cx| {
                    this.update(cx, |this, cx| this.open_add_submodule(window, cx))
                        .ok();
                })
                .into_any_element()
            });
        }

        if is_open(Section::Worktrees) {
            lists.push(
                section_header(
                    Section::Worktrees.title(),
                    self.worktrees.len(),
                    &p,
                    &m,
                    m.dp(14.),
                )
                .into_any_element(),
            );
            let current = repo.workdir.clone();
            for (ix, info) in self.worktrees.iter().enumerate() {
                let is_current = current.as_ref() == Some(&info.path);
                let status = if info.prunable.is_some() {
                    Some(t!("worktree.missing").into_owned())
                } else {
                    info.locked.as_ref().map(|reason| {
                        if reason.is_empty() {
                            t!("worktree.locked").into_owned()
                        } else {
                            t!("worktree.locked_because", reason = reason).into_owned()
                        }
                    })
                };
                let row = row(("worktree", ix), m.file_row, &p, &m, Some(p.fg))
                    .text_size(m.text.small)
                    .text_color(if is_current { p.fg } else { p.fg2 })
                    .when(is_current, |row| row.bg(p.sel))
                    .child(truncated("name", worktree_label(info)).flex_1())
                    .children(status.map(|status| {
                        div()
                            .flex_shrink_0()
                            .text_size(m.text.label)
                            .text_color(p.fg3)
                            .child(status)
                    }))
                    .when(marked(SidebarRow::Worktree(info.path.clone())), |row| {
                        row.relative().child(cursor_mark())
                    })
                    .context_menu(menu(worktree_menu(this.clone(), info.clone(), is_current)));
                lists.push(row.into_any_element());
            }
            lists.push({
                let this = this.clone();
                action_row("add-worktree", t!("worktree.add_item").into_owned(), &p, &m)
                    .on_click(move |_, window, cx| {
                        this.update(cx, |this, cx| this.open_add_worktree(None, window, cx))
                            .ok();
                    })
                    .into_any_element()
            });
            lists.push({
                let this = this.clone();
                let label = t!("worktree.add_in_folder_item").into_owned();
                action_row("add-worktree-in-folder", label, &p, &m)
                    .on_click(move |_, window, cx| {
                        this.update(cx, |this, cx| this.pick_worktree_folder(window, cx))
                            .ok();
                    })
                    .into_any_element()
            });
        }

        // The bottom block: one row per list, which opens or closes it above.
        let mut sections = vec![
            (Section::Tags, refs.tags.len()),
            (Section::Stashes, self.stashes.len()),
        ];
        if !self.submodules.is_empty() {
            sections.push((Section::Submodules, self.submodules.len()));
        }
        sections.push((Section::Worktrees, self.worktrees.len()));
        let bottom = sections.into_iter().map(|(section, count)| {
            let open = expanded.contains(section.key());
            let key = section.key();
            row(
                ("sidebar-section", section as usize),
                m.file_row,
                &p,
                &m,
                Some(p.fg),
            )
            .text_size(m.text.small)
            .text_color(if open { p.fg } else { p.fg2 })
            .child(Icon::new(section.icon()).text_color(if open { p.acc } else { p.fg3 }))
            .child(div().flex_1().child(section.title()))
            .child(
                div()
                    .font_family(mono_font())
                    .text_size(m.text.mono_small)
                    .text_color(p.fg3)
                    .child(count.to_string()),
            )
            .on_click(move |_, _, cx| {
                SettingsStore::update(cx, |settings| {
                    let open = &mut settings.layout.sidebar_expanded;
                    if !open.remove(key) {
                        open.insert(key.to_owned());
                    }
                });
            })
            .into_any_element()
        });

        v_flex()
            .size_full()
            .bg(p.panel)
            .child(
                div().px(m.dp(10.)).pt(m.dp(10.)).pb(m.dp(6.)).child(
                    Input::new(&self.sidebar_filter)
                        .small()
                        .prefix(Icon::new(AppIcon::Filter).text_color(p.fg3))
                        .cleanable(true),
                ),
            )
            .child(
                v_flex()
                    .id("sidebar-lists")
                    // The list keys work here, not in the filter above (it needs Space).
                    .key_context(list_keys::SIDEBAR)
                    .track_focus(&self.sidebar_focus)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| this.sidebar_focus.focus(window, cx)),
                    )
                    .on_action(cx.listener(|this, _: &list_keys::NextRow, _, cx| {
                        this.move_sidebar_cursor(Step::Next, cx)
                    }))
                    .on_action(cx.listener(|this, _: &list_keys::PreviousRow, _, cx| {
                        this.move_sidebar_cursor(Step::Previous, cx)
                    }))
                    .on_action(cx.listener(|this, _: &list_keys::FirstRow, _, cx| {
                        this.move_sidebar_cursor(Step::First, cx)
                    }))
                    .on_action(cx.listener(|this, _: &list_keys::LastRow, _, cx| {
                        this.move_sidebar_cursor(Step::Last, cx)
                    }))
                    .on_action(cx.listener(|this, _: &list_keys::OpenRow, window, cx| {
                        this.open_sidebar_row(window, cx)
                    }))
                    .on_action(cx.listener(|this, _: &list_keys::RowMenu, window, cx| {
                        this.sidebar_row_menu(window, cx)
                    }))
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.sidebar_scroll)
                    .px(m.dp(6.))
                    .pb(m.dp(10.))
                    .children(lists),
            )
            .child(
                v_flex()
                    .flex_shrink_0()
                    .p(m.dp(6.))
                    .border_t_1()
                    .border_color(p.line)
                    .children(bottom),
            )
            .into_any_element()
    }
}

/// A sidebar row: rounded, the hover tint (and a stronger text color, when given).
fn row(
    id: impl Into<ElementId>,
    height: Pixels,
    p: &Palette,
    m: &Metrics,
    hover_fg: Option<Hsla>,
) -> Stateful<Div> {
    let hover = p.hover;
    h_flex()
        .id(id)
        .h(height)
        .flex_shrink_0()
        .gap(m.dp(8.))
        .px(m.dp(8.))
        .rounded(m.dp(6.))
        .cursor_pointer()
        .hover(move |style| {
            let style = style.bg(hover);
            match hover_fg {
                Some(color) => style.text_color(color),
                None => style,
            }
        })
}

/// "LOCAL 3": an uppercase label with a mono count on the right.
fn section_header(label: String, count: usize, p: &Palette, m: &Metrics, top: Pixels) -> Div {
    h_flex()
        .flex_shrink_0()
        .justify_between()
        .px(m.dp(8.))
        .pt(top)
        .pb(m.dp(4.))
        .text_size(m.text.label)
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(p.fg3)
        .child(label.to_uppercase())
        .child(
            div()
                .font_family(mono_font())
                .font_weight(FontWeight::NORMAL)
                .child(count.to_string()),
        )
}

/// "Add remote…" and the like: a quiet row at the end of a list.
fn action_row(id: &'static str, label: String, p: &Palette, m: &Metrics) -> Stateful<Div> {
    row(id, m.file_row, p, m, Some(p.fg2))
        .text_size(m.text.small)
        .text_color(p.fg3)
        .child(label)
}

/// "hidden" (fg3) or "solo" (accent) after a row whose ref the graph leaves out or walks alone.
fn tip_mark(tip: TipState, p: &Palette, m: &Metrics) -> Option<Div> {
    let (text, color) = if tip.solo {
        (t!("sidebar.solo"), p.acc)
    } else if tip.hidden {
        (t!("sidebar.hidden"), p.fg3)
    } else {
        return None;
    };
    Some(
        div()
            .flex_shrink_0()
            .text_size(m.text.label)
            .text_color(color)
            .child(text.into_owned()),
    )
}

/// The checked-out branch's dot with a ring, in its lane color; other branches get the same
/// space, so the names line up.
fn head_marker(is_head: bool, color: Hsla, m: &Metrics) -> Div {
    let marker = div().size(m.dp(12.)).flex_shrink_0();
    if is_head {
        marker
            .rounded_full()
            .border_1()
            .border_color(color)
            .flex()
            .items_center()
            .justify_center()
            .child(div().size(m.dp(6.)).rounded_full().bg(color))
    } else {
        marker
    }
}
