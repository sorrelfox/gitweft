//! Worktrees: listing, adding, locking, moving, pruning and removing.

use super::*;

impl Workspace {
    /// Reads the repository's worktrees for the sidebar.
    pub(super) fn load_worktrees(&mut self, cx: &mut Context<Self>) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let cwd = repo.cwd();
        let job = self.jobs.spawn(Lane::Coalesce("worktrees"), move |cancel| {
            list_worktrees(&git, &cwd, cancel).unwrap_or_default()
        });
        cx.spawn(async move |this, cx| {
            if let Some(worktrees) = job.join().await {
                this.update(cx, |this, cx| {
                    if this.worktrees != worktrees {
                        this.worktrees = worktrees;
                        this.push_worktrees(cx);
                        cx.notify();
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    /// Hands the graph the local branches checked out in other worktrees, for their
    /// pills' mark.
    pub(super) fn push_worktrees(&mut self, cx: &mut Context<Self>) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let branches = other_worktree_branches(&self.worktrees, repo.workdir.as_deref());
        self.details_view.update(cx, |details, cx| {
            details.set_worktrees(branches.clone(), cx)
        });
        if let Some(graph) = &self.graph_view {
            graph.update(cx, |graph, cx| graph.set_worktrees(branches, cx));
        }
    }

    /// "Add worktree in a folder…": the desktop's folder picker first, then the add
    /// dialog with that folder filled in.
    pub(super) fn pick_worktree_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t!("worktree.pick_folder_prompt").into_owned().into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await
                && let Some(folder) = paths.into_iter().next()
            {
                this.update_in(cx, |this, window, cx| {
                    this.open_add_worktree(Some(folder), window, cx)
                })
                .ok();
            }
        })
        .detach();
    }

    /// "Add worktree…": a new branch from HEAD, checked out in a folder of its own
    /// (next to the repository unless one is given, or `folder` picked first), then
    /// opened in a new tab if asked. Worktrees live outside refs, so none of this is
    /// undoable.
    pub(super) fn open_add_worktree(
        &mut self,
        folder: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let open_after = Rc::new(Cell::new(true));
        let subtitle = if folder.is_some() {
            t!("worktree.add_subtitle_folder")
        } else {
            t!("worktree.add_subtitle")
        };
        let spec = TextDialog {
            title: t!("worktree.add_title").into_owned(),
            subtitle: Some(subtitle.into_owned()),
            placeholder: t!("worktree.branch_placeholder").into_owned(),
            initial: String::new(),
            detail: Some((
                t!("worktree.folder_placeholder").into_owned(),
                folder
                    .map(|folder| folder.display().to_string())
                    .unwrap_or_default(),
            )),
            checkbox: Some((t!("worktree.open_after").into_owned(), open_after.clone())),
            ok_label: t!("worktree.add").into_owned(),
            allow_empty: false,
            multiline: false,
        };
        let submit: SubmitText = Rc::new(move |this, name, folder, window, cx| {
            let name = name.trim().to_owned();
            let folder = folder.trim();
            let Some(path) = this.worktree_path(&name, folder) else {
                return;
            };
            let inside = if folder.is_empty() {
                None
            } else {
                this.worktree_path(&name, "")
            };
            let target = WorktreeTarget::NewBranch { name, start: None };
            this.run_add_worktree(path, inside, target, open_after.get(), window, cx);
        });
        self.open_text_dialog(spec, submit, window, cx);
    }

    /// "Check out in new worktree…" on a local branch: asks for the folder.
    pub(super) fn open_branch_worktree(
        &mut self,
        branch: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(initial) = self.worktree_path(&branch, "") else {
            return;
        };
        let open_after = Rc::new(Cell::new(true));
        let spec = TextDialog {
            title: t!("worktree.branch_title", branch = branch).into_owned(),
            subtitle: None,
            placeholder: t!("worktree.folder").into_owned(),
            initial: initial.display().to_string(),
            detail: None,
            checkbox: Some((t!("worktree.open_after").into_owned(), open_after.clone())),
            ok_label: t!("worktree.add").into_owned(),
            allow_empty: false,
            multiline: false,
        };
        let submit: SubmitText = Rc::new(move |this, folder, _, window, cx| {
            let Some(path) = this.worktree_path(&branch, folder.trim()) else {
                return;
            };
            let inside = if folder.trim().is_empty() {
                None
            } else {
                this.worktree_path(&branch, "")
            };
            let target = WorktreeTarget::Branch(branch.clone());
            this.run_add_worktree(path, inside, target, open_after.get(), window, cx);
        });
        self.open_text_dialog(spec, submit, window, cx);
    }

    /// Where a worktree for `branch` goes: `folder` (relative to this worktree, as git
    /// takes it), or next to the main worktree as `<main folder>-<branch>` when it's empty,
    /// whichever worktree this tab shows.
    pub(super) fn worktree_path(&self, branch: &str, folder: &str) -> Option<PathBuf> {
        let RepoState::Loaded(repo) = &self.state else {
            return None;
        };
        let workdir = repo.workdir.as_ref()?;
        if !folder.is_empty() {
            return Some(workdir.join(folder));
        }
        let main = self
            .worktrees
            .iter()
            .find(|info| info.is_main && !info.bare)
            .map_or(workdir, |info| &info.path);
        let name = main.file_name()?.to_string_lossy();
        let parent = main.parent().unwrap_or(main);
        Some(parent.join(format!("{name}-{}", branch.replace('/', "-"))))
    }

    /// Adds a worktree in the write lane; once it exists (and only then), opens its tab.
    /// A folder the user chose that has files already (git refuses it) gets the worktree
    /// inside it, named as `default` is (the folder it would get when none is given).
    pub(super) fn run_add_worktree(
        &mut self,
        path: PathBuf,
        default: Option<PathBuf>,
        target: WorktreeTarget,
        open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let cwd = repo.cwd();
        let task = self
            .jobs
            .spawn(Lane::Write, move |cancel| -> Result<PathBuf, GitError> {
                let path = match &default {
                    Some(default) => worktree_in(&path, default),
                    None => path,
                };
                add_worktree(&git, &cwd, &path, &target, cancel)?;
                Ok(path)
            });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.join().await;
            this.update_in(cx, |this, _, cx| {
                match result {
                    Some(Ok(path)) => {
                        let name = folder_name(&path);
                        note(
                            Toast::success(t!("worktree.added", name = name).into_owned()),
                            cx,
                        );
                        if open {
                            cx.emit(WorkspaceEvent::OpenInNewTab(path));
                        }
                    }
                    Some(Err(err)) => note(
                        Toast::error(err.summary()).title(t!("worktree.failed").into_owned()),
                        cx,
                    ),
                    None => {}
                }
                this.load_worktrees(cx);
                this.load_submodules(cx);
                this.refresh_refs(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Locks a worktree after asking why (optional: the sidebar and `git worktree list`
    /// show it), or unlocks it. A lock keeps `prune`, `remove` and `move` away from it.
    pub(super) fn set_worktree_lock(
        &mut self,
        path: PathBuf,
        lock: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !lock {
            self.run_worktree_lock(path, None, window, cx);
            return;
        }
        let spec = TextDialog {
            title: t!("worktree.lock_title", name = folder_name(&path)).into_owned(),
            subtitle: Some(t!("worktree.lock_subtitle").into_owned()),
            placeholder: t!("worktree.lock_placeholder").into_owned(),
            initial: String::new(),
            detail: None,
            checkbox: None,
            ok_label: t!("worktree.lock_ok").into_owned(),
            allow_empty: true,
            multiline: false,
        };
        let submit: SubmitText = Rc::new(move |this, reason, _, window, cx| {
            let reason = reason.trim().to_owned();
            this.run_worktree_lock(path.clone(), Some(reason), window, cx);
        });
        self.open_text_dialog(spec, submit, window, cx);
    }

    /// Locks the worktree at `path` with `reason` (possibly empty), or unlocks it (`None`).
    pub(super) fn run_worktree_lock(
        &mut self,
        path: PathBuf,
        reason: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_write_op(
            t!("worktree.failed").into_owned(),
            None,
            move |git, cwd, cancel| {
                let name = folder_name(&path);
                if let Some(reason) = &reason {
                    lock_worktree(git, cwd, &path, Some(reason), cancel)?;
                    Ok(t!("worktree.locked_done", name = name).into_owned())
                } else {
                    unlock_worktree(git, cwd, &path, cancel)?;
                    Ok(t!("worktree.unlocked_done", name = name).into_owned())
                }
            },
            window,
            cx,
        );
    }

    /// "Move…" on a worktree: asks for its new folder (relative to this worktree, as git
    /// takes it, or absolute), then `git worktree move`.
    pub(super) fn open_move_worktree(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let spec = TextDialog {
            title: t!("worktree.move_title", name = folder_name(&path)).into_owned(),
            subtitle: Some(t!("worktree.move_subtitle").into_owned()),
            placeholder: t!("worktree.folder").into_owned(),
            initial: path.display().to_string(),
            detail: None,
            checkbox: None,
            ok_label: t!("worktree.move_ok").into_owned(),
            allow_empty: false,
            multiline: false,
        };
        let submit: SubmitText = Rc::new(move |this, folder, _, window, cx| {
            let Some(to) = this.worktree_path("", folder.trim()) else {
                return;
            };
            let from = path.clone();
            this.run_write_op(
                t!("worktree.failed").into_owned(),
                None,
                move |git, cwd, cancel| {
                    gitweft_git::worktrees::move_worktree(git, cwd, &from, &to, cancel)?;
                    Ok(t!("worktree.moved", path = to.display().to_string()).into_owned())
                },
                window,
                cx,
            );
        });
        self.open_text_dialog(spec, submit, window, cx);
    }

    pub(super) fn prune_missing_worktrees(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run_write_op(
            t!("worktree.failed").into_owned(),
            None,
            move |git, cwd, cancel| {
                prune_worktrees(git, cwd, cancel)?;
                Ok(t!("worktree.pruned").into_owned())
            },
            window,
            cx,
        );
    }

    /// "Remove…": first finds out whether the worktree has local changes, so the question
    /// can say they'd be lost; only then is the removal forced.
    pub(super) fn confirm_remove_worktree(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let checked = path.clone();
        let job = self
            .jobs
            .spawn(Lane::Latest("worktree-check"), move |cancel| {
                worktree_has_changes(&git, &checked, cancel).unwrap_or(false)
            });
        cx.spawn_in(window, async move |this, cx| {
            let Some(changes) = job.join().await else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.ask_remove_worktree(path, changes, window, cx)
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn ask_remove_worktree(
        &mut self,
        path: PathBuf,
        changes: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = folder_name(&path);
        let message = t!("worktree.remove_confirm", name = name).into_owned();
        let detail = if changes {
            t!("worktree.remove_dirty")
        } else {
            t!("worktree.remove_detail")
        }
        .into_owned();
        self.confirm(
            message,
            detail,
            t!("worktree.remove_ok").into_owned(),
            move |this, confirmed, window, cx| {
                if confirmed {
                    this.run_write_op(
                        t!("worktree.failed").into_owned(),
                        None,
                        move |git, cwd, cancel| {
                            remove_worktree(git, cwd, &path, changes, cancel)?;
                            Ok(t!("worktree.removed", name = name).into_owned())
                        },
                        window,
                        cx,
                    )
                }
            },
            window,
            cx,
        );
    }
}
