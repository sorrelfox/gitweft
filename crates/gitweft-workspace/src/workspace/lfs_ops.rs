//! Git LFS beyond downloads: setting it up in this clone (`git lfs install --local`),
//! file locks (listed in the palette, each with Unlock; "Lock a file…"), deleting old
//! local copies (`git lfs prune`, asked first), which kinds of files are large in plain
//! git (`git lfs migrate info`, read-only), and moving them into or out of LFS on the
//! checked-out branch (`git lfs migrate import|export`, a rewrite).

use gitweft_git::lfs::{
    LfsLock, LfsTypeSize, MigrateDirection, lfs_install_local, lfs_lock, lfs_locks, lfs_migrate,
    lfs_migrate_info, lfs_prunable, lfs_prune, lfs_unlock, migrate_patterns,
};
use gitweft_ui::current::mono_font;

use super::commands::{command, heading};
use super::*;
use crate::trash_view::size_text;

impl Workspace {
    /// Reads what `git lfs prune` would delete, then asks before deleting it (the remote
    /// keeps those copies, but they can't be had back offline).
    pub(super) fn ask_lfs_prune(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let Some(cwd) = repo.workdir.clone() else {
            return;
        };
        let task = self.jobs.spawn(Lane::Latest("lfs-prune"), move |cancel| {
            lfs_prunable(&git, &cwd, cancel)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.join().await;
            this.update_in(cx, |this, window, cx| match result {
                Some(Ok(prunable)) if prunable.oids.is_empty() => {
                    note(Toast::info(t!("lfs.prune_nothing").into_owned()), cx)
                }
                Some(Ok(prunable)) => this.confirm(
                    tn!(
                        "lfs.prune_title",
                        count = prunable.oids.len(),
                        size = size_text(prunable.bytes)
                    )
                    .into_owned(),
                    t!("lfs.prune_text").into_owned(),
                    t!("lfs.prune_button").into_owned(),
                    |this, confirmed, window, cx| {
                        if confirmed {
                            this.run_lfs_prune(window, cx);
                        }
                    },
                    window,
                    cx,
                ),
                Some(Err(err)) => {
                    this.show_git_error(t!("lfs.prune_failed").into_owned(), &err, window, cx)
                }
                None => {}
            })
            .ok();
        })
        .detach();
    }

    /// Which kinds of files take the most room in plain git, in a dialog: this
    /// branch's history, or with `everything` every branch's and tag's. Only reads; moving
    /// them into LFS (`migrate import`, a rewrite) isn't offered.
    pub(super) fn show_lfs_sizes(
        &mut self,
        everything: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let Some(cwd) = repo.workdir.clone() else {
            return;
        };
        let task = self.jobs.spawn(Lane::Latest("lfs-sizes"), move |cancel| {
            lfs_migrate_info(&git, &cwd, everything, LFS_SIZES_SHOWN, cancel)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.join().await;
            this.update_in(cx, |this, window, cx| match result {
                Some(Ok(kinds)) if kinds.is_empty() => {
                    note(Toast::info(t!("lfs.sizes_nothing").into_owned()), cx)
                }
                Some(Ok(kinds)) => {
                    let workspace = cx.entity().downgrade();
                    open_lfs_sizes(kinds, everything, workspace, window, cx)
                }
                // "git: 'lfs' is not a git command".
                Some(Err(err)) if err.stderr.contains("not a git command") => {
                    note(Toast::warning(t!("lfs.sizes_no_git_lfs").into_owned()), cx)
                }
                Some(Err(err)) => {
                    this.show_git_error(t!("lfs.sizes_failed").into_owned(), &err, window, cx)
                }
                None => {}
            })
            .ok();
        })
        .detach();
    }

