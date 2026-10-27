//! Getting lost work back: the dangling commits `git fsck` finds, and the Trash page
//! (what discards, cleans and hard resets moved aside).

use std::time::Duration;

use gitweft_core::trash::{self as trash_store, TrashEntry};
use gitweft_git::read::dangling_commits;
use gitweft_git::stash::stash_apply_commit;

use crate::dangling_view::{DanglingEvent, DanglingView};
use crate::trash_view::{TrashEvent, TrashView};

use super::*;

/// Dangling commits listed at most (the newest); the count covers them all.
const DANGLING_LIMIT: usize = 500;

impl Workspace {
    pub(super) fn close_dangling(&mut self) {
        if let Some(cancel) = self.dangling_cancel.take() {
            cancel.cancel();
        }
        self.dangling_view = None;
        self._dangling_events = None;
    }

    /// Shows the dangling commits in the centre; `git fsck` runs in a job that the view
    /// can stop.
    pub(super) fn open_dangling(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.state, RepoState::Loaded(_)) {
            return;
        }
        let view = cx.new(DanglingView::new);
        self.close_centre();
        self._dangling_events = Some(cx.subscribe(&view, Self::on_dangling_event));
        self.dangling_view = Some(view);
        cx.notify();
        self.load_dangling(cx);
    }

    fn load_dangling(&mut self, cx: &mut Context<Self>) {
        let (RepoState::Loaded(repo), Some(view)) = (&self.state, &self.dangling_view) else {
            return;
        };
        let git = repo.git.clone();
        let cwd = repo.cwd();
        let limit = if view.read(cx).shows_all() {
            gitweft_git::read::NO_LIMIT
        } else {
            DANGLING_LIMIT
        };
        let job = self.jobs.spawn(Lane::Latest("dangling"), move |cancel| {
            dangling_commits(&git, &cwd, limit, cancel).map_err(|err| err.to_string())
        });
        self.dangling_cancel = Some(job.cancel_token().clone());
        let view = view.downgrade();
        cx.spawn(async move |this, cx| {
            let result = job.join().await;
            this.update(cx, |this, _| this.dangling_cancel = None).ok();
            match result {
                Some(result) => view.update(cx, |view, cx| view.set_result(result, cx)).ok(),
                // Stopped: nothing more to show.
                None => view
                    .update(cx, |view, cx| {
                        view.set_result(Err(t!("dangling.stopped").into_owned()), cx)
                    })
                    .ok(),
            };
        })
        .detach();
    }

    fn on_dangling_event(
        &mut self,
        _: Entity<DanglingView>,
        event: &DanglingEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            DanglingEvent::Close => {
                self.close_dangling();
                cx.notify();
            }
            DanglingEvent::Cancel => {
                if let Some(cancel) = self.dangling_cancel.take() {
                    cancel.cancel();
                }
            }
            DanglingEvent::Refresh => {
                if let Some(view) = &self.dangling_view {
                    view.update(cx, |view, cx| view.set_loading(cx));
                }
                self.load_dangling(cx);
            }
            DanglingEvent::Select(id) => self.show_details(*id, cx),
            DanglingEvent::CreateBranch(id) => {
                let start = (id.to_string(), id.to_hex_with_len(8).to_string());
                self.with_window(
                    move |this, window, cx| this.open_new_branch(Some(start.clone()), window, cx),
                    cx,
                );
            }
            DanglingEvent::Checkout(id) => {
                let op = BranchOp::Detach { commit: *id };
                self.with_window(
                    move |this, window, cx| this.run_branch_op(op.clone(), window, cx),
                    cx,
                );
            }
            DanglingEvent::CherryPick(id, summary) => {
                let commit = RowCommit {
                    id: *id,
                    merge: false,
                    summary: summary.clone(),
                };
                self.with_window(
                    move |this, window, cx| this.pick_commit(commit.clone(), false, window, cx),
                    cx,
                );
            }
            DanglingEvent::ApplyStash(id) => {
                let id = *id;
                self.with_window(
                    move |this, window, cx| this.apply_dangling_stash(id, window, cx),
                    cx,
                );
            }
            DanglingEvent::BrowseFiles(id) => self.open_tree(*id, cx),
        }
    }

    /// Applies a dropped stash's changes (`git stash apply <id>`), as a stash's Apply does.
    fn apply_dangling_stash(&mut self, id: ObjectId, window: &mut Window, cx: &mut Context<Self>) {
        let conflict = Some((
            t!("stash.conflict_title").into_owned(),
            t!("stash.conflict_text").into_owned(),
        ));
        self.run_write_op(
            t!("stash.failed").into_owned(),
            conflict,
            move |git, cwd, cancel| {
                stash_apply_commit(git, cwd, id, cancel)?;
                Ok(t!("dangling.stash_applied", sha = id.to_hex_with_len(8)).into_owned())
            },
            window,
            cx,
        );
    }

    pub(super) fn close_trash(&mut self) {
        self.trash_view = None;
        self._trash_events = None;
    }

    /// Applies the trash's optional limits (Settings → General; off by default) in the
    /// background: entries older than the age limit, and the oldest while the trash is
    /// over the size limit, are deleted. Run after each load.
    pub(super) fn enforce_trash_limits(&mut self, cx: &mut Context<Self>) {
        let general = &SettingsStore::settings(cx).general;
        let max_age = (general.trash_max_age_days > 0)
            .then(|| Duration::from_secs(u64::from(general.trash_max_age_days) * 86_400));
        let max_bytes = (general.trash_max_size_mb > 0)
            .then(|| u64::from(general.trash_max_size_mb) * 1024 * 1024);
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        if max_age.is_none() && max_bytes.is_none() {
            return;
        }
        let git = repo.git.clone();
        let job = self.jobs.spawn(Lane::Coalesce("trash-limits"), move |_| {
            trash_store::enforce_limits(&git, &trash_dir(), max_age, max_bytes)
        });
        cx.spawn(async move |this, cx| {
            if let Some(Ok(removed)) = job.join().await
                && removed > 0
            {
                // An open Trash page shows what's left.
                this.update(cx, |this, cx| this.load_trash(cx)).ok();
            }
        })
        .detach();
    }

    /// Shows this repository's trash in the centre.
    pub(super) fn open_trash(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.state, RepoState::Loaded(_)) {
            return;
        }
        let view = cx.new(TrashView::new);
        self.close_centre();
        self._trash_events = Some(cx.subscribe_in(&view, window, Self::on_trash_event));
        self.trash_view = Some(view);
        cx.notify();
        self.load_trash(cx);
    }

    /// (Re)reads the entries into the open Trash page.
    pub(super) fn load_trash(&mut self, cx: &mut Context<Self>) {
        let (RepoState::Loaded(repo), Some(view)) = (&self.state, &self.trash_view) else {
            return;
        };
        // Only a working tree has files to discard.
        let workdir = repo.workdir.clone();
        let git = repo.git.clone();
        let job = self
            .jobs
            .spawn(Lane::Latest("trash"), move |cancel| match workdir {
                Some(root) => {
                    let mut entries = trash_store::list_for_repo(&trash_dir(), &root);
                    // Entries from before refs kept their content from `git gc` get one.
                    trash_store::protect_all(&git, &entries, cancel);
                    // Folders from before records don't say where they came from, so every
                    // repository's page lists them, last.
                    let legacy = trash_store::list(&trash_dir())
                        .into_iter()
                        .filter(|entry| entry.legacy);
                    entries.extend(legacy);
                    entries
                }
                None => Vec::new(),
            });
        let view = view.downgrade();
        cx.spawn(async move |_, cx| {
            if let Some(entries) = job.join().await {
                view.update(cx, |view, cx| view.set_result(Ok(entries), cx))
                    .ok();
            }
        })
        .detach();
    }

    fn on_trash_event(
        &mut self,
        _: &Entity<TrashView>,
        event: &TrashEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            TrashEvent::Close => {
                self.close_trash();
                cx.notify();
            }
            TrashEvent::Restore { entry, paths } => {
                self.restore_from_trash(entry.clone(), paths.clone(), window, cx)
            }
            TrashEvent::Delete(entry) => {
                let message = t!("trash.delete_confirm").into_owned();
                let detail = t!("trash.delete_detail").into_owned();
                self.confirm_trash_delete(message, detail, vec![entry.clone()], window, cx)
            }
            TrashEvent::Empty(entries) => {
                let message = tn!("trash.empty_confirm", count = entries.len()).into_owned();
                let detail = t!("trash.delete_detail").into_owned();
                self.confirm_trash_delete(message, detail, entries.clone(), window, cx)
            }
            TrashEvent::DeleteOlder(entries) => {
                let message = tn!("trash.delete_older_confirm", count = entries.len()).into_owned();
                let detail = t!("trash.delete_older_detail").into_owned();
                self.confirm_trash_delete(message, detail, entries.clone(), window, cx)
            }
            TrashEvent::ShowFolder(path) => {
                self.open_in(external::Program::FileManager, path.clone(), window, cx)
            }
        }
    }

    /// Asks, then deletes the entries for good and reads the trash again.
    fn confirm_trash_delete(
        &mut self,
        message: String,
        detail: String,
        entries: Vec<TrashEntry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let answer = window.prompt(
            PromptLevel::Warning,
            &message,
            Some(&detail),
            &[
                PromptButton::ok(t!("trash.delete").into_owned()),
                PromptButton::cancel(t!("toolbar.cancel").into_owned()),
            ],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let Ok(Some(job)) = this.update(cx, |this, _| {
                let RepoState::Loaded(repo) = &this.state else {
                    return None;
                };
                let git = repo.git.clone();
                Some(this.jobs.spawn(Lane::Write, move |_| {
                    entries
                        .iter()
                        .map(|entry| trash_store::delete(&git, entry))
                        .find_map(Result::err)
                        .map_or(Ok(()), Err)
                }))
            }) else {
                return;
            };
            let result = job.join().await;
            this.update_in(cx, |this, _, cx| {
                if let Some(Err(err)) = result {
                    note(
                        Toast::error(err.to_string()).title(t!("trash.delete_failed").into_owned()),
                        cx,
                    );
                }
                this.load_trash(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Puts files back from the trash (never over a file changed since), then says what
    /// came back and what didn't.
    fn restore_from_trash(
        &mut self,
        entry: TrashEntry,
        paths: Vec<BString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let dir = entry.dir.clone();
        let job = self.jobs.spawn(Lane::Write, move |cancel| {
            trash_store::restore(&git, &entry, &paths, cancel).map_err(|err| trash_error_text(&err))
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = job.join().await;
            this.update_in(cx, |this, window, cx| {
                this.report_restore(result, dir, true, window, cx)
            })
            .ok();
        })
        .detach();
    }

    /// Files a file stands in the way of (where a folder is needed): asks, then moves what
    /// is in the way to the trash and puts the files back.
    fn ask_move_aside(
        &mut self,
        dir: PathBuf,
        blocked: Vec<(BString, BString)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut blockers: Vec<BString> =
            blocked.iter().map(|(_, blocker)| blocker.clone()).collect();
        blockers.sort();
        blockers.dedup();
        let paths: Vec<BString> = blocked.into_iter().map(|(path, _)| path).collect();
        let shown: Vec<String> = blockers.iter().map(ToString::to_string).collect();
        let message = tn!(
            "trash.blocked_confirm",
            count = paths.len(),
            path = crate::util::join_list(&shown)
        )
        .into_owned();
        self.ask(
            PromptLevel::Info,
            message,
            t!("trash.blocked_detail").into_owned(),
            t!("trash.move_and_restore").into_owned(),
            move |this, confirmed, window, cx| {
                if confirmed {
                    this.restore_moving_aside(dir, paths, blockers, window, cx);
                }
            },
            window,
            cx,
        );
    }

    /// Moves `blockers` to the trash, then puts `paths` of the entry in `dir` back.
    fn restore_moving_aside(
        &mut self,
        dir: PathBuf,
        paths: Vec<BString>,
        blockers: Vec<BString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let (git, Some(root)) = (repo.git.clone(), repo.workdir.clone()) else {
            return;
        };
        let entry_dir = dir.clone();
        let job = self.jobs.spawn(Lane::Write, move |cancel| {
            trash_store::move_aside(&trash_dir(), &root, &blockers)
                .map_err(|err| err.to_string())?;
            let entry = trash_store::read_entry(&entry_dir);
            trash_store::restore(&git, &entry, &paths, cancel).map_err(|err| trash_error_text(&err))
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = job.join().await;
            this.update_in(cx, |this, window, cx| {
                this.report_restore(result, dir, false, window, cx)
            })
            .ok();
        })
        .detach();
    }

    /// Says what came back from the trash and what didn't. Files blocked by a file where
    /// a folder is needed get the question whether to move it aside (`ask`), or, asked
    /// already, a note.
    fn report_restore(
        &mut self,
        result: Option<Result<trash_store::RestoreReport, String>>,
        dir: PathBuf,
        ask: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Some(Ok(report)) => {
                let restored = match report.restored.as_slice() {
                    [] => None,
                    [path] => Some(t!("trash.restored_single", path = path.to_string())),
                    many => Some(tn!("trash.restored_multiple", count = many.len())),
                };
                if let Some(text) = restored {
                    note(Toast::success(text.into_owned()), cx);
                }
                if !report.conflicts.is_empty() {
                    let paths: Vec<String> =
                        report.conflicts.iter().map(ToString::to_string).collect();
                    let text = t!("trash.conflicts", paths = crate::util::join_list(&paths));
                    note(Toast::warning(text.into_owned()), cx);
                }
                for (path, failure) in &report.failed {
                    let why = match failure {
                        trash_store::RestoreFailure::NotRelative => t!("trash.not_relative"),
                        trash_store::RestoreFailure::GoneFromTrash => t!("trash.gone_from_trash"),
                        trash_store::RestoreFailure::Other(message) => message.into(),
                    };
                    let text = t!(
                        "trash.restore_failed_file",
                        path = path.to_string(),
                        why = why
                    );
                    note(
                        Toast::error(text.into_owned())
                            .title(t!("trash.restore_failed").into_owned()),
                        cx,
                    );
                }
                if ask && !report.blocked.is_empty() {
                    self.ask_move_aside(dir, report.blocked, window, cx);
                } else {
                    for (path, blocker) in &report.blocked {
                        let text = t!(
                            "trash.blocked_still",
                            path = path.to_string(),
                            blocker = blocker.to_string()
                        );
                        note(Toast::warning(text.into_owned()), cx);
                    }
                }
            }
            Some(Err(err)) => note(
                Toast::error(err).title(t!("trash.restore_failed").into_owned()),
                cx,
            ),
            None => {}
        }
        self.load_trash(cx);
        self.refresh_status(cx);
    }
}

/// A trash error in the user's language: core's own cases are worded here; git's and
/// the system's messages stay as they are.
fn trash_error_text(err: &trash_store::TrashError) -> String {
    match err {
        trash_store::TrashError::UnknownRepository => t!("trash.unknown_repository").into_owned(),
        other => other.to_string(),
    }
}
