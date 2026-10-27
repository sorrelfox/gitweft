//! The tab's commands for the command palette (`crate::palette`): the operation banner's,
//! the selected commit's (the graph's row menu), the working changes', the toolbar's and
//! the sidebar's, plus what the shell needs to open repositories from the palette.

use std::borrow::Cow;

use super::*;
use crate::graph_view::{self, FocusSearch, GraphEvent};
use crate::palette::PaletteCommand;
use crate::staging_view::{entry_actions, file_actions};
use gitweft_git::lfs::MigrateDirection;

use super::row_actions::*;

/// Changed files beyond this many get no commands of their own (a huge untracked folder
/// would make the palette slow to open).
const FILE_LIMIT: usize = 500;

/// A palette command that calls a method of the workspace.
pub(super) fn command(
    this: &WeakEntity<Workspace>,
    group: &SharedString,
    label: impl Into<String>,
    action: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) -> PaletteCommand {
    let this = this.clone();
    PaletteCommand::new(group.clone(), label, move |window, cx| {
        this.update(cx, |this, cx| action(this, window, cx)).ok();
    })
}

pub(super) fn heading(key: &str) -> SharedString {
    t!(key).into_owned().into()
}

/// A sidebar row's enabled actions as palette commands, with what they act on as the detail.
pub(super) fn row_action_commands(
    this: &WeakEntity<Workspace>,
    group: &SharedString,
    detail: &str,
    actions: Vec<RowAction>,
) -> Vec<PaletteCommand> {
    actions
        .into_iter()
        .filter(|action| action.enabled)
        .map(|action| {
            let run = action.run;
            command(this, group, action.label, move |w, window, cx| {
                run(w, window, cx)
            })
            .detail(detail)
        })
        .collect()
}

/// A changed file's commands, as its row's menu has them: its links (Stage or Mark resolved
/// and Discard changes…, or Unstage), or the actions of the multi-file `selection` it's part
/// of; then, with `menu`, the file's menu items.
#[allow(clippy::too_many_arguments)]
pub(super) fn entry_commands(
    this: &WeakEntity<Workspace>,
    group: &SharedString,
    entry: &StatusEntry,
    staged: bool,
    selection: &[StatusEntry],
    menu: bool,
    commands: &mut Vec<PaletteCommand>,
    lfs: bool,
) {
    let detail = entry.display_path();
    for (action, label, entries) in entry_actions(entry, staged, selection) {
        let command = command(this, group, label, move |w, window, cx| {
            let entries = entries.clone();
            w.staging_view.update(cx, |staging, cx| {
                staging.run_entry_action(action, entries, window, cx)
            })
        });
        // A selection's commands act on its files, not on this one alone.
        commands.push(if selection.len() > 1 {
            command
        } else {
            command.detail(detail.clone())
        });
    }
    if menu {
        for (action, label, _) in file_actions(entry, lfs) {
            let entry = entry.clone();
            commands.push(
                command(this, group, label, move |w, _, cx| {
                    let event = action.event(&entry);
                    w.on_staging_event(w.staging_view.clone(), &event, cx)
                })
                .detail(detail.clone()),
            );
        }
    }
}

impl Workspace {
    /// The recently opened repositories, newest first.
    pub(crate) fn recent_repositories(&self) -> &[PathBuf] {
        &self.recent
    }

    /// Whether the tab shows the start screen (nothing open, or opening failed).
    pub(crate) fn is_start_screen(&self) -> bool {
        matches!(self.state, RepoState::Empty | RepoState::Failed(_))
    }

    pub(crate) fn open_repository(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.open(path, cx);
    }

    /// The start screen's buttons: Open, Create and Clone Repository….
    pub(crate) fn choose_repository(&mut self, cx: &mut Context<Self>) {
        self.prompt_open(cx);
    }

    pub(crate) fn choose_new_repository(&mut self, cx: &mut Context<Self>) {
        self.prompt_init(cx);
    }

    pub(crate) fn choose_clone(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt_clone(window, cx);
    }

