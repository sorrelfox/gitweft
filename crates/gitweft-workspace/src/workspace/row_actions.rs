//! The sidebar's row menus as data: a row's actions build its context menu and its
//! commands in the palette (`workspace/commands.rs`), so the two can't drift apart.

use gpui_kit::component::menu::PopupMenu;

use super::*;

/// What a row action does.
pub(super) type RowRun = Rc<dyn Fn(&mut Workspace, &mut Window, &mut Context<Workspace>)>;

/// One item of a row's menu.
pub(super) struct RowAction {
    pub label: String,
    /// Greyed out in the menu, left out of the palette.
    pub enabled: bool,
    /// A separator comes before it in the menu.
    pub separated: bool,
    pub run: RowRun,
}

impl RowAction {
    fn new(
        label: impl Into<String>,
        run: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    ) -> Self {
        Self {
            label: label.into(),
            enabled: true,
            separated: false,
            run: Rc::new(run),
        }
    }

    fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    fn separated(mut self) -> Self {
        self.separated = true;
        self
    }
}

/// A context menu of the actions `actions` returns when it opens.
fn actions_menu(
    this: WeakEntity<Workspace>,
    actions: impl Fn() -> Vec<RowAction> + 'static,
) -> impl Fn(PopupMenu, &mut Window, &mut App) -> PopupMenu + 'static {
    move |menu, _, _| {
        actions().into_iter().fold(menu, |menu, action| {
            let menu = if action.separated {
                menu.separator()
            } else {
                menu
            };
            let (this, run) = (this.clone(), action.run);
            menu.item(
                PopupMenuItem::new(action.label)
                    .disabled(!action.enabled)
                    .on_click(move |_, window, cx| {
                        this.update(cx, |this, cx| run(this, window, cx)).ok();
                    }),
            )
        })
    }
}

/// The remote a remote-tracking branch (`origin/main`) belongs to: the longest configured
/// remote name that prefixes it, since names may contain slashes; else its first part.
pub(super) fn remote_of<'a>(configured: &[&'a str], short_name: &'a str) -> &'a str {
    configured
        .iter()
        .copied()
        .filter(|name| {
            short_name
                .strip_prefix(name)
                .is_some_and(|rest| rest.starts_with('/'))
        })
        .max_by_key(|name| name.len())
        .unwrap_or_else(|| short_name.split('/').next().unwrap_or_default())
}

/// A remote-tracking branch's name within its remote.
pub(super) fn branch_in<'a>(remote: &str, short_name: &'a str) -> &'a str {
    short_name
        .strip_prefix(remote)
        .and_then(|rest| rest.strip_prefix('/'))
        .unwrap_or(short_name)
}

/// A local branch: check out, new branch from it, a worktree for it, rename, merge,
/// reflog, compare, delete. What makes no sense for the checked-out one is disabled.
pub(super) fn local_branch_actions(name: &str, full_name: &str, is_head: bool) -> Vec<RowAction> {
    let (switch, rename, delete, worktree) = (
        name.to_owned(),
        name.to_owned(),
        name.to_owned(),
        name.to_owned(),
    );
    let start = (full_name.to_owned(), name.to_owned());
    let (merge_revision, merge_name) = (full_name.to_owned(), name.to_owned());
    let (reflog_ref, reflog_name) = (full_name.to_owned(), name.to_owned());
    let (compare_ref, compare_name) = (full_name.to_owned(), name.to_owned());
    let (other_ref, other_name) = (full_name.to_owned(), name.to_owned());
    vec![
        RowAction::new(t!("branch.checkout"), move |w, window, cx| {
            let op = BranchOp::Switch {
                name: switch.clone(),
            };
            w.run_branch_op(op, window, cx)
        })
        .enabled(!is_head),
        RowAction::new(t!("branch.new"), move |w, window, cx| {
            w.open_new_branch(Some(start.clone()), window, cx)
        }),
        // A branch checked out here can't be checked out in another worktree too.
        RowAction::new(t!("worktree.checkout_in_new"), move |w, window, cx| {
            w.open_branch_worktree(worktree.clone(), window, cx)
        })
        .enabled(!is_head),
        RowAction::new(t!("branch.rename"), move |w, window, cx| {
            w.open_rename_branch(rename.clone(), window, cx)
        }),
        RowAction::new(t!("merge.into_current"), move |w, window, cx| {
            w.merge_into_current(merge_revision.clone(), merge_name.clone(), window, cx)
        })
        .enabled(!is_head),
        RowAction::new(t!("reflog.show"), move |w, _, cx| {
            w.open_reflog(reflog_ref.clone(), reflog_name.clone(), cx)
        }),
        RowAction::new(t!("compare.with_current"), move |w, window, cx| {
            w.open_compare(compare_ref.clone(), compare_name.clone(), window, cx)
        })
        .enabled(!is_head),
        RowAction::new(t!("compare.with_another"), move |w, window, cx| {
            w.choose_compare_base(other_ref.clone(), other_name.clone(), window, cx)
        }),
        RowAction::new(t!("branch.delete"), move |w, window, cx| {
            w.confirm_delete_branch(delete.clone(), window, cx)
        })
        .enabled(!is_head)
        .separated(),
    ]
}

