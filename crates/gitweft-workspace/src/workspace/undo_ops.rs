//! Undo and redo: the oplog, its history and the toolbar's buttons.

use super::*;

impl Workspace {
    /// A new operation: it can be undone, and what was undone before can't be redone.
    /// Shown at once; the saved log takes it in the write lane.
    pub(super) fn push_undo(&mut self, entry: UndoEntry, cx: &mut Context<Self>) {
        if self.undo_stack.len() >= UNDO_LIMIT {
            self.undo_stack.remove(0);
        }
        self.undo_stack.push(entry.clone());
        self.redo_stack.clear();
        self.sync_undo_history(Some(Box::new(move |history| history.push(entry))), cx);
    }

    /// Reads the saved stacks of the repository at `root`: when it's opened, and again
    /// whenever its refs change, since another tab or Gitweft process on the same
    /// repository shares the log (`oplog::update`).
    pub(super) fn load_undo_history(&mut self, root: &std::path::Path, cx: &mut Context<Self>) {
        let file = oplog::oplog_file(root);
        if self.undo_file.as_ref() != Some(&file) {
            self.undo_file = Some(file);
        }
        self.sync_undo_history(None, cx);
    }

    /// Applies `change` to the saved log (none: only reads it) in the write lane, so
    /// reads and writes land in order, then shows the stacks as saved unless a newer read
    /// or write is on its way.
    pub(super) fn sync_undo_history(&mut self, change: Option<UndoChange>, cx: &mut Context<Self>) {
        let Some(file) = self.undo_file.clone() else {
            return;
        };
        self.undo_syncs += 1;
        let sync = self.undo_syncs;
        let job = self.jobs.spawn(Lane::Write, {
            let file = file.clone();
            move |_| match change {
                Some(change) => oplog::update(&file, change),
                None => Ok(oplog::load(&file)),
            }
        });
        cx.spawn(async move |this, cx| {
            let Some(result) = job.join().await else {
                return;
            };
            this.update(cx, |this, cx| match result {
                Ok(history) => this.show_undo_history(&file, sync, history, cx),
                Err(err) => {
                    let text = t!("undo.save_failed", error = err.to_string()).into_owned();
                    crate::tab_notes::note_failure(text, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// The stacks as the log holds them after read or write number `sync`.
    pub(super) fn show_undo_history(
        &mut self,
        file: &std::path::Path,
        sync: u64,
        history: UndoHistory,
        cx: &mut Context<Self>,
    ) {
        // Another repository was opened meanwhile, or a newer result is coming.
        if self.undo_file.as_deref() != Some(file) || sync != self.undo_syncs {
            return;
        }
        if self.undo_stack != history.undo || self.redo_stack != history.redo {
            self.undo_stack = history.undo;
            self.redo_stack = history.redo;
            cx.notify();
        }
    }

    /// Undoes the newest operation (or redoes the newest undone one). A step that moves
    /// the checked-out branch back over commits a remote has asks first.
    pub(super) fn undo_last(&mut self, redo: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.undo_running {
            return;
        }
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let stack = if redo {
            &self.redo_stack
        } else {
            &self.undo_stack
        };
        let Some(entry) = stack.last() else {
            return;
        };
        let checked_out = match &repo.refs.head {
            HeadInfo::Branch { name, .. } if name.starts_with("refs/") => name.clone(),
            HeadInfo::Branch { name, .. } => format!("refs/heads/{name}"),
            _ => CHECKED_OUT.to_owned(),
        };
        let moves = entry
            .changes
            .iter()
            .find(|change| change.name == checked_out)
            .and_then(|change| Some((change.after?, change.before?)));
        let Some((tip, keep)) = moves else {
            self.apply_undo(redo, window, cx);
            return;
        };
        let label = entry.label.clone();
        // Blocks a second Ctrl+Z while the check runs.
        self.undo_running = true;
        cx.notify();
        self.check_published(
            tip.to_string(),
            Some(keep.to_string()),
            move |this, published, window, cx| {
                this.undo_running = false;
                if published.is_empty() {
                    this.apply_undo(redo, window, cx);
                    return;
                }
                let (message, button) = if redo {
                    (
                        t!("rewrite.redo_confirm", op = label),
                        t!("rewrite.redo_anyway"),
                    )
                } else {
                    (
                        t!("rewrite.undo_confirm", op = label),
                        t!("rewrite.undo_anyway"),
                    )
                };
                this.confirm(
                    message.into_owned(),
                    published_text(&published),
                    button.into_owned(),
                    move |this, confirmed, window, cx| {
                        if confirmed {
                            this.apply_undo(redo, window, cx);
                        }
                        cx.notify();
                    },
                    window,
                    cx,
                );
            },
            window,
            cx,
        );
    }

    /// Applies the newest undo (or redo) step in the write lane. An entry whose refs
    /// have moved since can never apply again, so it is dropped; after other failures
    /// (e.g. local changes in the way) it stays for another try.
    pub(super) fn apply_undo(&mut self, redo: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.undo_running {
            return;
        }
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let cwd = repo.cwd();
        let stack = if redo {
            &mut self.redo_stack
        } else {
            &mut self.undo_stack
        };
        let Some(entry) = stack.pop() else {
            return;
        };
        let applied = entry.clone();
        let trash = trash_dir();
        let log = self.undo_file.clone();
        // The step and the log's record of it in one job: another writer can't slip in
        // between. The log takes the step out if it was used (done, or refused for good).
        let job = self.jobs.spawn(Lane::Write, move |cancel| {
            let result = oplog::apply(&git, &cwd, &applied, &trash, cancel);
            let used = match &result {
                Ok(_) => true,
                Err(err) => err.kind == GitErrorKind::RefMoved,
            };
            let reverse = result.as_ref().ok().map(|done| done.reverse.clone());
            let saved = log.map(|log| {
                let history = oplog::update(&log, |history| {
                    if used {
                        history.used(&applied, redo, reverse);
                    }
                });
                (log, history)
            });
            (result, saved)
        });
        self.undo_syncs += 1;
        let sync = self.undo_syncs;
        self.undo_running = true;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let joined = job.join().await;
            this.update_in(cx, |this, _, cx| {
                this.undo_running = false;
                let label = entry.label.clone();
                let (result, saved) = match joined {
                    Some((result, saved)) => (Some(result), saved),
                    None => (None, None),
                };
                let (from, to) = if redo {
                    (&mut this.redo_stack, &mut this.undo_stack)
                } else {
                    (&mut this.undo_stack, &mut this.redo_stack)
                };
                match result {
                    Some(Ok(done)) => {
                        to.push(done.reverse);
                        let message = if redo {
                            t!("undo.redone", op = label)
                        } else {
                            t!("undo.undone", op = label)
                        };
                        note(Toast::success(message.into_owned()), cx);
                        if done.left_in_trash > 0 {
                            let text = tn!("undo.left_in_trash", count = done.left_in_trash);
                            note(Toast::warning(text.into_owned()), cx);
                        }
                    }
                    Some(Err(err)) => {
                        let text = if err.kind == GitErrorKind::RefMoved {
                            t!("undo.moved").into_owned()
                        } else {
                            from.push(entry);
                            err.summary()
                        };
                        let title = if redo {
                            t!("undo.redo_failed", op = label)
                        } else {
                            t!("undo.failed", op = label)
                        };
                        note(Toast::error(text).title(title.into_owned()), cx);
                    }
                    None => from.push(entry),
                }
                match saved {
                    Some((log, Ok(history))) => this.show_undo_history(&log, sync, history, cx),
                    Some((_, Err(err))) => {
                        let text = t!("undo.save_failed", error = err.to_string()).into_owned();
                        crate::tab_notes::note_failure(text, cx);
                    }
                    None => {}
                }
                this.refresh_refs(cx);
                this.refresh_status(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn render_undo_button(
        &self,
        redo: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (id, icon, stack) = if redo {
            ("redo", AppIcon::Redo, &self.redo_stack)
        } else {
            ("undo", AppIcon::Undo, &self.undo_stack)
        };
        let tooltip = match (stack.last(), redo) {
            (Some(entry), false) => t!("undo.tooltip", op = entry.label),
            (Some(entry), true) => t!("undo.redo_tooltip", op = entry.label),
            (None, false) => t!("undo.nothing"),
            (None, true) => t!("undo.nothing_to_redo"),
        }
        .into_owned();
        Button::new(id)
            .ghost()
            .small()
            .icon(Icon::from(icon))
            .tooltip(tooltip)
            .disabled(stack.is_empty() || self.undo_running)
            .on_click(cx.listener(move |this, _, window, cx| this.undo_last(redo, window, cx)))
    }

    /// Debug builds: `GITWEFT_DEBUG_UNDO=commit` records HEAD's last commit as if the app
    /// had made it, then undoes it once, so screenshot checks can see an undo.
    #[cfg(debug_assertions)]
    pub(super) fn debug_undo(&mut self, cx: &mut Context<Self>) {
        use std::sync::atomic::{AtomicBool, Ordering};
        static DONE: AtomicBool = AtomicBool::new(false);
        if std::env::var("GITWEFT_DEBUG_UNDO").ok().as_deref() != Some("commit")
            || DONE.swap(true, Ordering::Relaxed)
        {
            return;
        }
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let cwd = repo.cwd();
        let job = self
            .jobs
            .spawn(Lane::Write, move |cancel| -> Result<UndoEntry, GitError> {
                let name = undo::checked_out_ref(&git, &cwd, cancel)?;
                let names = [name.clone(), "HEAD~1".to_owned()];
                let values = undo::ref_values(&git, &cwd, &names, cancel)?;
                let change = undo::RefChange {
                    name,
                    before: values[1].1,
                    after: values[0].1,
                };
                UndoEntry::new("commit (debug)".to_owned(), vec![change], Worktree::Soft)
                    .ok_or_else(|| GitError::new(GitErrorKind::Other, "debug", "nothing moved"))
            });
        cx.spawn(async move |this, cx| {
            if let Some(Ok(entry)) = job.join().await {
                this.update(cx, |this, cx| {
                    this.push_undo(entry, cx);
                    this.with_window(|this, window, cx| this.undo_last(false, window, cx), cx);
                })
                .ok();
            }
        })
        .detach();
    }
}
