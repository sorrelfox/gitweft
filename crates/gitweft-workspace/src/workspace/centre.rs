//! The centre views: the diff, history, rebase editor, compare, reflog, file tree and files, blame; opening, closing and their events.

use super::*;

impl Workspace {
    /// Shows a file diff in the centre panel; the diff loads in the background.
    pub(super) fn open_diff(&mut self, mut params: DiffParams, cx: &mut Context<Self>) {
        if !matches!(self.state, RepoState::Loaded(_)) {
            return;
        }
        self.close_blame();
        // Only a diff opened from the review is a review diff (the caller says so after).
        self.review_diff = false;
        params.options.ignore_whitespace = self.diff_ignore_whitespace;
        let layout = self.diff_layout;
        let view = cx.new(|cx| {
            DiffView::new(
                params.title.clone(),
                params.subtitle.clone(),
                params.source.clone(),
                params.options,
                layout,
                cx,
            )
        });
        self._diff_events = Some(cx.subscribe(&view, Self::on_diff_event));
        self.diff_view = Some(view);
        self.diff_params = Some(params);
        cx.notify();
        self.reload_diff(cx);
    }

    /// (Re)loads the open diff in the background into the same view, so it keeps its
    /// scroll position after staging parts of it.
    pub(super) fn reload_diff(&mut self, cx: &mut Context<Self>) {
        let (RepoState::Loaded(repo), Some(view), Some(params)) =
            (&self.state, &self.diff_view, &self.diff_params)
        else {
            return;
        };
        let git = repo.git.clone();
        let cwd = repo.cwd();
        let DiffParams {
            source,
            path,
            old_path,
            options,
            ..
        } = params.clone();
        let view = view.clone();

        // Opening another file supersedes a diff that is still loading.
        // Syntax colors and word ranges are computed on the job too (never on the UI thread).
        let dark = cx.palette().dark;
        let tab = crate::code_scroll::tab_width(cx);
        let job = self.jobs.spawn(Lane::Latest("file-diff"), move |cancel| {
            file_diff(
                &git,
                &cwd,
                &source,
                &path,
                old_path.as_ref(),
                options,
                cancel,
            )
            .map(|diff| {
                // A block can leave one file of a diff and arrive in another, so the
                // diff's other files are read too: without context lines, and not at all
                // when the whole diff is too big to hold as text.
                let others = gitweft_git::read::whole_diff(&git, &cwd, &source, options, cancel)
                    .map(|files| {
                        let lines: usize = files.iter().map(|f| f.line_count()).sum();
                        if lines > crate::diff_decor::MAX_ACROSS_LINES {
                            return Vec::new();
                        }
                        files.into_iter().filter(|f| f.path != diff.path).collect()
                    })
                    .unwrap_or_default();
                let mut decorations = decorate(&diff, &others, dark, tab);
                // A binary image is shown as two pictures; both versions are read here too.
                // An image kept in Git LFS diffs as its pointers: its pictures come from the
                // local LFS store, when every side is there (never fetched; else the pointer
                // panel stays).
                let lfs = gitweft_git::lfs::pointer_sides(&diff).is_some();
                if (diff.binary || lfs) && crate::diff_view::image_format(&path).is_some() {
                    let max = crate::diff_view::MAX_IMAGE_BYTES;
                    let smudged = |side: Option<Vec<u8>>| -> Option<Option<Vec<u8>>> {
                        let Some(bytes) = side else {
                            return Some(None);
                        };
                        match gitweft_git::lfs::parse_pointer(&bytes) {
                            Some(pointer) => {
                                gitweft_git::lfs::local_object(&git, &cwd, &pointer, max, cancel)
                                    .ok()
                                    .flatten()
                                    .map(Some)
                            }
                            None => Some(Some(bytes)),
                        }
                    };
                    decorations.images = gitweft_git::read::file_versions(
                        &git,
                        &cwd,
                        &source,
                        &path,
                        old_path.as_ref(),
                        max,
                        cancel,
                    )
                    .ok()
                    .and_then(|versions| {
                        let (old, new) = if lfs {
                            (smudged(versions.old)?, smudged(versions.new)?)
                        } else {
                            (versions.old, versions.new)
                        };
                        Some(crate::diff_decor::ImageSides {
                            path: path.clone(),
                            old,
                            new,
                        })
                    });
                }
                (diff, decorations)
            })
            // A partial clone's missing contents, which only the user may fetch.
            .map_err(|e| {
                (
                    gitweft_git::partial::is_missing_objects(e.as_ref()),
                    e.to_string(),
                )
            })
        });
        cx.spawn(async move |_, cx| {
            if let Some(result) = job.join().await {
                view.update(cx, |view, cx| match result {
                    Err((true, _)) => view.set_missing(cx),
                    result => view.set_diff(result.map_err(|(_, message)| message), cx),
                });
            }
        })
        .detach();
    }