    /// The commands of an open repository, in the order the palette lists their groups.
    pub(crate) fn palette_commands(&self, cx: &mut Context<Self>) -> Vec<PaletteCommand> {
        let RepoState::Loaded(repo) = &self.state else {
            return Vec::new();
        };
        let this = cx.entity().downgrade();
        let head = match &repo.refs.head {
            HeadInfo::Branch { oid, .. } | HeadInfo::Detached { oid } => Some(*oid),
            HeadInfo::Unborn { .. } => None,
        };
        let mut commands = Vec::new();
        self.centre_commands(&mut commands, cx);
        self.operation_commands(&this, &mut commands);
        self.commit_commands(&this, head, &mut commands, cx);
        self.change_commands(&this, &mut commands, cx);
        self.remote_commands(&this, &mut commands);
        self.repository_commands(&this, head, &mut commands);
        self.identity_commands(&this, &mut commands, cx);
        self.view_commands(&this, head, &mut commands);
        self.row_commands(&this, &mut commands, cx);
        self.file_commands(&this, &mut commands);
        commands
    }

    /// The buttons of the view shown in the centre, listed first: the conflict tool, else
    /// the rebase editor, else a blame (its cursor line's items, #29), else a diff.
    fn centre_commands(&self, commands: &mut Vec<PaletteCommand>, cx: &App) {
        if let Some(view) = &self.conflict_view {
            commands.extend(view.read(cx).palette_commands(&view.downgrade()));
        } else if let Some(view) = &self.rebase_view {
            commands.extend(view.read(cx).palette_commands(&view.downgrade(), cx));
        } else if let Some(view) = &self.blame_view {
            commands.extend(view.read(cx).palette_commands(&view.downgrade()));
        } else if let Some(view) = &self.diff_view {
            commands.extend(view.read(cx).palette_commands(&view.downgrade()));
        }
    }

    /// The sidebar's row menus: every enabled item of every row (as if nothing were
    /// filtered or closed), with what it acts on as the detail.
    fn row_commands(&self, this: &WeakEntity<Self>, commands: &mut Vec<PaletteCommand>, cx: &App) {
        for row in self.sidebar_rows(true, cx) {
            if let Some((group, detail, actions)) = self.sidebar_row_actions(&row) {
                commands.extend(row_action_commands(this, &heading(group), &detail, actions));
            }
        }
    }

    /// Each changed file's links (stage or mark resolved, discard, unstage) and its menu.
    fn file_commands(&self, this: &WeakEntity<Self>, commands: &mut Vec<PaletteCommand>) {
        let Some(status) = &self.status else {
            return;
        };
        let group = heading("palette.group.files");
        let unstaged: Vec<&StatusEntry> = status.unstaged().take(FILE_LIMIT).collect();
        for &entry in &unstaged {
            entry_commands(
                this,
                &group,
                entry,
                false,
                &[],
                true,
                commands,
                self.sync.lfs_locking(),
            );
        }
        for entry in status.staged().take(FILE_LIMIT) {
            // A file in both lists has its menu once.
            let menu = !unstaged.iter().any(|other| other.path == entry.path);
            entry_commands(
                this,
                &group,
                entry,
                true,
                &[],
                menu,
                commands,
                self.sync.lfs_locking(),
            );
        }
    }

    /// The operation banner's buttons, while a merge, rebase, … or bisect is stopped.
    fn operation_commands(&self, this: &WeakEntity<Self>, commands: &mut Vec<PaletteCommand>) {
        let Some(operation) = self.operation else {
            return;
        };
        let group = heading("palette.group.operation");
        if matches!(operation, Operation::Bisect) {
            for (mark, label) in [
                (BisectMark::Good, t!("palette.bisect_good")),
                (BisectMark::Bad, t!("palette.bisect_bad")),
                (BisectMark::Skip, t!("palette.bisect_skip")),
            ] {
                commands.push(command(this, &group, label, move |w, window, cx| {
                    w.bisect_mark_head(mark, window, cx)
                }));
            }
            if self.bisect_running.is_some() {
                commands.push(command(
                    this,
                    &group,
                    t!("palette.bisect_run_cancel"),
                    |w, _, _| w.cancel_bisect_run(),
                ));
            } else {
                commands.push(command(
                    this,
                    &group,
                    t!("bisect.run_button"),
                    |w, window, cx| w.open_bisect_run(window, cx),
                ));
            }
            commands.push(command(
                this,
                &group,
                t!("palette.bisect_reset"),
                |w, window, cx| w.run_bisect(bisect_reset, window, cx),
            ));
            return;
        }
        for action in [
            OperationAction::Continue,
            OperationAction::Skip,
            OperationAction::Abort,
        ] {
            if let Some(label) = operation_command_label(operation, action) {
                commands.push(command(this, &group, label, move |w, window, cx| {
                    w.run_operation(operation, action, window, cx)
                }));
            }
        }
    }