/// A remote-tracking branch: check out (its local branch), new branch, merge, compare.
pub(super) fn remote_branch_actions(remote: &str, branch: &str, full_name: &str) -> Vec<RowAction> {
    let shown = format!("{remote}/{branch}");
    let (remote, branch) = (remote.to_owned(), branch.to_owned());
    let start = (full_name.to_owned(), shown.clone());
    let (merge_revision, merge_name) = (full_name.to_owned(), shown.clone());
    let (compare_ref, compare_name) = (full_name.to_owned(), shown.clone());
    let (other_ref, other_name) = (full_name.to_owned(), shown);
    vec![
        RowAction::new(t!("branch.checkout"), move |w, window, cx| {
            let op = BranchOp::SwitchRemote {
                remote: remote.clone(),
                branch: branch.clone(),
            };
            w.run_branch_op(op, window, cx)
        }),
        RowAction::new(t!("branch.new"), move |w, window, cx| {
            w.open_new_branch(Some(start.clone()), window, cx)
        }),
        RowAction::new(t!("merge.into_current"), move |w, window, cx| {
            w.merge_into_current(merge_revision.clone(), merge_name.clone(), window, cx)
        }),
        RowAction::new(t!("compare.with_current"), move |w, window, cx| {
            w.open_compare(compare_ref.clone(), compare_name.clone(), window, cx)
        }),
        RowAction::new(t!("compare.with_another"), move |w, window, cx| {
            w.choose_compare_base(other_ref.clone(), other_name.clone(), window, cx)
        }),
    ]
}

/// A tag: new branch, check out, compare, publish to or delete from each remote, delete.
pub(super) fn tag_actions(
    full_name: &str,
    name: &str,
    target: ObjectId,
    remotes: &[String],
) -> Vec<RowAction> {
    let start = (full_name.to_owned(), name.to_owned());
    let (compare_ref, compare_name) = (full_name.to_owned(), name.to_owned());
    let (other_ref, other_name) = (full_name.to_owned(), name.to_owned());
    let mut actions = vec![
        RowAction::new(t!("branch.new"), move |w, window, cx| {
            w.open_new_branch(Some(start.clone()), window, cx)
        }),
        RowAction::new(t!("branch.checkout_detached"), move |w, window, cx| {
            w.run_branch_op(BranchOp::Detach { commit: target }, window, cx)
        }),
        RowAction::new(t!("compare.with_current"), move |w, window, cx| {
            w.open_compare(compare_ref.clone(), compare_name.clone(), window, cx)
        }),
        RowAction::new(t!("compare.with_another"), move |w, window, cx| {
            w.choose_compare_base(other_ref.clone(), other_name.clone(), window, cx)
        }),
    ];
    for (ix, remote) in remotes.iter().enumerate() {
        let (push_remote, push_name) = (remote.clone(), name.to_owned());
        let (unpublish_remote, unpublish_name) = (remote.clone(), name.to_owned());
        let push = RowAction::new(t!("tag.push", remote = remote), move |w, window, cx| {
            w.push_tag_to(push_remote.clone(), push_name.clone(), window, cx)
        });
        actions.push(if ix == 0 { push.separated() } else { push });
        actions.push(RowAction::new(
            t!("tag.delete_remote", remote = remote),
            move |w, window, cx| {
                let (remote, name) = (unpublish_remote.clone(), unpublish_name.clone());
                w.delete_tag_on_remote(remote, name, window, cx)
            },
        ));
    }
    let delete = name.to_owned();
    let delete = RowAction::new(t!("tag.delete"), move |w, window, cx| {
        w.confirm_delete_tag(delete.clone(), window, cx)
    });
    actions.push(if remotes.is_empty() {
        delete.separated()
    } else {
        delete
    });
    actions
}