    /// Moves files into Git LFS, or out of it, on the checked-out branch: asks for
    /// the patterns (`initial` filled in), then confirms the rewrite, naming the remotes
    /// that have commits it changes. Needs a branch and a clean work tree.
    pub(super) fn ask_lfs_migrate(
        &mut self,
        direction: MigrateDirection,
        initial: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let branch = match &repo.refs.head {
            // An encoded (non-UTF-8) name can't be given to git-lfs as text.
            HeadInfo::Branch { name, .. } if !name.starts_with('\0') => name.clone(),
            _ => {
                note(
                    Toast::error(t!("lfs.migrate_needs_branch").into_owned()),
                    cx,
                );
                return;
            }
        };
        let dirty = self.status.as_ref().is_some_and(|status| {
            status.staged().next().is_some()
                || status
                    .unstaged()
                    .any(|entry| entry.kind != EntryKind::Untracked)
        });
        if dirty {
            note(Toast::error(t!("lfs.migrate_dirty").into_owned()), cx);
            return;
        }
        let (title, subtitle) = match direction {
            MigrateDirection::Import => (
                t!("lfs.migrate_import_title", branch = branch),
                t!("lfs.migrate_import_hint"),
            ),
            MigrateDirection::Export => (
                t!("lfs.migrate_export_title", branch = branch),
                t!("lfs.migrate_export_hint"),
            ),
        };
        let spec = TextDialog {
            title: title.into_owned(),
            subtitle: Some(subtitle.into_owned()),
            placeholder: t!("lfs.migrate_patterns").into_owned(),
            initial,
            detail: None,
            checkbox: None,
            ok_label: t!("lfs.migrate_next").into_owned(),
            allow_empty: false,
            multiline: false,
        };
        let submit: SubmitText = Rc::new(move |this, text, _, window, cx| {
            let patterns = migrate_patterns(&text);
            if patterns.is_empty() {
                return;
            }
            this.confirm_lfs_migrate(direction, patterns, branch.clone(), window, cx);
        });
        self.open_text_dialog(spec, submit, window, cx);
    }

    fn confirm_lfs_migrate(
        &mut self,
        direction: MigrateDirection,
        patterns: Vec<String>,
        branch: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Every commit of the branch may be rewritten.
        self.check_published(
            "HEAD".to_owned(),
            None,
            move |this, published, window, cx| {
                let list = patterns.join(", ");
                let message = match direction {
                    MigrateDirection::Import => {
                        t!(
                            "lfs.migrate_import_confirm",
                            patterns = list,
                            branch = branch
                        )
                    }
                    MigrateDirection::Export => {
                        t!(
                            "lfs.migrate_export_confirm",
                            patterns = list,
                            branch = branch
                        )
                    }
                }
                .into_owned();
                let detail = if published.is_empty() {
                    t!("lfs.migrate_detail").into_owned()
                } else {
                    t!(
                        "lfs.migrate_detail_published",
                        published = published_text(&published)
                    )
                    .into_owned()
                };
                this.confirm(
                    message,
                    detail,
                    t!("lfs.migrate_rewrite").into_owned(),
                    move |this, confirmed, window, cx| {
                        if confirmed {
                            this.run_lfs_migrate(direction, patterns, branch, window, cx);
                        }
                    },
                    window,
                    cx,
                );
            },
            window,
            cx,
        );
    }

    fn run_lfs_migrate(
        &mut self,
        direction: MigrateDirection,
        patterns: Vec<String>,
        branch: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let list = patterns.join(", ");
        let (label, done) = match direction {
            MigrateDirection::Import => (
                t!("undo.op_lfs_import", patterns = list, branch = branch),
                t!("lfs.migrate_import_done", patterns = list, branch = branch),
            ),
            MigrateDirection::Export => (
                t!("undo.op_lfs_export", patterns = list, branch = branch),
                t!("lfs.migrate_export_done", patterns = list, branch = branch),
            ),
        };
        let done = done.into_owned();
        let record = Record::checked_out(label.into_owned(), Worktree::Follow);
        let full = format!("refs/heads/{branch}");
        self.run_recorded_op(
            t!("lfs.migrate_failed").into_owned(),
            None,
            Some(record),
            move |git, cwd, cancel| {
                lfs_migrate(git, cwd, direction, &patterns, &full, cancel)?;
                Ok(done)
            },
            window,
            cx,
        );
    }

    /// `git lfs prune` in the write lane; the notification says how much it freed.
    fn run_lfs_prune(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run_write_op(
            t!("lfs.prune_failed").into_owned(),
            None,
            |git, cwd, cancel| {
                let pruned = lfs_prune(git, cwd, cancel)?;
                let size = size_text(pruned.bytes);
                Ok(tn!("lfs.pruned", count = pruned.oids.len(), size = size).into_owned())
            },
            window,
            cx,
        );
    }