    /// The graph's row menu for the selected commit.
    pub(super) fn commit_commands(
        &self,
        this: &WeakEntity<Self>,
        head: Option<ObjectId>,
        commands: &mut Vec<PaletteCommand>,
        cx: &App,
    ) {
        let Some(graph) = self.graph_view.clone() else {
            return;
        };
        let group = heading("palette.group.commit");
        let picks = graph.read(cx).picked_commits(false);
        if picks.len() > 1 {
            let saves = picks.clone();
            let label = tn!("graph.cherry_pick_multiple", count = picks.len());
            commands.push(command(this, &group, label, move |w, window, cx| {
                w.pick_commits(picks.clone(), false, window, cx)
            }));
            let label = tn!("files.save_patches", count = saves.len());
            commands.push(command(this, &group, label, move |w, window, cx| {
                w.save_patches(saves.clone(), window, cx)
            }));
        }
        let Some(commit) = graph.read(cx).selected_commit() else {
            return;
        };
        let detail = format!("{} {}", commit.id.to_hex_with_len(8), commit.summary);
        let id = commit.id;
        let is_head = head == Some(id);
        let mut add = |label: Cow<'_, str>, event: Box<dyn Fn() -> GraphEvent>| {
            let graph = graph.clone();
            commands.push(
                command(this, &group, label, move |w, _, cx| {
                    w.on_graph_event(graph.clone(), &event(), cx)
                })
                .detail(detail.clone()),
            );
        };
        add(
            t!("branch.new_here"),
            Box::new(move || GraphEvent::CreateBranchAt(id)),
        );
        add(
            t!("branch.checkout_detached"),
            Box::new(move || GraphEvent::CheckoutCommit(id)),
        );
        add(
            t!("tag.new_here"),
            Box::new(move || GraphEvent::CreateTagAt(id)),
        );
        if !is_head {
            let picked = commit.clone();
            add(
                t!("rewrite.cherry_pick"),
                Box::new(move || GraphEvent::CherryPick(picked.clone())),
            );
            let loose = commit.clone();
            add(
                t!("rewrite.cherry_pick_no_commit"),
                Box::new(move || GraphEvent::CherryPickNoCommit(loose.clone())),
            );
        }
        let reverted = commit.clone();
        add(
            t!("rewrite.revert"),
            Box::new(move || GraphEvent::Revert(reverted.clone())),
        );
        let from = commit.clone();
        add(
            t!("rebase.menu_from"),
            Box::new(move || GraphEvent::InteractiveRebaseFrom(from.clone())),
        );
        if !is_head {
            let rebased = commit.clone();
            add(
                t!("rebase.menu"),
                Box::new(move || GraphEvent::InteractiveRebase(rebased.clone())),
            );
            for (mode, label) in [
                (ResetMode::Soft, t!("palette.reset_soft")),
                (ResetMode::Mixed, t!("palette.reset_mixed")),
                (ResetMode::Hard, t!("palette.reset_hard")),
            ] {
                let reset = commit.clone();
                add(
                    label,
                    Box::new(move || GraphEvent::Reset(reset.clone(), mode)),
                );
            }
        }
        if !is_head && head.is_some() {
            add(
                t!("graph.select_merge_base"),
                Box::new(move || GraphEvent::SelectMergeBase(id)),
            );
        }
        add(
            t!("compare.with_another"),
            Box::new(move || GraphEvent::CompareWith(id)),
        );
        if !commit.merge {
            for (edit, label) in graph_view::quick_edits() {
                let quick = commit.clone();
                add(
                    label.into(),
                    Box::new(move || GraphEvent::QuickRebase(quick.clone(), edit)),
                );
            }
            let staged = self
                .status
                .as_ref()
                .is_some_and(|status| status.staged().next().is_some());
            if staged {
                let fixup = commit.clone();
                add(
                    t!("rebase.quick_fixup"),
                    Box::new(move || GraphEvent::CreateFixup(fixup.clone())),
                );
            }
        }
        add(
            t!("tree.browse"),
            Box::new(move || GraphEvent::BrowseFiles(id)),
        );
        add(
            t!("files.save_patch"),
            Box::new(move || GraphEvent::SavePatch(id)),
        );
        add(
            t!("files.export_archive"),
            Box::new(move || GraphEvent::Archive(id)),
        );
        for (mark, label) in graph_view::bisect_items(self.bisect.is_some()) {
            add(label.into(), Box::new(move || GraphEvent::Bisect(id, mark)));
        }
    }

    /// The staging panel's links, and the stash.
    fn change_commands(
        &self,
        this: &WeakEntity<Self>,
        commands: &mut Vec<PaletteCommand>,
        cx: &App,
    ) {
        let group = heading("palette.group.changes");
        if let Some(status) = &self.status {
            let unstaged: Vec<StatusEntry> = status.unstaged().cloned().collect();
            let staged: Vec<StatusEntry> = status.staged().cloned().collect();
            if !status.is_clean() {
                commands.push(command(
                    this,
                    &group,
                    t!("palette.show_changes"),
                    |w, _, cx| {
                        if let Some(graph) = w.graph_view.clone() {
                            graph.update(cx, |graph, cx| graph.select_wip(cx));
                        }
                    },
                ));
            }
            if self.staging_view.read(cx).can_commit(cx) {
                commands.push(
                    command(this, &group, t!("palette.commit"), |w, _, cx| {
                        w.commit_staged(cx)
                    })
                    .key(Commit, Some(KEY_CONTEXT)),
                );
            }
            if !unstaged.is_empty() {
                commands.push(
                    command(this, &group, t!("staging.stage_all"), |w, _, cx| {
                        w.stage_all(cx)
                    })
                    .key(StageAll, Some(KEY_CONTEXT)),
                );
                commands.push(command(
                    this,
                    &group,
                    t!("palette.discard_all"),
                    move |w, window, cx| {
                        let entries = unstaged.clone();
                        w.staging_view.update(cx, |staging, cx| {
                            staging.confirm_discard(entries, window, cx)
                        })
                    },
                ));
            }
            if !staged.is_empty() {
                commands.push(
                    command(this, &group, t!("staging.unstage_all"), |w, _, cx| {
                        w.unstage_all(cx)
                    })
                    .key(UnstageAll, Some(KEY_CONTEXT)),
                );
            }
            if !status.is_clean() && self.remote_op.is_none() {
                commands.push(
                    command(this, &group, t!("palette.stash"), |w, window, cx| {
                        w.ask_stash(window, cx)
                    })
                    .key(StashChanges, Some(KEY_CONTEXT)),
                );
            }
        }
        if !self.stashes.is_empty() && self.remote_op.is_none() {
            commands.push(
                command(this, &group, t!("palette.pop"), |w, window, cx| {
                    w.pop_stash(window, cx)
                })
                .key(PopStash, Some(KEY_CONTEXT)),
            );
        }
    }

    /// Commit, as the staging panel's button: with its message, when it could.
    pub(super) fn commit_staged(&mut self, cx: &mut Context<Self>) {
        if self.is_start_screen() {
            return;
        }
        self.staging_view
            .update(cx, |staging, cx| staging.commit(cx));
    }

    /// Stage every unstaged file.
    pub(super) fn stage_all(&mut self, cx: &mut Context<Self>) {
        let Some(status) = &self.status else {
            return;
        };
        let entries: Vec<StatusEntry> = status.unstaged().cloned().collect();
        if !entries.is_empty() {
            let event = StagingEvent::Stage(entries);
            self.on_staging_event(self.staging_view.clone(), &event, cx);
        }
    }

    /// Unstage every staged file.
    pub(super) fn unstage_all(&mut self, cx: &mut Context<Self>) {
        let Some(status) = &self.status else {
            return;
        };
        let entries: Vec<StatusEntry> = status.staged().cloned().collect();
        if !entries.is_empty() {
            let event = StagingEvent::Unstage(entries);
            self.on_staging_event(self.staging_view.clone(), &event, cx);
        }
    }

    /// The stash dialog for every change, when there are changes and no network operation.
    pub(super) fn ask_stash(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dirty = self
            .status
            .as_ref()
            .is_some_and(|status| !status.is_clean());
        if dirty && self.remote_op.is_none() {
            self.open_stash_dialog(Vec::new(), window, cx);
        }
    }

    /// Pop the newest stash, when there is one and no network operation.
    pub(super) fn pop_stash(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.remote_op.is_none() {
            self.pop_latest_stash(window, cx);
        }
    }

    /// The New tag dialog at HEAD; nothing on an unborn branch.
    pub(super) fn new_tag_at_head(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        if let Some(head) = head_oid(&repo.refs.head) {
            self.open_new_tag(head, window, cx);
        }
    }

    /// The toolbar's Fetch, Pull and Push with their menus, and the More menu's network
    /// items; Cancel while one runs.
    fn remote_commands(&self, this: &WeakEntity<Self>, commands: &mut Vec<PaletteCommand>) {
        let group = heading("palette.group.remote");
        if self.remote_op.is_some() {
            commands.push(
                command(this, &group, t!("palette.cancel_remote"), |w, _, cx| {
                    w.cancel_remote(cx)
                })
                .key(CancelRemote, Some(KEY_CONTEXT)),
            );
            return;
        }
        // Each with the action that runs it from the keyboard (#33).
        let mut remote = |label: Cow<'_, str>, kind: RemoteOpKind, action: Box<dyn Action>| {
            let command = command(this, &group, label, move |w, window, cx| {
                w.run_remote(kind, window, cx)
            });
            commands.push(command.key_boxed(action, Some(KEY_CONTEXT)));
        };
        remote(t!("toolbar.fetch"), RemoteOpKind::Fetch, Box::new(Fetch));
        remote(
            t!("toolbar.pull"),
            RemoteOpKind::Pull(PullMode::Configured),
            Box::new(Pull),
        );
        remote(
            t!("toolbar.pull_merge"),
            RemoteOpKind::Pull(PullMode::Merge),
            Box::new(PullMerge),
        );
        remote(
            t!("toolbar.pull_rebase"),
            RemoteOpKind::Pull(PullMode::Rebase),
            Box::new(PullRebase),
        );
        remote(
            t!("toolbar.pull_ff_only"),
            RemoteOpKind::Pull(PullMode::FastForwardOnly),
            Box::new(PullFastForward),
        );
        remote(
            t!("toolbar.push"),
            RemoteOpKind::Push(ForceMode::None),
            Box::new(Push),
        );
        remote(
            t!("toolbar.force_push"),
            RemoteOpKind::Push(ForceMode::WithLease),
            Box::new(ForcePush),
        );
        if self.sync.lfs_ready() {
            remote(
                t!("lfs.pull"),
                RemoteOpKind::LfsPull,
                Box::new(DownloadLfsFiles),
            );
            remote(
                t!("lfs.fetch"),
                RemoteOpKind::LfsFetch,
                Box::new(FetchLfsObjects),
            );
            remote(
                t!("lfs.fetch_all"),
                RemoteOpKind::LfsFetchAll,
                Box::new(FetchAllLfsObjects),
            );
        }
        if !self.sync.extras.shallow.is_empty() {
            remote(
                tn!("shallow.deepen", count = sync::DEEPEN_BY),
                RemoteOpKind::Deepen(Deepen::By(sync::DEEPEN_BY)),
                Box::new(DeepenHistory),
            );
            remote(
                t!("shallow.unshallow"),
                RemoteOpKind::Deepen(Deepen::All),
                Box::new(FetchAllHistory),
            );
        }
        let label = if self.sync.settings.background_fetch {
            t!("palette.background_fetch_off")
        } else {
            let minutes = self.sync.settings.fetch_interval_minutes;
            tn!("palette.background_fetch_on", count = minutes)
        };
        commands.push(
            command(this, &group, label, |w, window, cx| {
                w.toggle_background_fetch(window, cx)
            })
            .key(ToggleBackgroundFetch, Some(KEY_CONTEXT)),
        );
    }

    /// Identity profiles (#45): use each in this repository, delete it, and save this
    /// repository's identity as a new one.
    fn identity_commands(
        &self,
        this: &WeakEntity<Self>,
        commands: &mut Vec<PaletteCommand>,
        cx: &App,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        if repo.workdir.is_none() {
            return;
        }
        let group = heading("palette.group.repository");
        for profile in SettingsStore::settings(cx).identity_profiles.clone() {
            let detail = t!(
                "identity.profile_detail",
                name = profile.name,
                email = profile.email
            )
            .into_owned();
            let label = t!("identity.use_profile", profile = profile.label);
            let used = profile.clone();
            commands.push(
                command(this, &group, label, move |w, window, cx| {
                    w.apply_identity_profile(used.clone(), window, cx)
                })
                .detail(detail),
            );
            let label = t!("identity.delete_profile", profile = profile.label);
            let deleted = profile.label.clone();
            commands.push(command(this, &group, label, move |w, _, cx| {
                w.delete_identity_profile(&deleted, cx)
            }));
        }
        commands.push(command(
            this,
            &group,
            t!("identity.save_profile"),
            |w, window, cx| w.save_identity_profile(window, cx),
        ));
    }

    /// Undo and redo, the More menu's local items, and the sidebar's "Add …" rows.
    fn repository_commands(
        &self,
        this: &WeakEntity<Self>,
        head: Option<ObjectId>,
        commands: &mut Vec<PaletteCommand>,
    ) {
        let group = heading("palette.group.repository");
        if !self.undo_running {
            if let Some(entry) = self.undo_stack.last() {
                commands.push(
                    command(
                        this,
                        &group,
                        t!("palette.undo", op = entry.label),
                        |w, window, cx| w.undo_last(false, window, cx),
                    )
                    .key(UndoOperation, Some(KEY_CONTEXT)),
                );
            }
            if let Some(entry) = self.redo_stack.last() {
                commands.push(
                    command(
                        this,
                        &group,
                        t!("palette.redo", op = entry.label),
                        |w, window, cx| w.undo_last(true, window, cx),
                    )
                    .key(RedoOperation, Some(KEY_CONTEXT)),
                );
            }
        }
        commands.push(
            command(
                this,
                &group,
                t!("external.open_terminal"),
                |w, window, cx| w.open_repository_in(external::Program::Terminal, window, cx),
            )
            .key(OpenInTerminal, Some(KEY_CONTEXT)),
        );
        commands.push(
            command(
                this,
                &group,
                t!("external.open_file_manager"),
                |w, window, cx| w.open_repository_in(external::Program::FileManager, window, cx),
            )
            .key(OpenInFileManager, Some(KEY_CONTEXT)),
        );
        commands.push(
            command(this, &group, t!("branch.new"), |w, window, cx| {
                w.open_new_branch(None, window, cx)
            })
            .key(NewBranch, Some(KEY_CONTEXT)),
        );
        if head.is_some() {
            commands.push(
                command(this, &group, t!("palette.new_tag"), |w, window, cx| {
                    w.new_tag_at_head(window, cx)
                })
                .key(NewTag, Some(KEY_CONTEXT)),
            );
        }
        commands.push(
            command(this, &group, t!("remote.add_item"), |w, window, cx| {
                w.open_add_remote(window, cx)
            })
            .key(AddRemote, Some(KEY_CONTEXT)),
        );
        commands.push(
            command(this, &group, t!("worktree.add_item"), |w, window, cx| {
                w.open_add_worktree(None, window, cx)
            })
            .key(AddWorktree, Some(KEY_CONTEXT)),
        );
        commands.push(command(
            this,
            &group,
            t!("worktree.add_in_folder_item"),
            |w, window, cx| w.pick_worktree_folder(window, cx),
        ));
        if self.worktrees.iter().any(|info| info.prunable.is_some()) {
            commands.push(command(
                this,
                &group,
                t!("worktree.prune"),
                |w, window, cx| w.prune_missing_worktrees(window, cx),
            ));
        }
        if !self.submodules.is_empty() {
            commands.push(command(
                this,
                &group,
                t!("submodule.update_all"),
                |w, window, cx| w.run_submodule_update(PathBuf::new(), Vec::new(), window, cx),
            ));
        }
        commands.push(
            command(this, &group, t!("files.apply_patches"), |w, window, cx| {
                w.prompt_apply_patches(window, cx)
            })
            .key(ApplyPatches, Some(KEY_CONTEXT)),
        );
        commands.push(
            command(
                this,
                &group,
                t!("files.clean_untracked"),
                |w, window, cx| w.ask_clean(false, window, cx),
            )
            .key(CleanUntracked, Some(KEY_CONTEXT)),
        );
        commands.push(
            command(this, &group, t!("files.clean_ignored"), |w, window, cx| {
                w.ask_clean(true, window, cx)
            })
            .key(CleanIgnored, Some(KEY_CONTEXT)),
        );
    }

    /// Search, the reflog and activity panels, and the files of HEAD.
    fn view_commands(
        &self,
        this: &WeakEntity<Self>,
        head: Option<ObjectId>,
        commands: &mut Vec<PaletteCommand>,
    ) {
        let group = heading("palette.group.view");
        commands.push(
            command(this, &group, t!("palette.search"), |w, window, cx| {
                if w.centre_covered() {
                    w.close_centre();
                }
                if let Some(graph) = w.graph_view.clone() {
                    graph.update(cx, |graph, cx| graph.focus_search_box(window, cx));
                }
                cx.notify();
            })
            .key(FocusSearch, Some(graph_view::CONTEXT)),
        );
        let reflog = if self.reflog_view.is_some() {
            t!("palette.hide_reflog")
        } else {
            t!("palette.show_reflog")
        };
        commands.push(command(this, &group, reflog, |w, _, cx| {
            if w.reflog_view.is_some() {
                w.close_reflog();
                cx.notify();
            } else {
                w.open_reflog("HEAD".into(), "HEAD".into(), cx);
            }
        }));
        commands.push(
            command(this, &group, t!("dangling.show"), |w, _, cx| {
                w.open_dangling(cx)
            })
            .key(ShowDanglingCommits, Some(KEY_CONTEXT)),
        );
        commands.push(
            command(this, &group, t!("trash.show"), |w, window, cx| {
                w.open_trash(window, cx)
            })
            .key(ShowTrash, Some(KEY_CONTEXT)),
        );
        commands.push(
            command(this, &group, t!("submodule.add_menu"), |w, window, cx| {
                w.open_add_submodule(window, cx)
            })
            .key(AddSubmodule, Some(KEY_CONTEXT)),
        );
        if self.sync.lfs_ready() {
            commands.push(
                command(this, &group, t!("lfs.locks_menu"), |w, window, cx| {
                    w.show_lfs_locks(window, cx)
                })
                .key(ShowLfsLocks, Some(KEY_CONTEXT)),
            );
            if self.remote_op.is_none() {
                commands.push(
                    command(this, &group, t!("lfs.prune"), |w, window, cx| {
                        w.ask_lfs_prune(window, cx)
                    })
                    .key(PruneLfsObjects, Some(KEY_CONTEXT)),
                );
            }
        }
        // Useful before LFS is used, so offered whatever the repository's LFS state; without
        // git-lfs it says so.
        commands.push(command(
            this,
            &group,
            t!("lfs.sizes_branch"),
            |w, window, cx| w.show_lfs_sizes(false, window, cx),
        ));
        commands.push(command(
            this,
            &group,
            t!("lfs.sizes_all"),
            |w, window, cx| w.show_lfs_sizes(true, window, cx),
        ));
        // History rewrites of the checked-out branch (#71).
        commands.push(command(
            this,
            &group,
            t!("lfs.migrate_import_menu"),
            |w, window, cx| w.ask_lfs_migrate(MigrateDirection::Import, String::new(), window, cx),
        ));
        commands.push(command(
            this,
            &group,
            t!("lfs.migrate_export_menu"),
            |w, window, cx| w.ask_lfs_migrate(MigrateDirection::Export, String::new(), window, cx),
        ));
        let activity = if self.show_activity {
            t!("palette.hide_activity")
        } else {
            t!("palette.show_activity")
        };
        commands.push(command(this, &group, activity, |w, _, cx| {
            w.show_activity = !w.show_activity;
            cx.notify();
        }));
        if let Some(head) = head {
            commands.push(command(
                this,
                &group,
                t!("palette.browse_head"),
                move |w, _, cx| w.open_tree(head, cx),
            ));
        }
        // The graph's view menu, one toggle per option.
        if let Some(walk) = self.walk {
            let toggles = [
                (
                    if walk.current_branch {
                        t!("palette.graph_all_branches")
                    } else {
                        t!("palette.graph_current_branch")
                    },
                    WalkOptions {
                        current_branch: !walk.current_branch,
                        ..walk
                    },
                ),
                (
                    if walk.first_parent {
                        t!("palette.graph_all_parents")
                    } else {
                        t!("palette.graph_first_parent")
                    },
                    WalkOptions {
                        first_parent: !walk.first_parent,
                        ..walk
                    },
                ),
                (
                    if walk.topo_order {
                        t!("palette.graph_date_order")
                    } else {
                        t!("palette.graph_topo_order")
                    },
                    WalkOptions {
                        topo_order: !walk.topo_order,
                        ..walk
                    },
                ),
            ];
            for (label, options) in toggles {
                commands.push(command(this, &group, label, move |w, _, cx| {
                    w.set_walk(options, cx)
                }));
            }
            if self.tips.as_ref().is_some_and(|tips| !tips.is_empty()) {
                let label = t!("palette.graph_show_every_branch");
                commands.push(command(this, &group, label, |w, _, cx| {
                    w.show_every_branch(cx)
                }));
            }
        }
    }
}