/// Whether a ref is left out of the graph's tips, or is the only one (G8c).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct TipState {
    pub hidden: bool,
    pub solo: bool,
}

/// A branch's or tag's graph items: hide or show it, solo it or stop. The checked-out
/// branch can't be hidden (its commit always stays in the graph).
pub(super) fn tip_actions(full_name: &str, state: TipState, is_head: bool) -> Vec<RowAction> {
    let (hide, solo) = (full_name.to_owned(), full_name.to_owned());
    let hide_label = if state.hidden {
        t!("graph.unhide")
    } else {
        t!("graph.hide")
    };
    let solo_label = if state.solo {
        t!("graph.unsolo")
    } else {
        t!("graph.solo")
    };
    vec![
        RowAction::new(hide_label, move |w, _, cx| {
            w.toggle_hidden(hide.clone(), cx)
        })
        .enabled(!is_head || state.hidden)
        .separated(),
        RowAction::new(solo_label, move |w, _, cx| {
            let solo = (!state.solo).then(|| solo.clone());
            w.set_solo(solo, cx)
        }),
    ]
}

/// A stash: apply, pop, a branch from it, drop.
pub(super) fn stash_actions(entry: &StashEntry) -> Vec<RowAction> {
    let (apply, pop, drop, branch) = (entry.clone(), entry.clone(), entry.clone(), entry.clone());
    vec![
        RowAction::new(t!("stash.apply"), move |w, window, cx| {
            w.apply_stash(apply.clone(), false, window, cx)
        }),
        RowAction::new(t!("stash.pop"), move |w, window, cx| {
            w.apply_stash(pop.clone(), true, window, cx)
        }),
        RowAction::new(t!("stash.branch"), move |w, window, cx| {
            w.open_stash_branch(branch.clone(), window, cx)
        }),
        RowAction::new(t!("stash.drop"), move |w, window, cx| {
            w.confirm_drop_stash(drop.clone(), window, cx)
        })
        .separated(),
    ]
}

/// A submodule (`full_path`: where it lives on disk): open it, update (or set it up),
/// sync its URL, copy its path.
pub(super) fn submodule_actions(info: &SubmoduleInfo, full_path: PathBuf) -> Vec<RowAction> {
    let set_up = info.state != SubmoduleState::Uninitialized;
    let (open, copy) = (full_path.clone(), full_path.clone());
    let (terminal, folder) = (full_path.clone(), full_path);
    // A nested submodule is changed from the repository that holds it.
    let parent = info.superproject.clone();
    let inner = info
        .path
        .strip_prefix(&parent)
        .unwrap_or(&info.path)
        .to_owned();
    let (update, sync) = (
        (parent.clone(), inner.clone()),
        (parent.clone(), inner.clone()),
    );
    let (deinit, remove) = ((parent.clone(), inner.clone()), (parent, inner));
    let update_label = if set_up {
        t!("submodule.update")
    } else {
        t!("submodule.set_up")
    };
    vec![
        RowAction::new(t!("submodule.open_tab"), move |_, _, cx| {
            cx.emit(WorkspaceEvent::OpenInNewTab(open.clone()))
        })
        .enabled(set_up),
        RowAction::new(t!("external.open_terminal"), move |w, window, cx| {
            w.open_in(external::Program::Terminal, terminal.clone(), window, cx)
        })
        .enabled(set_up),
        RowAction::new(t!("external.open_file_manager"), move |w, window, cx| {
            w.open_in(external::Program::FileManager, folder.clone(), window, cx)
        }),
        RowAction::new(update_label, move |w, window, cx| {
            let (parent, path) = update.clone();
            w.run_submodule_update(parent, vec![path], window, cx)
        }),
        RowAction::new(t!("submodule.sync"), move |w, window, cx| {
            let (parent, path) = sync.clone();
            w.run_submodule_sync(parent, path, window, cx)
        }),
        RowAction::new(t!("submodule.copy_path"), move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(copy.display().to_string()))
        }),
        RowAction::new(t!("submodule.deinit_menu"), move |w, window, cx| {
            let (parent, path) = deinit.clone();
            w.confirm_submodule_out(parent, path, false, window, cx)
        })
        .enabled(set_up)
        .separated(),
        RowAction::new(t!("submodule.remove_menu"), move |w, window, cx| {
            let (parent, path) = remove.clone();
            w.confirm_submodule_out(parent, path, true, window, cx)
        }),
    ]
}