    /// "Fetch missing contents" on a file a partial clone couldn't show: that version,
    /// then the file view again.
    fn fetch_missing_file(&mut self, commit: ObjectId, path: BString, cx: &mut Context<Self>) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let (git, cwd) = (repo.git.clone(), repo.cwd());
        let files = vec![(commit.to_string(), path.clone())];
        let job = self.jobs.spawn(Lane::Write, move |cancel| {
            gitweft_git::partial::fetch_missing(&git, &cwd, &files, cancel)
        });
        cx.spawn(async move |this, cx| {
            let result = job.join().await;
            this.update(cx, |this, cx| match result {
                Some(Ok(_)) => this.open_file(commit, path, cx),
                Some(Err(err)) => {
                    let text = t!("diff.fetch_missing_failed", error = err.summary()).into_owned();
                    crate::tab_notes::note_failure(text, cx);
                }
                None => {}
            })
            .ok();
        })
        .detach();
    }

    /// "Fetch missing contents" on a blame a partial clone couldn't finish: every
    /// version of the file along the history blamed, then the blame again.
    fn fetch_missing_blame(
        &mut self,
        path: BString,
        rev: Option<ObjectId>,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let (git, cwd) = (repo.git.clone(), repo.cwd());
        let from = rev.map_or_else(|| "HEAD".to_owned(), |rev| rev.to_string());
        let history_path = path.clone();
        let job = self.jobs.spawn(Lane::Write, move |cancel| {
            gitweft_git::partial::fetch_history(&git, &cwd, &from, history_path.as_ref(), cancel)
        });
        cx.spawn(async move |this, cx| {
            let result = job.join().await;
            this.update(cx, |this, cx| match result {
                Some(Ok(_)) => this.open_blame_at(path, rev, None, cx),
                Some(Err(err)) => {
                    let text = t!("diff.fetch_missing_failed", error = err.summary()).into_owned();
                    crate::tab_notes::note_failure(text, cx);
                }
                None => {}
            })
            .ok();
        })
        .detach();
    }

    /// "Fetch missing contents" on a diff a partial clone couldn't show: downloads the
    /// blobs of its two versions from the promisor remote, then reads it again. The only
    /// place such a download happens, and only on request.
    fn fetch_missing_for_diff(&mut self, cx: &mut Context<Self>) {
        let (RepoState::Loaded(repo), Some(params)) = (&self.state, &self.diff_params) else {
            return;
        };
        let (git, cwd) = (repo.git.clone(), repo.cwd());
        let old = params
            .old_path
            .clone()
            .unwrap_or_else(|| params.path.clone());
        let files: Vec<(String, BString)> = match &params.source {
            DiffSource::Commit { parent, commit } => {
                std::iter::once((commit.to_string(), params.path.clone()))
                    .chain(parent.map(|parent| (parent.to_string(), old)))
                    .collect()
            }
            DiffSource::Staged => vec![("HEAD".to_owned(), old)],
            DiffSource::Unstaged | DiffSource::Untracked => Vec::new(),
        };
        let job = self.jobs.spawn(Lane::Write, move |cancel| {
            gitweft_git::partial::fetch_missing(&git, &cwd, &files, cancel)
        });
        cx.spawn(async move |this, cx| {
            let result = job.join().await;
            this.update(cx, |this, cx| match result {
                Some(Ok(_)) => this.reload_diff(cx),
                Some(Err(err)) => {
                    let text = t!("diff.fetch_missing_failed", error = err.summary()).into_owned();
                    crate::tab_notes::note_failure(text, cx);
                }
                None => {}
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn on_diff_event(
        &mut self,
        _: Entity<DiffView>,
        event: &DiffEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            DiffEvent::FetchMissing => self.fetch_missing_for_diff(cx),
            DiffEvent::Close => {
                self.close_diff();
                self.refocus_centre(cx);
                cx.notify();
            }
            DiffEvent::Apply {
                action,
                patch,
                path,
            } => self.apply_diff_patch(*action, patch.clone(), path.clone(), cx),
            DiffEvent::Reload(options) => {
                self.diff_ignore_whitespace = options.ignore_whitespace;
                let ignore = options.ignore_whitespace;
                SettingsStore::update(cx, |settings| settings.diff.ignore_whitespace = ignore);
                if let Some(params) = &mut self.diff_params {
                    params.options = *options;
                }
                self.reload_diff(cx);
            }
            DiffEvent::LayoutChanged(layout) => {
                self.diff_layout = *layout;
                let layout = settings_layout(*layout);
                SettingsStore::update(cx, |settings| settings.diff.layout = layout);
            }
            DiffEvent::Comment { side, line, anchor } => {
                let Some(params) = &self.diff_params else {
                    return;
                };
                let path = review::path_key(&params.path);
                let (side, line, anchor) = (*side, *line, anchor.clone());
                self.with_window(
                    move |this, window, cx| {
                        this.write_comment(path, side, line, anchor, window, cx)
                    },
                    cx,
                );
            }
            DiffEvent::OpenComment(id) => {
                let id = *id;
                self.with_window(
                    move |this, window, cx| this.open_comment(id, window, cx),
                    cx,
                );
            }
            DiffEvent::WholeFile { action, path } => {
                let entry = self
                    .status
                    .as_ref()
                    .and_then(|status| status.entries.iter().find(|e| e.path == *path))
                    .cloned();
                if let Some(entry) = entry {
                    let event = match action {
                        PatchAction::Stage => StagingEvent::Stage(vec![entry]),
                        PatchAction::Unstage => StagingEvent::Unstage(vec![entry]),
                        PatchAction::Discard => StagingEvent::Discard(vec![entry]),
                    };
                    let staging = self.staging_view.clone();
                    self.on_staging_event(staging, &event, cx);
                }
                // A discarded new file is gone. An unstaged one comes back untracked, and
                // its diff follows it there once the status is read (`follow_moved_file`).
                if *action == PatchAction::Discard {
                    self.close_diff();
                }
                cx.notify();
            }
        }
    }

    /// Applies a hunk/line patch from the diff view (write lane), then refreshes the
    /// status and reloads the diff. Discards back up the file first.
    pub(super) fn apply_diff_patch(
        &mut self,
        action: PatchAction,
        patch: Vec<u8>,
        path: BString,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let Some(workdir) = repo.workdir.clone() else {
            return;
        };
        let git = repo.git.clone();
        let job = self.jobs.spawn(Lane::Write, move |cancel| {
            let backup = if action == PatchAction::Discard {
                backup_worktree_file(&git, &workdir, &path, cancel)
                    .map_err(|e| e.to_string())?
                    .map(|blob| DiscardBackup {
                        blobs: vec![(path.clone(), blob)],
                        trash_dir: None,
                    })
            } else {
                None
            };
            let (cached, reverse) = match action {
                PatchAction::Stage => (true, false),
                PatchAction::Unstage => (true, true),
                PatchAction::Discard => (false, true),
            };
            apply_patch(&git, &workdir, patch, cached, reverse, cancel)
                .map_err(|e| e.to_string())?;
            // The Trash page lists what discarding some lines threw away too. If that fails
            // the lines are still discarded (their content is saved), and the user hears of it.
            let recorded = backup.as_ref().and_then(|backup| {
                gitweft_git::ops::record_discarded_blobs(
                    &git,
                    &workdir,
                    &backup.blobs,
                    &trash_dir(),
                )
                .err()
                .map(|err| err.to_string())
            });
            Ok::<_, String>((backup, recorded))
        });
        cx.spawn(async move |this, cx| {
            let result = job.join().await;
            this.update(cx, |this, cx| {
                match result {
                    // The backup went to the trash, which the Trash page lists.
                    Some(Ok((_, Some(err)))) => {
                        let text = t!("trash.record_failed", error = err).into_owned();
                        crate::tab_notes::note_failure(text, cx);
                    }
                    Some(Ok((_, None))) | None => {}
                    Some(Err(err)) => {
                        let text = t!("patch.apply_failed", error = err).into_owned();
                        crate::tab_notes::note_failure(text, cx);
                    }
                }
                this.refresh_status(cx);
                this.reload_diff(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Debug builds only: `GITWEFT_DEBUG_OPEN_DIFF=<repo-relative path>` opens that file's
    /// diff once the status is known, so screenshot-driven UI checks can reach the diff
    /// view without mouse input.
    #[cfg(debug_assertions)]
    pub(super) fn debug_open_diff(&mut self, cx: &mut Context<Self>) {
        // `GITWEFT_DEBUG_OPEN_CONFLICT=<path>` opens the conflict tool for that file.
        if let Some(path) = std::env::var_os("GITWEFT_DEBUG_OPEN_CONFLICT") {
            let path = BString::from(path.as_encoded_bytes());
            self.with_window(
                move |this, window, cx| this.open_conflict(path, window, cx),
                cx,
            );
        }
        let Some(path) = std::env::var_os("GITWEFT_DEBUG_OPEN_DIFF") else {
            return;
        };
        let entry = self.status.as_ref().and_then(|status| {
            status
                .entries
                .iter()
                .find(|e| e.path == path.as_encoded_bytes())
                .cloned()
        });
        if let Some(entry) = entry {
            let staged = entry.worktree.is_none() && entry.kind == EntryKind::Tracked;
            self.open_status_diff(&entry, staged, cx);
        }
    }

    pub(super) fn close_diff(&mut self) {
        self.diff_view = None;
        self._diff_events = None;
        self.diff_params = None;
        self.review_diff = false;
        self.review_since = false;
    }

    /// A review diff follows the branch: once the comparison has moved on, it reloads to
    /// show the new version, so re-anchored comments land on the right lines.
    pub(super) fn follow_review_range(&mut self, cx: &mut Context<Self>) {
        if !self.review_diff || self.review_since {
            return;
        }
        let Some((parent, commit)) = self
            .compare_view
            .as_ref()
            .and_then(|compare| compare.read(cx).range())
        else {
            return;
        };
        let Some(params) = &mut self.diff_params else {
            return;
        };
        let source = DiffSource::Commit {
            parent: Some(parent),
            commit,
        };
        if params.source != source {
            params.source = source;
            self.reload_diff(cx);
        }
    }

    /// Shows the review's comments in the open diff, when it was opened from the review.
    pub(super) fn push_review_comments(&mut self, cx: &mut Context<Self>) {
        let (Some(diff), Some(params)) = (&self.diff_view, &self.diff_params) else {
            return;
        };
        let comments = self
            .compare_view
            .as_ref()
            .filter(|_| self.review_diff)
            .and_then(|compare| {
                let compare = compare.read(cx);
                let path = review::path_key(&params.path);
                compare
                    .review()
                    .is_some()
                    .then(|| compare.comments_for(&path))
            });
        let new_only = self.review_since;
        diff.update(cx, |diff, cx| diff.set_comments(comments, new_only, cx));
    }

    /// Review diffs: asks for a comment on a line, then adds it to the review.
    pub(super) fn write_comment(
        &mut self,
        path: String,
        side: review::Side,
        line: u32,
        anchor: review::Anchor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(compare) = &self.compare_view else {
            return;
        };
        let compare = compare.downgrade();
        // The comment's author: the name a commit would record, as the commit box shows it.
        let author = self
            .staging_view
            .read(cx)
            .identity()
            .author()
            .0
            .unwrap_or_default()
            .to_owned();
        let spec = TextDialog {
            title: t!(
                "review.comment_title",
                path = review::path_shown(&path),
                line = line
            )
            .into_owned(),
            subtitle: Some(anchor.text.trim().to_owned()),
            placeholder: t!("review.comment_placeholder").into_owned(),
            initial: String::new(),
            detail: None,
            checkbox: None,
            ok_label: t!("review.add_comment").into_owned(),
            allow_empty: false,
            multiline: true,
        };
        let submit: SubmitText = Rc::new(move |this, body, _, _, cx| {
            let draft = crate::compare_view::CommentDraft {
                path: path.clone(),
                side,
                line,
                anchor: anchor.clone(),
                body: body.trim().to_owned(),
                author: author.clone(),
            };
            compare
                .update(cx, |compare, cx| compare.add_comment(draft, cx))
                .ok();
            this.push_review_comments(cx);
        });
        self.open_text_dialog(spec, submit, window, cx);
    }

    /// Review comments: asks for a comment's new text, then saves it.
    pub(super) fn edit_comment(
        &mut self,
        id: u64,
        path: String,
        line: u32,
        body: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(compare) = &self.compare_view else {
            return;
        };
        let compare = compare.downgrade();
        let spec = TextDialog {
            title: t!(
                "review.edit_title",
                path = review::path_shown(&path),
                line = line
            )
            .into_owned(),
            subtitle: None,
            placeholder: t!("review.comment_placeholder").into_owned(),
            initial: body,
            detail: None,
            checkbox: None,
            ok_label: t!("review.save").into_owned(),
            allow_empty: false,
            multiline: true,
        };
        let submit: SubmitText = Rc::new(move |this, body, _, _, cx| {
            let body = body.trim().to_owned();
            compare
                .update(cx, |compare, cx| compare.edit_comment(id, body, cx))
                .ok();
            this.push_review_comments(cx);
        });
        self.open_text_dialog(spec, submit, window, cx);
    }

    /// A comment in a review diff: its whole text, with Resolve (or Reopen), Edit and Delete.
    pub(super) fn open_comment(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(compare) = &self.compare_view else {
            return;
        };
        let Some(comment) = compare.read(cx).comment(id).cloned() else {
            return;
        };
        let title = t!(
            "review.open_title",
            path = review::path_shown(&comment.path),
            line = comment.line
        )
        .into_owned();
        let text = if comment.outdated {
            t!(
                "common.paragraphs",
                first = comment.body,
                second = t!("review.open_outdated")
            )
            .into_owned()
        } else {
            comment.body.clone()
        };
        // Who wrote it and when (comments saved by older versions have no author).
        let text = if comment.author.is_empty() {
            text
        } else {
            let when = crate::util::ago(comment.created as i64);
            let by = t!("review.comment_by", author = comment.author, when = when);
            t!("common.paragraphs", first = by, second = text).into_owned()
        };
        let weak = compare.downgrade();
        let resolved = !comment.resolved;
        let toggle: ChoiceAction = Rc::new({
            let weak = weak.clone();
            move |this, _, cx| {
                weak.update(cx, |compare, cx| compare.set_resolved(id, resolved, cx))
                    .ok();
                this.push_review_comments(cx);
            }
        });
        let edit: ChoiceAction = Rc::new({
            let (path, line, body) = (comment.path.clone(), comment.line, comment.body.clone());
            move |this, window, cx| {
                this.edit_comment(id, path.clone(), line, body.clone(), window, cx)
            }
        });
        let delete: ChoiceAction = Rc::new(move |this, _, cx| {
            weak.update(cx, |compare, cx| compare.delete_comment(id, cx))
                .ok();
            this.push_review_comments(cx);
        });
        let toggle_label = if comment.resolved {
            t!("review.reopen")
        } else {
            t!("review.resolve")
        }
        .into_owned();
        self.open_choice(
            title,
            text,
            t!("review.close").into_owned(),
            vec![
                (t!("review.delete").into_owned(), delete),
                (t!("review.edit").into_owned(), edit),
                (toggle_label, toggle),
            ],
            window,
            cx,
        );
    }

    pub(super) fn close_history(&mut self) {
        if let Some(cancel) = self.history_cancel.take() {
            cancel.cancel();
        }
        self.history_view = None;
        self._history_events = None;
    }

    /// Lists the commits that changed `path`, up to `rev` (HEAD when `None`), in the
    /// centre. Diffs and blames opened from the list show on top of it.
    pub(super) fn open_history(
        &mut self,
        path: BString,
        rev: Option<ObjectId>,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let cwd = repo.cwd();
        let subtitle = match rev {
            Some(rev) => t!("history.from_commit", sha = rev.to_hex_with_len(8)).into_owned(),
            None => t!("history.from_head").into_owned(),
        };
        let view = cx.new(|cx| HistoryView::new(path.to_string(), subtitle, cx));
        self.close_diff();
        self.close_blame();
        self.close_file();
        self.close_history();
        self._history_events = Some(cx.subscribe(&view, Self::on_history_event));
        self.history_view = Some(view.clone());
        cx.notify();

        // Opening another history cancels one still loading.
        let job = self.jobs.spawn(Lane::Latest("history"), move |cancel| {
            gitweft_git::read::file_history(&git, &cwd, rev, &path, None, cancel)
                .map_err(|err| err.to_string())
        });
        self.history_cancel = Some(job.cancel_token().clone());
        let view = view.downgrade();
        cx.spawn(async move |_, cx| {
            if let Some(result) = job.join().await {
                view.update(cx, |view, cx| view.set_result(result, cx)).ok();
            }
        })
        .detach();
    }

    /// Hands the keys back to the topmost centre view left once one on top of it closes.
    pub(super) fn refocus_centre(&self, cx: &mut Context<Self>) {
        macro_rules! refocus_top {
            ($this:expr; $($field:ident $close:ident),*) => {
                $(if let Some(view) = &$this.$field {
                    view.update(cx, |view, cx| view.refocus(cx));
                    return;
                })*
            };
        }
        centre_views!(refocus_top!(self));
    }

    /// The window's focus went to an element that isn't drawn any more (a view closed
    /// while it had the keys, and lived on a moment longer): the topmost centre view takes
    /// them, or the graph, or the workspace itself when there is no repository.
    pub(crate) fn take_lost_focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.centre_covered() {
            self.refocus_centre(cx);
            return;
        }
        let handle = match &self.graph_view {
            Some(view) => view.read(cx).focus_handle(cx),
            None => self.focus_handle().clone(),
        };
        window.focus(&handle, cx);
    }

    /// A view covers the graph in the centre (the conflict tool aside).
    pub(super) fn centre_covered(&self) -> bool {
        macro_rules! any_open {
            ($this:expr; $($field:ident $close:ident),*) => {
                false $(|| $this.$field.is_some())*
            };
        }
        centre_views!(any_open!(self))
    }

    /// Closes every view shown instead of the graph, except the conflict tool.
    pub(super) fn close_centre(&mut self) {
        macro_rules! close_all {
            ($this:expr; $($field:ident $close:ident),*) => {
                $($this.$close();)*
            };
        }
        centre_views!(close_all!(self));
    }

    /// Stops the running change search; results still on their way are dropped.
    pub(super) fn stop_change_search(&mut self) {
        if let Some(cancel) = self.pickaxe_cancel.take() {
            cancel.cancel();
        }
        self.pickaxe_generation += 1;
    }

    /// Runs a change search (`git log --all -S`/`-G`) and streams the commits it finds
    /// into the graph. A new search, Cancel or a reload stops it.
    pub(super) fn start_change_search(
        &mut self,
        query: gitweft_git::read::Pickaxe,
        cx: &mut Context<Self>,
    ) {
        self.stop_change_search();
        let (RepoState::Loaded(repo), Some(graph)) = (&self.state, &self.graph_view) else {
            return;
        };
        let git = repo.git.clone();
        let cwd = repo.cwd();
        let (tx, rx) = async_channel::unbounded::<Vec<ObjectId>>();
        let job = self.jobs.spawn(Lane::Latest("pickaxe"), move |cancel| {
            gitweft_git::read::pickaxe(&git, &cwd, &query, cancel, |ids| {
                let _ = tx.try_send(ids);
            })
            .map_err(|err| err.to_string())
        });
        self.pickaxe_cancel = Some(job.cancel_token().clone());
        let generation = self.pickaxe_generation;
        let graph = graph.downgrade();
        cx.spawn(async move |this, cx| {
            // Everything that arrived since the last frame goes in as one batch.
            while let Ok(mut ids) = rx.recv().await {
                while let Ok(more) = rx.try_recv() {
                    ids.extend(more);
                }
                let current = this
                    .update(cx, |this, _| this.pickaxe_generation == generation)
                    .unwrap_or(false);
                if !current {
                    return;
                }
                graph
                    .update(cx, |graph, cx| graph.add_change_results(ids, cx))
                    .ok();
            }
            // `None`: cancelled or superseded, which the graph already shows.
            if let Some(result) = job.join().await {
                graph
                    .update(cx, |graph, cx| {
                        graph.change_search_finished(result.err(), cx)
                    })
                    .ok();
            }
        })
        .detach();
    }

    pub(super) fn close_rebase(&mut self) {
        self.rebase_view = None;
        self._rebase_events = None;
    }

    /// Row menu: plans an interactive rebase of the commits after `commit` (with
    /// `include`, from `commit` on: from the first commit, it's a `--root` plan), then
    /// opens the editor in the centre.
    pub(super) fn open_rebase(
        &mut self,
        id: ObjectId,
        include: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let cwd = repo.cwd();
        let branch = head_label(&repo.refs.head);
        let job = self.jobs.spawn(Lane::Write, move |cancel| {
            if include {
                gitweft_git::rebase::plan_before(&git, &cwd, id, 1, cancel)
            } else {
                gitweft_git::rebase::plan(&git, &cwd, &id.to_string(), cancel)
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let Some(result) = job.join().await else {
                return;
            };
            this.update_in(cx, |this, window, cx| match result {
                Ok(plan) => {
                    let lane = this.head_lane(cx);
                    let view = cx.new(|cx| RebaseView::new(branch, plan, lane, cx));
                    this.close_centre();
                    this._rebase_events = Some(cx.subscribe(&view, Self::on_rebase_event));
                    this.rebase_view = Some(view);
                    cx.notify();
                }
                Err(err) => this.plan_failed(err, true, window, cx),
            })
            .ok();
        })
        .detach();
    }

    /// Why an interactive rebase couldn't be planned. Merges among the commits: with
    /// `offer`, the commits after the newest merge can be opened in the editor instead (the
    /// graph's quick actions don't offer it: their commit may come before the merge).
    pub(super) fn plan_failed(
        &mut self,
        err: GitError,
        offer: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let failed = |text: String| Toast::error(text).title(t!("rebase.plan_failed").into_owned());
        match err.kind {
            GitErrorKind::MergesInRange { count, after } => {
                let title = tn!("rebase.merges_title", count = count).into_owned();
                match after {
                    Some(merge) if offer => {
                        let open: ChoiceAction = Rc::new(move |this, window, cx| {
                            this.open_rebase(merge, false, window, cx)
                        });
                        let sha = merge.to_hex_with_len(8).to_string();
                        self.open_choice(
                            title,
                            t!("rebase.merges_text", sha = sha).into_owned(),
                            t!("toolbar.cancel").into_owned(),
                            vec![(t!("rebase.merges_after").into_owned(), open)],
                            window,
                            cx,
                        );
                    }
                    Some(_) => note(
                        Toast::warning(t!("rebase.merges_quick").into_owned()).title(title),
                        cx,
                    ),
                    None => note(
                        Toast::warning(t!("rebase.merges_head").into_owned()).title(title),
                        cx,
                    ),
                }
            }
            GitErrorKind::NotInHistory => {
                note(failed(t!("rebase.not_in_history").into_owned()), cx)
            }
            GitErrorKind::NothingToRebase => {
                note(failed(t!("rebase.nothing_after").into_owned()), cx)
            }
            _ => note(failed(err.summary()), cx),
        }
    }

    pub(super) fn on_rebase_event(
        &mut self,
        _: Entity<RebaseView>,
        event: &RebaseEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            RebaseEvent::Close => {
                self.close_rebase();
                cx.notify();
            }
            RebaseEvent::SelectCommit(id) => self.show_details(*id, cx),
            RebaseEvent::Start(plan) => {
                let plan = plan.clone();
                self.with_window(
                    move |this, window, cx| this.start_rebase(plan, window, cx),
                    cx,
                );
            }
        }
    }

    /// Runs the edited plan, after asking if it rewrites commits a remote has. Local
    /// changes stop it before the editor closes, so the plan isn't lost.
    pub(super) fn start_rebase(
        &mut self,
        plan: RebasePlan,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let dirty = self.status.as_ref().is_some_and(|status| {
            status.staged().next().is_some()
                || status
                    .unstaged()
                    .any(|entry| entry.kind != EntryKind::Untracked)
        });
        if dirty {
            note(
                Toast::error(t!("rebase.local_changes").into_owned())
                    .title(t!("rebase.failed").into_owned()),
                cx,
            );
            return;
        }
        let branch = head_label(&repo.refs.head);
        // From the first commit on, every commit is rewritten.
        let base = plan.base.map(|base| base.to_string());
        self.check_published(
            "HEAD".to_owned(),
            base,
            move |this, published, window, cx| {
                if published.is_empty() {
                    this.run_rebase(plan, branch, window, cx);
                    return;
                }
                let message = t!("rebase.confirm", branch = branch).into_owned();
                this.confirm(
                    message,
                    published_text(&published),
                    t!("rebase.anyway").into_owned(),
                    move |this, confirmed, window, cx| {
                        if confirmed {
                            this.run_rebase(plan, branch, window, cx);
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

    pub(super) fn run_rebase(
        &mut self,
        plan: RebasePlan,
        branch: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Git runs this binary again as the sequence editor.
        let helper = match sequence_editor() {
            Ok(path) => path,
            Err(err) => {
                note(
                    Toast::error(err.to_string()).title(t!("rebase.failed").into_owned()),
                    cx,
                );
                return;
            }
        };
        self.close_rebase();
        cx.notify();
        let done = t!("rebase.done", branch = branch).into_owned();
        let paused = t!("rebase.paused", branch = branch).into_owned();
        let conflict = Some((
            t!("rebase.conflict_title").into_owned(),
            t!("rewrite.conflict_text").into_owned(),
        ));
        let record = Record::checked_out(
            t!("undo.op_rebase", branch = branch).into_owned(),
            Worktree::Follow,
        );
        self.run_recorded_op(
            t!("rebase.failed").into_owned(),
            conflict,
            Some(record),
            move |git, cwd, cancel| {
                // A pause leaves the branch where it was: the Continue that finishes the
                // rebase records it for undo.
                Ok(
                    match rebase_interactive(git, cwd, &plan, &helper, cancel)? {
                        RebaseOutcome::Finished => done,
                        RebaseOutcome::Stopped => paused,
                    },
                )
            },
            window,
            cx,
        );
    }

    /// Continues a rebase and records the branch it moves when it finishes: the one named
    /// in git's rebase state (a detached rebase records nothing). A stop at the next
    /// conflict records nothing either; the Continue that finishes does.
    pub(super) fn continue_rebase(
        &mut self,
        failed: String,
        conflict: Option<(String, String)>,
        done: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git_dir = repo.repo.git_dir().to_owned();
        self.run_entry_op(
            failed,
            conflict,
            move |git, cwd, cancel| {
                let proceed = || {
                    run_operation_action(
                        git,
                        cwd,
                        Operation::Rebase,
                        OperationAction::Continue,
                        cancel,
                    )
                };
                let branch = ["rebase-merge", "rebase-apply"]
                    .iter()
                    .find_map(|dir| {
                        std::fs::read_to_string(git_dir.join(dir).join("head-name")).ok()
                    })
                    .map(|name| name.trim().to_owned())
                    .filter(|name| name.starts_with("refs/heads/"));
                let Some(branch) = branch else {
                    proceed()?;
                    return Ok((done, None));
                };
                let (_, changes) = undo::record(git, cwd, &[branch.as_str()], cancel, proceed)?;
                let short = branch.trim_start_matches("refs/heads/");
                let label = t!("undo.op_rebase", branch = short).into_owned();
                Ok((done, UndoEntry::new(label, changes, Worktree::Follow)))
            },
            window,
            cx,
        );
    }

    pub(super) fn close_compare(&mut self) {
        self.compare_view = None;
        self._compare_events = None;
        self.compare_head = None;
        self.compare_base = None;
        self.compare_review_file = None;
        self.compare_review_base = None;
    }

    /// Compares `head` (a branch, remote branch or tag) with the current branch in the
    /// centre: commits on each side, and the files `head` changed since the merge base.
    pub(super) fn open_compare(
        &mut self,
        head: String,
        head_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_compare_refs(None, head, head_name, window, cx);
    }

    /// Compares `head` with `base` (a ref or commit, and its name shown), or with the
    /// current branch when `None`. Only a comparison with the current branch is a review.
    pub(super) fn open_compare_refs(
        &mut self,
        base: Option<(String, String)>,
        head: String,
        head_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let current = repo.refs.head.branch_name();
        // The checked-out branch chosen as the base is the usual comparison.
        let base = base.filter(|(rev, _)| {
            current.is_none_or(|name| rev.as_str() != format!("refs/heads/{name}"))
        });
        let base_name = match &base {
            Some((_, name)) => name.clone(),
            None => current.unwrap_or("HEAD").to_owned(),
        };
        let view = cx.new(|cx| CompareView::new(base_name, head_name, cx));
        self.close_centre();
        self._compare_events = Some(cx.subscribe_in(&view, window, Self::on_compare_event));
        self.compare_view = Some(view);
        self.compare_head = Some(head);
        self.compare_base = base;
        cx.notify();
        self.load_compare(cx);
    }

    /// (Re)compares into the open view; runs again after every reload.
    pub(super) fn load_compare(&mut self, cx: &mut Context<Self>) {
        const COMPARE_LIMIT: usize = 500;
        let (RepoState::Loaded(repo), Some(view), Some(head)) =
            (&self.state, &self.compare_view, self.compare_head.clone())
        else {
            return;
        };
        let git = repo.git.clone();
        let cwd = repo.cwd();
        // A branch compared with the checked-out branch is a review, and brings its
        // session (which files are viewed). Tags and detached HEADs don't.
        let current = match &repo.refs.head {
            HeadInfo::Branch { name, .. } if name.starts_with("refs/") => Some(name.clone()),
            HeadInfo::Branch { name, .. } => Some(format!("refs/heads/{name}")),
            _ => None,
        };
        let is_branch = head.starts_with("refs/heads/") || head.starts_with("refs/remotes/");
        let base_rev = self
            .compare_base
            .as_ref()
            .map_or_else(|| "HEAD".to_owned(), |(rev, _)| rev.clone());
        let review_file = current
            .clone()
            .filter(|_| is_branch && self.compare_base.is_none())
            .map(|base| (review::review_file(&cwd, &head), base));
        // The view's own session (with marks not saved yet) is re-anchored; the first
        // time, the one on disk.
        let known = view.read(cx).review().cloned();
        let limit = if view.read(cx).shows_all() {
            gitweft_git::read::NO_LIMIT
        } else {
            COMPARE_LIMIT
        };
        let creation_factor = view
            .read(cx)
            .match_harder()
            .then_some(gitweft_git::read::HARDER_CREATION_FACTOR);
        let job = self.jobs.spawn(Lane::Latest("compare"), {
            let review_file = review_file.clone();
            move |cancel| {
                let result =
                    gitweft_git::read::compare(&git, &cwd, &base_rev, &head, limit, cancel)
                        .map_err(|err| err.to_string());
                // The session as the file holds it, when it's read here: the base the next
                // save merges from.
                let mut loaded = None;
                let session = review_file.map(|(file, base)| {
                    let mut session = known
                        .or_else(|| {
                            loaded = review::load(&file);
                            loaded.clone()
                        })
                        .unwrap_or_else(|| ReviewSession::new(head.clone(), base.clone()));
                    session.base = base;
                    if let Ok(comparison) = &result {
                        reanchor_comments(&git, &cwd, comparison, &mut session, cancel);
                    }
                    session
                });
                // What changed since the review was last marked done.
                let since = match (&result, session.as_ref().and_then(|s| s.reviewed)) {
                    (Ok(comparison), Some(reviewed)) => {
                        let base = comparison.merge_base.unwrap_or(comparison.base);
                        let since = review::since_review(
                            &git,
                            &cwd,
                            &reviewed,
                            base,
                            comparison.head,
                            creation_factor,
                            cancel,
                        );
                        Some((reviewed, since))
                    }
                    _ => None,
                };
                (result, session, since, loaded)
            }
        });
        let view = view.downgrade();
        let review_file = review_file.map(|(file, _)| file);
        // Another branch's review starts from what its own file holds (read by the job).
        if self.compare_review_file != review_file {
            self.compare_review_base = None;
        }
        self.compare_review_file = review_file;
        cx.spawn(async move |this, cx| {
            if let Some((result, session, since, loaded)) = job.join().await {
                if loaded.is_some() {
                    this.update(cx, |this, _| this.compare_review_base = loaded)
                        .ok();
                }
                view.update(cx, |view, cx| view.set_result(result, session, since, cx))
                    .ok();
                // An open review diff follows the branch, and its comments may have moved.
                this.update(cx, |this, cx| this.follow_review_range(cx))
                    .ok();
                this.update(cx, |this, cx| this.push_review_comments(cx))
                    .ok();
            }
        })
        .detach();
    }

    pub(super) fn on_compare_event(
        &mut self,
        _: &Entity<CompareView>,
        event: &CompareEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            CompareEvent::Close => {
                self.close_compare();
                cx.notify();
            }
            CompareEvent::SelectCommit(id) => self.show_details(*id, cx),
            CompareEvent::ShowInGraph(id) => self.show_commit(*id, cx),
            CompareEvent::CherryPick(id, summary) => {
                let commit = RowCommit {
                    id: *id,
                    merge: false,
                    summary: summary.clone(),
                };
                self.pick_commit(commit, false, window, cx);
            }
            CompareEvent::CreateBranch(id) => {
                let start = (id.to_string(), id.to_hex_with_len(8).to_string());
                self.open_new_branch(Some(start), window, cx);
            }
            CompareEvent::Blame { path, commit } => {
                self.open_blame(path.clone(), Some(*commit), cx);
            }
            CompareEvent::History { path, commit } => {
                self.open_history(path.clone(), Some(*commit), cx);
            }
            CompareEvent::DiffTool { from, to, change } => {
                let target = gitweft_git::tools::DiffToolTarget::Commit {
                    parent: Some(*from),
                    commit: *to,
                };
                let (path, old_path) = (change.path.clone(), change.old_path.clone());
                self.run_diff_tool(target, path, old_path, window, cx);
            }
            CompareEvent::Swap => self.swap_compare(window, cx),
            CompareEvent::Recompare => self.load_compare(cx),
            // Saved in the write lane, so the saves land in order.
            CompareEvent::ReviewChanged(session) => {
                // An open review diff shows the comments as they are now (a restored one too).
                self.push_review_comments(cx);
                if let Some(file) = self.compare_review_file.clone() {
                    let session = session.clone();
                    // Merged onto the file as it is now: another Gitweft may have added or
                    // changed comments meanwhile. Nothing read yet: a fresh start.
                    let base = self.compare_review_base.clone().unwrap_or_else(|| {
                        ReviewSession::new(session.branch.clone(), session.base.clone())
                    });
                    let job = self.jobs.spawn(Lane::Write, {
                        let file = file.clone();
                        move |_| {
                            review::save_merged(&file, &base, &session)
                                .map(|(_, saved)| (session, saved))
                        }
                    });
                    // A session file that couldn't be read is kept aside, one a newer
                    // version wrote is left alone: either way the user hears of it.
                    cx.spawn(async move |this, cx| match job.join().await {
                        Some(Ok((sent, saved))) => {
                            this.update(cx, |this, cx| {
                                // What this tab sent is what it knows of the file now.
                                if this.compare_review_file.as_ref() == Some(&file) {
                                    this.compare_review_base = Some(sent);
                                }
                                if let Some(notice) = sync::data_notice(&file, &saved) {
                                    crate::tab_notes::note_soon(notice, cx);
                                }
                            })
                            .ok();
                        }
                        Some(Err(err)) => {
                            let text =
                                t!("review.save_failed", error = err.to_string()).into_owned();
                            this.update(cx, |_, cx| crate::tab_notes::note_failure(text, cx))
                                .ok();
                        }
                        None => {}
                    })
                    .detach();
                }
            }
            CompareEvent::OpenFile {
                from,
                to,
                change,
                subtitle,
                kind,
            } => {
                let params = DiffParams {
                    title: change.path.to_string(),
                    subtitle: subtitle.clone(),
                    source: DiffSource::Commit {
                        parent: Some(*from),
                        commit: *to,
                    },
                    path: change.path.clone(),
                    old_path: change.old_path.clone(),
                    options: DiffOptions::default(),
                };
                self.open_diff(params, cx);
                // Opened from the review: its lines take comments (a diff since the last
                // review only on its new side: its old side isn't the review's).
                self.review_diff = true;
                self.review_since = *kind == ReviewDiff::Since;
                self.push_review_comments(cx);
            }
            CompareEvent::OpenInterdiff {
                title,
                subtitle,
                lines,
                commit,
            } => self.open_interdiff(title.clone(), subtitle.clone(), lines, *commit, cx),
            CompareEvent::OpenComment(id) => self.open_comment(*id, window, cx),
            CompareEvent::ExportReview {
                markdown,
                name,
                head,
                target,
            } => match target {
                ExportTarget::File => self.export_review(markdown.clone(), name, window, cx),
                ExportTarget::Clipboard => {
                    cx.write_to_clipboard(ClipboardItem::new_string(markdown.clone()));
                    let message = t!("review.copied").into_owned();
                    note(Toast::success(message), cx);
                }
                ExportTarget::Note => self.note_review(markdown.clone(), *head, window, cx),
            },
        }
    }

    /// Asks where to save a review's Markdown (the desktop's save dialog, starting in the
    /// repository), then writes it in the write lane and says where it went.
    pub(super) fn export_review(
        &mut self,
        markdown: String,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let dir = repo.cwd();
        let chosen = cx.prompt_for_new_path(&dir, Some(name));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = chosen.await else {
                return;
            };
            let file = path.clone();
            let Ok(job) = this.update(cx, |this, _| {
                this.jobs
                    .spawn(Lane::Write, move |_| std::fs::write(&file, markdown))
            }) else {
                return;
            };
            let result = job.join().await;
            this.update_in(cx, |_, _, cx| match result {
                Some(Ok(())) => note(
                    Toast::success(
                        t!("review.exported", path = path.display().to_string()).into_owned(),
                    ),
                    cx,
                ),
                Some(Err(err)) => note(
                    Toast::error(err.to_string()).title(t!("review.export_failed").into_owned()),
                    cx,
                ),
                None => {}
            })
            .ok();
        })
        .detach();
    }

    /// Shows how a rewritten commit's patch changed (git's diff of the two patches) on top
    /// of the comparison, coloured as a diff.
    pub(super) fn open_interdiff(
        &mut self,
        title: String,
        subtitle: String,
        lines: &[String],
        commit: ObjectId,
        cx: &mut Context<Self>,
    ) {
        let view = cx.new(|cx| FileView::text(title, subtitle, cx));
        self.close_diff();
        self.close_blame();
        self.close_file();
        self._file_events = Some(cx.subscribe(&view, Self::on_file_event));
        // Already in memory and short (one commit's patches), so no job. Each line is
        // shown without the interdiff's own `+`/`-`/` `: that becomes the row's tint and
        // its numbers in the two patches, and what is left is the patch line, which
        // the diff highlighter then colours as the patch's own.
        let rows = gitweft_git::read::interdiff_rows(lines);
        let text = if rows.is_empty() {
            String::new()
        } else {
            // The trailing newline keeps an empty last line, which `lines()` would drop.
            rows.iter()
                .map(|row| row.text.as_str())
                .collect::<Vec<_>>()
                .join("\n")
                + "\n"
        };
        let result = file_text(
            text.as_bytes(),
            "interdiff.diff",
            cx.palette().dark,
            crate::code_scroll::tab_width(cx),
        )
        .ok_or_else(|| t!("file.binary").into_owned());
        view.update(cx, |view, cx| view.set_interdiff(rows, result, cx));
        self.file_view = Some(view);
        self.interdiff_commit = Some(commit);
        cx.notify();
    }

    pub(super) fn close_reflog(&mut self) {
        self.reflog_view = None;
        self._reflog_events = None;
        self.reflog_ref = None;
    }

    /// Shows `refname`'s reflog (`HEAD`, or a branch) in the centre, with the commits only
    /// the reflog still reaches marked as lost.
    pub(super) fn open_reflog(&mut self, refname: String, name: String, cx: &mut Context<Self>) {
        if !matches!(self.state, RepoState::Loaded(_)) {
            return;
        }
        let view = cx.new(|cx| ReflogView::new(name, cx));
        self.close_centre();
        self._reflog_events = Some(cx.subscribe(&view, Self::on_reflog_event));
        self.reflog_view = Some(view);
        self.reflog_ref = Some(refname);
        cx.notify();
        self.load_reflog(cx);
    }

    /// (Re)reads the open reflog into its view. Called again after every reload, so new
    /// entries appear and the lost marks follow the refs (a recovered commit loses it).
    pub(super) fn load_reflog(&mut self, cx: &mut Context<Self>) {
        const REFLOG_LIMIT: usize = 1_000;
        let (RepoState::Loaded(repo), Some(view), Some(refname)) =
            (&self.state, &self.reflog_view, self.reflog_ref.clone())
        else {
            return;
        };
        let git = repo.git.clone();
        let cwd = repo.cwd();

        let job = self.jobs.spawn(Lane::Latest("reflog"), move |cancel| {
            let load = || -> Result<Reflog, gitweft_git::read::ReadError> {
                let entries =
                    gitweft_git::read::reflog(&git, &cwd, &refname, REFLOG_LIMIT, cancel)?;
                let mut ids: Vec<ObjectId> = entries.iter().map(|entry| entry.id).collect();
                ids.sort_unstable();
                ids.dedup();
                let lost = gitweft_git::read::unreachable_from_refs(&git, &cwd, &ids, cancel)?;
                Ok(Reflog { entries, lost })
            };
            load().map_err(|err| err.to_string())
        });
        let view = view.downgrade();
        cx.spawn(async move |_, cx| {
            if let Some(result) = job.join().await {
                view.update(cx, |view, cx| view.set_result(result, cx)).ok();
            }
        })
        .detach();
    }

    pub(super) fn on_reflog_event(
        &mut self,
        _: Entity<ReflogView>,
        event: &ReflogEvent,
        cx: &mut Context<Self>,
    ) {
        match *event {
            ReflogEvent::Close => {
                self.close_reflog();
                cx.notify();
            }
            ReflogEvent::Select(id) => self.show_details(id, cx),
            ReflogEvent::CreateBranch(id) => {
                let start = (id.to_string(), id.to_hex_with_len(8).to_string());
                self.with_window(
                    move |this, window, cx| this.open_new_branch(Some(start), window, cx),
                    cx,
                );
            }
            ReflogEvent::Checkout(id) => {
                let op = BranchOp::Detach { commit: id };
                self.with_window(
                    move |this, window, cx| this.run_branch_op(op, window, cx),
                    cx,
                );
            }
            ReflogEvent::ShowInGraph(id) => self.show_commit(id, cx),
            ReflogEvent::CherryPick(id, ref summary) => {
                let commit = RowCommit {
                    id,
                    merge: false,
                    summary: summary.clone(),
                };
                self.with_window(
                    move |this, window, cx| this.pick_commit(commit.clone(), false, window, cx),
                    cx,
                );
            }
            ReflogEvent::BrowseFiles(id) => self.open_tree(id, cx),
            // The graph's reset: the published-commits check, and the hard reset's prompt.
            ReflogEvent::Reset(id, mode) => {
                let commit = RowCommit {
                    id,
                    merge: false,
                    summary: id.to_hex_with_len(8).to_string(),
                };
                self.with_window(
                    move |this, window, cx| this.reset_to(commit, mode, window, cx),
                    cx,
                );
            }
            ReflogEvent::ShowDangling => self.open_dangling(cx),
        }
    }

    pub(super) fn close_tree(&mut self) {
        self.tree_view = None;
        self._tree_events = None;
    }

    pub(super) fn close_file(&mut self) {
        self.file_view = None;
        self._file_events = None;
        self.interdiff_commit = None;
    }

    /// Shows the files of `commit` in the centre. Files, blames and histories opened from
    /// it show on top of it.
    pub(super) fn open_tree(&mut self, commit: ObjectId, cx: &mut Context<Self>) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let (shared, workdir) = (repo.repo.clone(), repo.workdir.clone());
        let view = cx.new(|cx| TreeView::new(commit, workdir, cx));
        self.close_centre();
        self._tree_events = Some(cx.subscribe(&view, Self::on_tree_event));
        self.tree_view = Some(view.clone());
        cx.notify();

        // The model is built on the job too: sorting a large tree takes a few ms.
        let job = self.jobs.spawn(Lane::Latest("tree"), move |_| {
            gitweft_git::read::tree_entries(&shared.to_thread_local(), commit)
                .map(TreeModel::new)
                .map_err(|err| err.to_string())
        });
        let view = view.downgrade();
        cx.spawn(async move |_, cx| {
            if let Some(result) = job.join().await {
                view.update(cx, |view, cx| view.set_result(result, cx)).ok();
            }
        })
        .detach();
    }

    /// Shows `path` as of `commit`, on top of the tree.
    pub(super) fn open_file(&mut self, commit: ObjectId, path: BString, cx: &mut Context<Self>) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let cwd = repo.cwd();
        let view = cx.new(|cx| FileView::new(commit, path.clone(), cx));
        self.close_diff();
        self.close_blame();
        self.close_file();
        self._file_events = Some(cx.subscribe(&view, Self::on_file_event));
        self.file_view = Some(view.clone());
        cx.notify();

        let dark = cx.palette().dark;
        let tab = crate::code_scroll::tab_width(cx);
        let job = self.jobs.spawn(
            Lane::Latest("file"),
            move |cancel| -> Result<FileText, (bool, String)> {
                let bytes =
                    gitweft_git::read::file_content(&git, &cwd, Some(commit), &path, cancel)
                        .map_err(|err| {
                            // A partial clone's missing version: offered to fetch.
                            let missing = gitweft_git::partial::is_missing_objects(err.as_ref());
                            (missing, err.to_string())
                        })?;
                file_text(&bytes, &path.to_string(), dark, tab)
                    .ok_or_else(|| (false, t!("file.binary").into_owned()))
            },
        );
        let view = view.downgrade();
        cx.spawn(async move |_, cx| {
            if let Some(result) = job.join().await {
                view.update(cx, |view, cx| match result {
                    Err((true, _)) => view.set_missing(cx),
                    result => view.set_text(result.map_err(|(_, message)| message), cx),
                })
                .ok();
            }
        })
        .detach();
    }

    pub(super) fn on_tree_event(
        &mut self,
        _: Entity<TreeView>,
        event: &TreeEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            TreeEvent::Close => {
                self.close_tree();
                cx.notify();
            }
            TreeEvent::OpenFile { commit, path } => self.open_file(*commit, path.clone(), cx),
            TreeEvent::Blame { commit, path } => self.open_blame(path.clone(), Some(*commit), cx),
            TreeEvent::History { commit, path } => {
                self.open_history(path.clone(), Some(*commit), cx)
            }
            TreeEvent::Editor { path } => self.open_tree_path(path, external::Program::Editor, cx),
            TreeEvent::FileManager { path } => {
                self.open_tree_path(path, external::Program::FileManager, cx)
            }
            TreeEvent::Terminal { path } => {
                self.open_tree_path(path, external::Program::Terminal, cx)
            }
            TreeEvent::Bisect { path } => self.bisect_path(path, cx),
        }
    }

    /// "Bisect changes to this path…": a bisect of only the commits that change it (`git
    /// bisect start -- <path>`); the banner then asks for a bad and a good commit, marked
    /// in the graph, so the tree makes way for it. One bisect at a time.
    pub(super) fn bisect_path(&mut self, path: &BString, cx: &mut Context<Self>) {
        let path = gitweft_core::external::path_from_bytes(path).to_path_buf();
        let running = matches!(self.operation, Some(Operation::Bisect));
        self.with_window(
            move |this, window, cx| {
                if running {
                    let message = t!("bisect.already_running").into_owned();
                    note(Toast::warning(message), cx);
                    return;
                }
                this.close_tree();
                this.run_bisect(
                    move |git, cwd, cancel| {
                        bisect_start(git, cwd, None, &[], std::slice::from_ref(&path), cancel)
                            .map(|_| ())
                    },
                    window,
                    cx,
                );
            },
            cx,
        );
    }

    /// A tree row's working-tree copy in one of the user's programs.
    pub(super) fn open_tree_path(
        &mut self,
        path: &BString,
        program: external::Program,
        cx: &mut Context<Self>,
    ) {
        let path = path.clone();
        self.with_window(
            move |this, window, cx| this.open_working_file(&path, program, window, cx),
            cx,
        );
    }

    pub(super) fn on_file_event(
        &mut self,
        _: Entity<FileView>,
        event: &FileEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            FileEvent::Close => {
                self.close_file();
                self.refocus_centre(cx);
                cx.notify();
            }
            FileEvent::FetchMissing { commit, path } => {
                self.fetch_missing_file(*commit, path.clone(), cx)
            }
            FileEvent::Blame { commit, path } => self.open_blame(path.clone(), Some(*commit), cx),
            FileEvent::History { commit, path } => {
                self.open_history(path.clone(), Some(*commit), cx)
            }
            FileEvent::CommentOnInterdiff { file, anchor } => {
                self.comment_on_interdiff(file.clone(), anchor.clone(), cx)
            }
        }
    }

    /// A comment picked on a line of an interdiff. The interdiff has no line numbers
    /// of the file (git prints none in its `@@`), so the file is read at the commit as it
    /// is now and the anchored line found in it; the comment is then an ordinary one on
    /// that file's new side, and shows up in the review and its export like any other.
    pub(super) fn comment_on_interdiff(
        &mut self,
        file: String,
        anchor: review::Anchor,
        cx: &mut Context<Self>,
    ) {
        let (RepoState::Loaded(repo), Some(commit)) = (&self.state, self.interdiff_commit) else {
            return;
        };
        let git = repo.git.clone();
        let cwd = repo.cwd();
        let path = BString::from(file.clone());
        let wanted = anchor.clone();
        let job = self
            .jobs
            .spawn(Lane::Latest("interdiff-comment"), move |cancel| {
                let bytes =
                    gitweft_git::read::file_content(&git, &cwd, Some(commit), &path, cancel)
                        .ok()?;
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let lines: Vec<&str> = text.lines().collect();
                wanted.find(&lines, 1)
            });
        cx.spawn(async move |this, cx| {
            let Some(found) = job.join().await else {
                return;
            };
            this.update(cx, |this, cx| match found {
                Some(line) => this.with_window(
                    move |this, window, cx| {
                        this.write_comment(file, review::Side::New, line, anchor, window, cx)
                    },
                    cx,
                ),
                // The line isn't in the file as this commit has it: nothing to anchor to.
                None => crate::tab_notes::note_soon(
                    Toast::warning(t!("review.patch_line_gone").into_owned()),
                    cx,
                ),
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn close_blame(&mut self) {
        if let Some(cancel) = self.blame_cancel.take() {
            cancel.cancel();
        }
        self.blame_view = None;
        self._blame_events = None;
    }

    /// Shows who last changed each line of `path` at `rev` (the working-tree file when
    /// `None`) in the centre. The text comes first, then the blame streams in.
    pub(super) fn open_blame(
        &mut self,
        path: BString,
        rev: Option<ObjectId>,
        cx: &mut Context<Self>,
    ) {
        self.blame_back.clear();
        self.open_blame_at(path, rev, None, cx);
    }

    /// [`Self::open_blame`] with the first line to show, keeping the Back list. The
    /// commits ignored in this repository are left out.
    pub(super) fn open_blame_at(
        &mut self,
        path: BString,
        rev: Option<ObjectId>,
        scroll_to: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let cwd = repo.cwd();
        let subtitle = match rev {
            Some(rev) => t!("blame.at_commit", sha = rev.to_hex_with_len(8)).into_owned(),
            None => t!("blame.working_tree").into_owned(),
        };
        let shown = path.to_string();
        let view = cx.new(|cx| BlameView::new(shown.clone(), subtitle, cx));
        self.close_diff();
        self.close_blame();
        self._blame_events = Some(cx.subscribe(&view, Self::on_blame_event));
        self.blame_view = Some(view.clone());
        let ignore_revs = self.sync.settings.blame_ignored.clone();
        let can_go_back = !self.blame_back.is_empty();
        view.update(cx, |view, _| {
            view.source = Some((path.clone(), rev));
            view.set_context(ignore_revs.len(), can_go_back, scroll_to);
        });
        cx.notify();

        enum Message {
            Text(FileText),
            IgnoreFile(Option<String>),
            Ignored(Vec<(ObjectId, Option<String>)>),
            Updates(Vec<gitweft_git::read::BlameUpdate>),
        }
        let (tx, rx) = async_channel::unbounded::<Message>();
        let dark = cx.palette().dark;
        let tab = crate::code_scroll::tab_width(cx);
        // Opening another blame cancels one still running.
        let job = self.jobs.spawn(
            Lane::Latest("blame"),
            move |cancel| -> Result<(), (bool, String)> {
                // A partial clone's missing versions are offered to fetch.
                let missing = |err: gitweft_git::read::ReadError| -> (bool, String) {
                    (
                        gitweft_git::partial::is_missing_objects(err.as_ref()),
                        err.to_string(),
                    )
                };
                let bytes = gitweft_git::read::file_content(&git, &cwd, rev, &path, cancel)
                    .map_err(missing)?;
                let text = file_text(&bytes, &shown, dark, tab)
                    .ok_or_else(|| (false, t!("blame.binary").into_owned()))?;
                let _ = tx.try_send(Message::Text(text));
                // Git applies the repository's own list itself; the header names it.
                if let Ok(Some(file)) =
                    gitweft_git::read::blame_ignore_revs_file(&git, &cwd, cancel)
                    && file.is_file()
                {
                    let shown = file.strip_prefix(&cwd).unwrap_or(&file);
                    let shown = shown.display().to_string();
                    let _ = tx.try_send(Message::IgnoreFile(Some(shown)));
                }
                // The Stop ignoring menu names them.
                if !ignore_revs.is_empty()
                    && let Ok(commits) =
                        gitweft_git::read::commit_summaries(&git, &cwd, &ignore_revs, cancel)
                {
                    let _ = tx.try_send(Message::Ignored(commits));
                }
                let options = gitweft_git::read::BlameOptions {
                    ignore_revs,
                    ..Default::default()
                };
                gitweft_git::read::blame(&git, &cwd, rev, &path, options, cancel, |updates| {
                    let _ = tx.try_send(Message::Updates(updates));
                })
                .map_err(missing)
            },
        );
        self.blame_cancel = Some(job.cancel_token().clone());
        let view = view.downgrade();
        cx.spawn(async move |_, cx| {
            // Everything that arrived since the last frame goes in as one update. The
            // channel closes when the job ends.
            while let Ok(first) = rx.recv().await {
                let mut batch = vec![first];
                while let Ok(more) = rx.try_recv() {
                    batch.push(more);
                }
                let shown = view.update(cx, |view, cx| {
                    for message in batch {
                        match message {
                            Message::Text(text) => view.set_text(text, cx),
                            Message::IgnoreFile(file) => view.set_ignore_file(file, cx),
                            Message::Ignored(commits) => view.set_ignored_commits(commits, cx),
                            Message::Updates(updates) => view.apply(updates, cx),
                        }
                    }
                });
                if shown.is_err() {
                    return;
                }
            }
            if let Some(result) = job.join().await {
                view.update(cx, |view, cx| match result {
                    Err((true, _)) => view.finish_missing(cx),
                    result => view.finish(result.map_err(|(_, message)| message), cx),
                })
                .ok();
            }
        })
        .detach();
    }

    pub(super) fn on_blame_event(
        &mut self,
        view: Entity<BlameView>,
        event: &BlameEvent,
        cx: &mut Context<Self>,
    ) {
        // The blame shown now, and the first line shown.
        let row_height = cx.metrics().code_line;
        let (source, top) = {
            let view = view.read(cx);
            (view.source.clone(), view.top_line(row_height))
        };
        match event {
            BlameEvent::Close => {
                self.blame_back.clear();
                self.close_blame();
                self.refocus_centre(cx);
                cx.notify();
            }
            BlameEvent::FetchMissing => {
                if let Some((path, rev)) = source {
                    self.fetch_missing_blame(path, rev, cx);
                }
            }
            BlameEvent::ShowCommit(id) => self.show_commit(*id, cx),
            BlameEvent::BlameParent { parent, path, line } => {
                if let Some((current, rev)) = source {
                    self.blame_back.push((current, rev, top));
                }
                // The line a few lines down from the top, so what's around it shows too.
                let scroll = line.saturating_sub(5);
                self.open_blame_at(path.clone(), Some(*parent), Some(scroll), cx);
            }
            BlameEvent::Back => {
                if let Some((path, rev, top)) = self.blame_back.pop() {
                    self.open_blame_at(path, rev, Some(top), cx);
                }
            }
            BlameEvent::Ignore(_) | BlameEvent::StopIgnoring(_) => {
                let ignored = &mut self.sync.settings.blame_ignored;
                match event {
                    BlameEvent::Ignore(id) if !ignored.contains(id) => ignored.push(*id),
                    BlameEvent::StopIgnoring(Some(id)) => ignored.retain(|ignored| ignored != id),
                    BlameEvent::StopIgnoring(None) => ignored.clear(),
                    _ => return,
                }
                self.save_repo_settings(cx);
                if let Some((path, rev)) = source {
                    self.open_blame_at(path, rev, Some(top), cx);
                }
            }
        }
    }

    pub(super) fn on_history_event(
        &mut self,
        _: Entity<HistoryView>,
        event: &HistoryEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            HistoryEvent::Close => {
                self.close_history();
                self.refocus_centre(cx);
                cx.notify();
            }
            HistoryEvent::OpenDiff(revision) => {
                let params = DiffParams {
                    title: revision.path.to_string(),
                    subtitle: t!("diff.in_commit", sha = revision.id.to_hex_with_len(8))
                        .into_owned(),
                    source: DiffSource::Commit {
                        parent: revision.parents.first().copied(),
                        commit: revision.id,
                    },
                    path: revision.path.clone(),
                    old_path: revision.old_path.clone(),
                    options: DiffOptions::default(),
                };
                self.open_diff(params, cx);
            }
            HistoryEvent::Blame(revision) => {
                self.open_blame(revision.path.clone(), Some(revision.id), cx);
            }
            HistoryEvent::ShowCommit(id) => self.show_commit(*id, cx),
            HistoryEvent::OpenFile(revision) => {
                self.open_file(revision.id, revision.path.clone(), cx);
            }
            HistoryEvent::BrowseFiles(id) => self.open_tree(*id, cx),
            HistoryEvent::DiffTool(revision) => {
                let target = gitweft_git::tools::DiffToolTarget::Commit {
                    parent: revision.parents.first().copied(),
                    commit: revision.id,
                };
                let (path, old_path) = (revision.path.clone(), revision.old_path.clone());
                self.with_window(
                    move |this, window, cx| this.run_diff_tool(target, path, old_path, window, cx),
                    cx,
                );
            }
        }
    }
}

/// The program git runs as the sequence editor: this binary, which handles
/// `--sequence-editor`. A unit test's binary doesn't, so the headless UI tests set a stand-in.
fn sequence_editor() -> std::io::Result<std::path::PathBuf> {
    #[cfg(test)]
    if let Some(path) = crate::ui_test::SEQUENCE_EDITOR.get() {
        return Ok(path.clone());
    }
    gitweft_git::helper_program()
}