/// The palette's label for `action` on the operation in progress, when it can take it: one
/// sentence per operation, not its name put into one (#46: "Continue the Rebase" read
/// wrong even in English).
fn operation_command_label(operation: Operation, action: OperationAction) -> Option<String> {
    use Operation::*;
    use OperationAction::*;
    let label = match (action, operation) {
        (_, Bisect) => return None,
        (Continue, Merge) => t!("palette.continue_merge"),
        (Continue, Rebase) => t!("palette.continue_rebase"),
        (Continue, CherryPick) => t!("palette.continue_cherry_pick"),
        (Continue, Revert) => t!("palette.continue_revert"),
        (Continue, ApplyMailbox) => t!("palette.continue_am"),
        (Skip, Merge) => return None,
        (Skip, Rebase) => t!("palette.skip_rebase"),
        (Skip, CherryPick) => t!("palette.skip_cherry_pick"),
        (Skip, Revert) => t!("palette.skip_revert"),
        (Skip, ApplyMailbox) => t!("palette.skip_am"),
        (Abort, Merge) => t!("palette.abort_merge"),
        (Abort, Rebase) => t!("palette.abort_rebase"),
        (Abort, CherryPick) => t!("palette.abort_cherry_pick"),
        (Abort, Revert) => t!("palette.abort_revert"),
        (Abort, ApplyMailbox) => t!("palette.abort_am"),
    };
    Some(label.into_owned())
}

#[cfg(test)]
mod tests {
    use super::{Operation, OperationAction, operation_command_label};

    #[test]
    fn each_operation_names_itself_in_its_commands() {
        use Operation::*;
        for operation in [Merge, Rebase, CherryPick, Revert, ApplyMailbox, Bisect] {
            let label = |action| operation_command_label(operation, action);
            let continues = label(OperationAction::Continue);
            assert_eq!(
                continues.is_some(),
                operation.can_continue(),
                "{operation:?}"
            );
            assert_eq!(
                label(OperationAction::Skip).is_some(),
                operation.can_skip(),
                "{operation:?}"
            );
            assert_eq!(label(OperationAction::Abort).is_some(), operation != Bisect);
            // Lowercase inside the sentence (it read "Continue the Rebase").
            if let Some(text) = continues {
                assert_eq!(text, text[..1].to_owned() + &text[1..].to_lowercase());
            }
        }
    }
}