    /// `git lfs install --local`, then the banner is read again.
    pub(super) fn lfs_set_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let Some(cwd) = repo.workdir.clone() else {
            return;
        };
        let task = self.jobs.spawn(Lane::Write, move |cancel| {
            lfs_install_local(&git, &cwd, cancel)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.join().await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Some(Ok(())) => note(Toast::success(t!("lfs.set_up_done").into_owned()), cx),
                    Some(Err(err)) => {
                        this.show_git_error(t!("lfs.set_up_failed").into_owned(), &err, window, cx)
                    }
                    None => {}
                }
                this.load_extras(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Reads the remote's locks, then lists them in the palette (each with Unlock and Force
    /// unlock), with "Lock a file…" first.
    pub(super) fn show_lfs_locks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let Some(cwd) = repo.workdir.clone() else {
            return;
        };
        let task = self.jobs.spawn(Lane::Latest("lfs-locks"), move |cancel| {
            lfs_locks(&git, &cwd, cancel)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.join().await;
            this.update_in(cx, |this, window, cx| match result {
                Some(Ok(locks)) => this.open_locks_palette(locks, window, cx),
                Some(Err(err)) if err.kind == GitErrorKind::LfsLockingUnsupported => {
                    note(Toast::warning(t!("lfs.locks_unsupported").into_owned()), cx)
                }
                Some(Err(err)) => {
                    this.show_git_error(t!("lfs.locks_failed").into_owned(), &err, window, cx)
                }
                None => {}
            })
            .ok();
        })
        .detach();
    }

    fn open_locks_palette(
        &mut self,
        locks: Vec<LfsLock>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity().downgrade();
        let group = heading("lfs.locks_group");
        let mut commands = vec![command(
            &this,
            &group,
            t!("lfs.lock_file"),
            |w, window, cx| w.open_lfs_lock(window, cx),
        )];
        for lock in locks {
            let path = lock.path.clone();
            let when = lock
                .locked_at
                .map(crate::util::format_time)
                .unwrap_or_default();
            let detail = t!("lfs.locked_by", owner = lock.owner, when = when).into_owned();
            commands.push(
                command(
                    &this,
                    &group,
                    t!("lfs.unlock", path = lock.path.to_string()),
                    move |w, window, cx| w.run_lfs_unlock(path.clone(), window, cx),
                )
                .detail(detail.clone()),
            );
            // Someone else's lock: the server refuses a plain unlock (git-lfs also does for a
            // changed file), and breaking it takes --force.
            let (path, owner) = (lock.path.clone(), lock.owner.clone());
            commands.push(
                command(
                    &this,
                    &group,
                    t!("lfs.force_unlock", path = lock.path.to_string()),
                    move |w, window, cx| {
                        w.confirm_force_unlock(path.clone(), owner.clone(), window, cx)
                    },
                )
                .detail(detail),
            );
        }
        crate::palette::open(commands, window, cx);
    }

    /// Asks for a path, then locks it.
    fn open_lfs_lock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let spec = TextDialog {
            title: t!("lfs.lock_title").into_owned(),
            subtitle: None,
            placeholder: t!("lfs.lock_path").into_owned(),
            initial: String::new(),
            detail: None,
            checkbox: None,
            ok_label: t!("lfs.lock").into_owned(),
            allow_empty: false,
            multiline: false,
        };
        let submit: SubmitText = Rc::new(|this, path, _, window, cx| {
            let path = BString::from(path.trim());
            this.run_lfs_lock_op(path, true, false, window, cx)
        });
        self.open_text_dialog(spec, submit, window, cx);
    }

    fn run_lfs_unlock(&mut self, path: BString, window: &mut Window, cx: &mut Context<Self>) {
        self.run_lfs_lock_op(path, false, false, window, cx);
    }

    /// Asks before breaking a lock (`owner` holds it, as the server names them).
    fn confirm_force_unlock(
        &mut self,
        path: BString,
        owner: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let shown = path.to_string();
        let force: ChoiceAction = Rc::new(move |this, window, cx| {
            this.run_lfs_lock_op(path.clone(), false, true, window, cx)
        });
        self.open_choice(
            t!("lfs.force_title", path = shown).into_owned(),
            t!("lfs.force_text", owner = owner).into_owned(),
            t!("toolbar.cancel").into_owned(),
            vec![(t!("lfs.force_button").into_owned(), force)],
            window,
            cx,
        );
    }