/// A worktree: open it, copy its path, lock or unlock it, then prune it when its folder is
/// gone or else remove it. The main worktree, the one this tab shows and a locked one
/// can't be removed from here.
pub(super) fn worktree_actions(info: &WorktreeInfo, is_current: bool) -> Vec<RowAction> {
    let (open, copy, lock, remove) = (
        info.path.clone(),
        info.path.clone(),
        info.path.clone(),
        info.path.clone(),
    );
    let locked = info.locked.is_some();
    let missing = info.prunable.is_some();
    let lock_label = if locked {
        t!("worktree.unlock")
    } else {
        t!("worktree.lock")
    };
    let last = if missing {
        RowAction::new(t!("worktree.prune"), |w, window, cx| {
            w.prune_missing_worktrees(window, cx)
        })
    } else {
        RowAction::new(t!("worktree.remove"), move |w, window, cx| {
            w.confirm_remove_worktree(remove.clone(), window, cx)
        })
        .enabled(!info.is_main && !is_current && !locked)
    };
    let (terminal, folder, moved) = (info.path.clone(), info.path.clone(), info.path.clone());
    vec![
        RowAction::new(t!("worktree.open_tab"), move |_, _, cx| {
            cx.emit(WorkspaceEvent::OpenInNewTab(open.clone()))
        })
        .enabled(!is_current && !missing),
        RowAction::new(t!("external.open_terminal"), move |w, window, cx| {
            w.open_in(external::Program::Terminal, terminal.clone(), window, cx)
        })
        .enabled(!missing),
        RowAction::new(t!("external.open_file_manager"), move |w, window, cx| {
            w.open_in(external::Program::FileManager, folder.clone(), window, cx)
        })
        .enabled(!missing),
        RowAction::new(t!("worktree.copy_path"), move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(copy.display().to_string()))
        }),
        RowAction::new(t!("worktree.move"), move |w, window, cx| {
            w.open_move_worktree(moved.clone(), window, cx)
        })
        .enabled(!info.is_main && !is_current && !locked && !missing),
        RowAction::new(lock_label, move |w, window, cx| {
            w.set_worktree_lock(lock.clone(), !locked, window, cx)
        })
        .enabled(!info.is_main),
        last.separated(),
    ]
}

/// A configured remote: rename, change its URL, copy it, remove; hide its branches in
/// the graph (`hidden`: they are).
pub(super) fn remote_actions(remote: &RemoteInfo, hidden: bool) -> Vec<RowAction> {
    let prefix = remote_prefix(&remote.name);
    let hide_label = if hidden {
        t!("graph.unhide_remote")
    } else {
        t!("graph.hide_remote")
    };
    let (rename, change, url, remove) = (
        remote.name.clone(),
        remote.clone(),
        remote.url.clone(),
        remote.name.clone(),
    );
    let fetch = remote.name.clone();
    vec![
        RowAction::new(t!("remote.fetch_this"), move |w, window, cx| {
            w.fetch_from(Some(fetch.clone()), window, cx)
        }),
        RowAction::new(t!("remote.rename"), move |w, window, cx| {
            w.open_rename_remote(rename.clone(), window, cx)
        }),
        RowAction::new(t!("remote.change_url"), move |w, window, cx| {
            w.open_change_remote_url(change.clone(), window, cx)
        }),
        RowAction::new(t!("remote.copy_url"), move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(url.clone()))
        }),
        RowAction::new(t!("remote.remove"), move |w, window, cx| {
            w.confirm_remove_remote(remove.clone(), window, cx)
        })
        .separated(),
        RowAction::new(hide_label, move |w, _, cx| {
            w.toggle_hidden(prefix.clone(), cx)
        })
        .separated(),
    ]
}

/// What hides every branch of a remote from the graph.
pub(super) fn remote_prefix(name: &str) -> String {
    format!("refs/remotes/{name}/")
}

// The sidebar's menus (`sidebar.rs`), built from the actions above.

pub(super) fn local_branch_menu(
    this: WeakEntity<Workspace>,
    name: String,
    full_name: String,
    is_head: bool,
    tip: TipState,
) -> impl Fn(PopupMenu, &mut Window, &mut App) -> PopupMenu + 'static {
    actions_menu(this, move || {
        let mut actions = local_branch_actions(&name, &full_name, is_head);
        actions.extend(tip_actions(&full_name, tip, is_head));
        actions
    })
}

pub(super) fn remote_branch_menu(
    this: WeakEntity<Workspace>,
    remote: String,
    branch: String,
    full_name: String,
    tip: TipState,
) -> impl Fn(PopupMenu, &mut Window, &mut App) -> PopupMenu + 'static {
    actions_menu(this, move || {
        let mut actions = remote_branch_actions(&remote, &branch, &full_name);
        actions.extend(tip_actions(&full_name, tip, false));
        actions
    })
}

pub(super) fn tag_menu(
    this: WeakEntity<Workspace>,
    full_name: String,
    name: String,
    target: ObjectId,
    remotes: Vec<String>,
    tip: TipState,
) -> impl Fn(PopupMenu, &mut Window, &mut App) -> PopupMenu + 'static {
    actions_menu(this, move || {
        let mut actions = tag_actions(&full_name, &name, target, &remotes);
        actions.extend(tip_actions(&full_name, tip, false));
        actions
    })
}

pub(super) fn stash_menu(
    this: WeakEntity<Workspace>,
    entry: StashEntry,
) -> impl Fn(PopupMenu, &mut Window, &mut App) -> PopupMenu + 'static {
    actions_menu(this, move || stash_actions(&entry))
}

pub(super) fn submodule_menu(
    this: WeakEntity<Workspace>,
    info: SubmoduleInfo,
    full_path: PathBuf,
) -> impl Fn(PopupMenu, &mut Window, &mut App) -> PopupMenu + 'static {
    actions_menu(this, move || submodule_actions(&info, full_path.clone()))
}

pub(super) fn worktree_menu(
    this: WeakEntity<Workspace>,
    info: WorktreeInfo,
    is_current: bool,
) -> impl Fn(PopupMenu, &mut Window, &mut App) -> PopupMenu + 'static {
    actions_menu(this, move || worktree_actions(&info, is_current))
}

pub(super) fn remote_menu(
    this: WeakEntity<Workspace>,
    remote: RemoteInfo,
    hidden: bool,
) -> impl Fn(PopupMenu, &mut Window, &mut App) -> PopupMenu + 'static {
    actions_menu(this, move || remote_actions(&remote, hidden))
}

#[cfg(test)]
mod tests {
    use super::{branch_in, remote_of};

    #[test]
    fn remote_branches_split_at_the_longest_configured_remote() {
        let configured = ["origin", "team/fork", "team"];
        assert_eq!(remote_of(&configured, "origin/main"), "origin");
        assert_eq!(remote_of(&configured, "team/fork/feature/x"), "team/fork");
        assert_eq!(remote_of(&configured, "team/main"), "team");
        // Not configured any more: its first part.
        assert_eq!(remote_of(&configured, "gone/topic"), "gone");
        assert_eq!(branch_in("team/fork", "team/fork/feature/x"), "feature/x");
        assert_eq!(branch_in("origin", "elsewhere"), "elsewhere");
    }
}