    /// Locks (`lock`) or unlocks `path` on the remote; `force` breaks someone else's lock.
    pub(super) fn run_lfs_lock_op(
        &mut self,
        path: BString,
        lock: bool,
        force: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let Some(cwd) = repo.workdir.clone() else {
            return;
        };
        let shown = path.to_string();
        let retry = path.clone();
        let task = self.jobs.spawn(Lane::Write, move |cancel| {
            if lock {
                lfs_lock(&git, &cwd, &path, cancel).map(|_| ())
            } else {
                lfs_unlock(&git, &cwd, &path, force, cancel)
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.join().await;
            this.update_in(cx, |this, window, cx| match result {
                Some(Ok(())) => {
                    let message = if lock {
                        t!("lfs.locked", path = shown)
                    } else {
                        t!("lfs.unlocked", path = shown)
                    };
                    note(Toast::success(message.into_owned()), cx);
                }
                Some(Err(err)) if err.kind == GitErrorKind::LfsLockingUnsupported => {
                    note(Toast::warning(t!("lfs.locks_unsupported").into_owned()), cx)
                }
                // git-lfs keeps the lock of a file with uncommitted changes unless forced:
                // ask, rather than leave the user with its refusal.
                Some(Err(err)) if !lock && !force && err.stderr.contains("uncommitted changes") => {
                    let unlock: ChoiceAction = Rc::new(move |this, window, cx| {
                        this.run_lfs_lock_op(retry.clone(), false, true, window, cx)
                    });
                    this.open_choice(
                        t!("lfs.unlock_changed_title", path = shown).into_owned(),
                        t!("lfs.unlock_changed_text").into_owned(),
                        t!("toolbar.cancel").into_owned(),
                        vec![(t!("lfs.force_button").into_owned(), unlock)],
                        window,
                        cx,
                    );
                }
                Some(Err(err)) => {
                    this.show_git_error(t!("lfs.locks_failed").into_owned(), &err, window, cx)
                }
                None => {}
            })
            .ok();
        })
        .detach();
    }
}

/// How many kinds of files the LFS size report lists.
const LFS_SIZES_SHOWN: usize = 20;

/// The report of [`Workspace::show_lfs_sizes`]: each kind of file, how many versions and
/// their size, largest first.
fn open_lfs_sizes(
    kinds: Vec<LfsTypeSize>,
    everything: bool,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let look = cx.look().clone();
    let (p, m) = (look.palette, look.metrics);
    let title = if everything {
        t!("lfs.sizes_title_all")
    } else {
        t!("lfs.sizes_title_branch")
    }
    .into_owned();
    let hint = t!("lfs.sizes_hint").into_owned();
    let dismiss = t!("lfs.sizes_close").into_owned();
    window.open_dialog(cx, move |dialog, window, _| {
        let (kinds, hint, dismiss) = (kinds.clone(), hint.clone(), dismiss.clone());
        let workspace = workspace.clone();
        dialog
            .bg(p.panel)
            .width((px(560.) * m.zoom).min(window.viewport_size().width * 0.9))
            .title(title.clone())
            .content(move |content, _, _| {
                let rows = kinds.iter().enumerate().map(|(ix, kind)| {
                    h_flex()
                        .debug_selector(move || format!("lfs-size-{ix}"))
                        .gap_3()
                        .child(
                            div()
                                .flex_1()
                                .font_family(mono_font())
                                .child(kind.pattern.clone()),
                        )
                        .child(
                            div()
                                .text_color(p.fg3)
                                .child(tn!("lfs.sizes_versions", count = kind.files).into_owned()),
                        )
                        .child(
                            div()
                                .min_w(px(72.) * m.zoom)
                                .text_right()
                                .child(size_text(kind.bytes)),
                        )
                        // Straight to moving that kind into LFS on this branch.
                        .child({
                            let (workspace, pattern) = (workspace.clone(), kind.pattern.clone());
                            Button::new(("lfs-move", ix))
                                .debug_selector(move || format!("lfs-move-{ix}"))
                                .xsmall()
                                .ghost()
                                .label(t!("lfs.migrate_row").into_owned())
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let pattern = pattern.clone();
                                    workspace
                                        .update(cx, |w, cx| {
                                            w.ask_lfs_migrate(
                                                MigrateDirection::Import,
                                                pattern,
                                                window,
                                                cx,
                                            )
                                        })
                                        .ok();
                                })
                        })
                });
                content.child(
                    v_flex().gap_1().children(rows).child(
                        div()
                            .pt_2()
                            .text_size(m.text.small)
                            .text_color(p.fg3)
                            .child(hint.clone()),
                    ),
                )
            })
            .footer(
                DialogFooter::new()
                    .child(DialogClose::new().trigger(move |button| button.label(dismiss))),
            )
    });
}
