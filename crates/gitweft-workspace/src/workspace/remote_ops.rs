//! Remotes and network operations: remote config, fetch, pull, push, their choices and the activity panel.

use super::*;

impl Workspace {
    /// Reads the configured remotes (and their URLs) for the sidebar and the push choice.
    pub(super) fn load_remotes(&mut self, cx: &mut Context<Self>) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let cwd = repo.cwd();
        let job = self.jobs.spawn(Lane::Coalesce("remotes"), move |cancel| {
            configured_remotes(&git, &cwd, cancel).unwrap_or_default()
        });
        cx.spawn(async move |this, cx| {
            if let Some(remotes) = job.join().await {
                this.update(cx, |this, cx| {
                    if this.remotes != remotes {
                        this.remotes = remotes;
                        cx.notify();
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    /// "Add remote…": its name and URL, then (by default) a fetch so its branches show.
    /// Remote changes live in the config, not in refs, so they aren't undoable.
    pub(super) fn open_add_remote(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let fetch_after = Rc::new(Cell::new(true));
        let spec = TextDialog {
            title: t!("remote.add_title").into_owned(),
            subtitle: None,
            placeholder: t!("remote.name_placeholder").into_owned(),
            // The first remote is usually origin.
            initial: if self.remotes.is_empty() {
                "origin".to_owned()
            } else {
                String::new()
            },
            detail: Some((t!("remote.url_placeholder").into_owned(), String::new())),
            checkbox: Some((
                t!("remote.fetch_after_add").into_owned(),
                fetch_after.clone(),
            )),
            ok_label: t!("remote.add").into_owned(),
            allow_empty: false,
            multiline: false,
        };
        let submit: SubmitText = Rc::new(move |this, name, url, window, cx| {
            let (name, url) = (name.trim().to_owned(), url.trim().to_owned());
            this.run_add_remote(name, url, fetch_after.get(), window, cx);
        });
        self.open_text_dialog(spec, submit, window, cx);
    }

    /// Adds a remote in the write lane; once it exists (and only then), fetches if asked,
    /// so a refused name isn't followed by a fetch whose toast would cover the error.
    pub(super) fn run_add_remote(
        &mut self,
        name: String,
        url: String,
        fetch: bool,
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
            .spawn(Lane::Write, move |cancel| -> Result<String, GitError> {
                check_remote(&git, &cwd, &name, Some(&url), cancel)?;
                add_remote(&git, &cwd, &name, &url, cancel)?;
                Ok(t!("remote.added", name = name).into_owned())
            });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.join().await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Some(Ok(message)) => {
                        note(Toast::success(message), cx);
                        if fetch {
                            this.run_remote(RemoteOpKind::Fetch, window, cx);
                        }
                    }
                    Some(Err(err)) => note(
                        Toast::error(err.summary()).title(t!("remote.failed").into_owned()),
                        cx,
                    ),
                    None => {}
                }
                this.load_remotes(cx);
                this.load_worktrees(cx);
                this.load_submodules(cx);
                this.refresh_refs(cx);
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn open_rename_remote(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let spec = TextDialog {
            title: t!("remote.rename_title", name = name).into_owned(),
            subtitle: None,
            placeholder: t!("remote.name_placeholder").into_owned(),
            initial: name.clone(),
            detail: None,
            checkbox: None,
            ok_label: t!("remote.rename_ok").into_owned(),
            allow_empty: false,
            multiline: false,
        };
        let submit: SubmitText = Rc::new(move |this, new, _, window, cx| {
            let (old, new) = (name.clone(), new.trim().to_owned());
            if new == old {
                return;
            }
            this.push_choice = None;
            this.run_write_op(
                t!("remote.failed").into_owned(),
                None,
                move |git, cwd, cancel| {
                    check_remote(git, cwd, &new, None, cancel)?;
                    rename_remote(git, cwd, &old, &new, cancel)?;
                    Ok(t!("remote.renamed", old = old, new = new).into_owned())
                },
                window,
                cx,
            );
        });
        self.open_text_dialog(spec, submit, window, cx);
    }

    pub(super) fn open_change_remote_url(
        &mut self,
        remote: RemoteInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The URL it fetches from, then where it pushes to when that's somewhere else.
        let spec = TextDialog {
            title: t!("remote.change_url_title", name = remote.name).into_owned(),
            subtitle: Some(t!("remote.push_url_hint").into_owned()),
            placeholder: t!("remote.url_placeholder").into_owned(),
            initial: remote.url.clone(),
            detail: Some((
                t!("remote.push_url_placeholder").into_owned(),
                remote.push_url.clone().unwrap_or_default(),
            )),
            checkbox: None,
            ok_label: t!("remote.save").into_owned(),
            allow_empty: false,
            multiline: false,
        };
        let (name, old_push) = (remote.name, remote.push_url);
        let submit: SubmitText = Rc::new(move |this, url, push, window, cx| {
            let (name, url) = (name.clone(), url.trim().to_owned());
            let push = push.trim();
            // Empty, or the URL itself: no separate push URL.
            let push = (!push.is_empty() && push != url).then(|| push.to_owned());
            let push_changed = push != old_push;
            this.run_write_op(
                t!("remote.failed").into_owned(),
                None,
                move |git, cwd, cancel| {
                    set_remote_url(git, cwd, &name, &url, cancel)?;
                    if push_changed {
                        set_remote_push_url(git, cwd, &name, push.as_deref(), cancel)?;
                    }
                    Ok(match &push {
                        Some(push) => {
                            t!("remote.urls_changed", name = name, url = url, push = push)
                        }
                        None => t!("remote.url_changed", name = name, url = url),
                    }
                    .into_owned())
                },
                window,
                cx,
            );
        });
        self.open_text_dialog(spec, submit, window, cx);
    }

    /// Asks before removing a remote: its remote-tracking branches go too, for good.
    pub(super) fn confirm_remove_remote(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let message = t!("remote.remove_confirm", name = name).into_owned();
        let detail = t!("remote.remove_confirm_detail").into_owned();
        self.confirm(
            message,
            detail,
            t!("remote.remove_ok").into_owned(),
            move |this, confirmed, window, cx| {
                if confirmed {
                    this.push_choice = None;
                    this.run_write_op(
                        t!("remote.failed").into_owned(),
                        None,
                        move |git, cwd, cancel| {
                            remove_remote(git, cwd, &name, cancel)?;
                            Ok(t!("remote.removed", name = name).into_owned())
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

    /// The branch to push and the remotes to pick from, when pushing the checked-out
    /// branch needs a choice: no upstream, nothing picked yet, and several remotes with
    /// none called origin.
    pub(super) fn push_needs_remote(&self) -> Option<(String, Vec<String>)> {
        let RepoState::Loaded(repo) = &self.state else {
            return None;
        };
        let branch = repo.refs.head.branch_name()?.to_owned();
        // By its bytes when a branch of that name is listed (#63).
        let key = repo
            .refs
            .local_branches
            .iter()
            .find(|entry| entry.short_name == branch)
            .map_or_else(|| branch.clone(), |entry| branch_key(&entry.full_name));
        let has_upstream = self
            .tracking
            .get(&key)
            .is_some_and(|tracking| tracking.upstream.is_some());
        let picked = self
            .push_choice
            .as_ref()
            .is_some_and(|(picked, _)| *picked == branch);
        let names: Vec<String> = self.remotes.iter().map(|r| r.name.clone()).collect();
        let ambiguous = names.len() > 1 && !names.iter().any(|name| name == "origin");
        (!has_upstream && !picked && ambiguous).then_some((branch, names))
    }

    /// Asks which remote a branch's first push goes to, then pushes there.
    pub(super) fn ask_push_remote(
        &mut self,
        kind: RemoteOpKind,
        branch: String,
        names: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let choices = names
            .into_iter()
            .map(|name| {
                let (branch, remote) = (branch.clone(), name.clone());
                let action: ChoiceAction = Rc::new(move |this, window, cx| {
                    this.push_choice = Some((branch.clone(), remote.clone()));
                    this.run_remote(kind, window, cx);
                });
                (name, action)
            })
            .collect();
        self.open_choice(
            t!("remote.pick_title", branch = branch).into_owned(),
            t!("remote.pick_text").into_owned(),
            t!("toolbar.cancel").into_owned(),
            choices,
            window,
            cx,
        );
    }

    /// The activity panel under the graph: recent git commands, newest first.
    pub(super) fn render_activity(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);
        let z = |value: f32| px(value * m.zoom);
        let entries: Vec<ActivityEntry> = self
            .activity
            .entries()
            .into_iter()
            .filter(|entry| self.activity_reads || !entry.read_only)
            .take(200)
            .collect();
        v_flex()
            .h(z(220.))
            .flex_shrink_0()
            .border_t_1()
            .border_color(p.line)
            .child(
                widgets::list_header(&p, &m)
                    .child(
                        truncated("title", t!("activity.title").into_owned())
                            .flex_1()
                            .text_size(m.text.small)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.fg),
                    )
                    .child(
                        Checkbox::new("activity-reads")
                            .small()
                            .label(t!("activity.show_reads").into_owned())
                            .checked(self.activity_reads)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.activity_reads = *checked;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("activity-close")
                            .ghost()
                            .xsmall()
                            .label(t!("activity.close").into_owned())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_activity = false;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .id("activity-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(z(12.))
                    .py(z(4.))
                    .font_family(gitweft_ui::current::mono_font())
                    .text_size(m.text.mono_small)
                    .when(entries.is_empty(), |list| {
                        list.child(
                            div()
                                .text_color(p.fg3)
                                .child(t!("activity.empty").into_owned()),
                        )
                    })
                    .children(entries.into_iter().map(|entry| activity_row(entry, &p, &m))),
            )
    }

    /// Fetches one remote (`Some`), or all of them.
    pub(super) fn fetch_from(
        &mut self,
        remote: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let kind = if remote.is_some() {
            RemoteOpKind::FetchOne
        } else {
            RemoteOpKind::Fetch
        };
        self.fetch_one = remote;
        self.run_remote(kind, window, cx);
    }

    /// Starts a fetch, pull or push in the write lane. Progress shows in the toolbar; the
    /// outcome arrives as a notification, or as a dialog offering the way out.
    pub(super) fn run_remote(
        &mut self,
        kind: RemoteOpKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.remote_op.is_some() {
            // One network operation at a time: say so, rather than drop the request (#12).
            note(Toast::info(t!("remote.busy").into_owned()), cx);
            return;
        }
        if matches!(kind, RemoteOpKind::Push(_))
            && let Some((branch, names)) = self.push_needs_remote()
        {
            self.ask_push_remote(kind, branch, names, window, cx);
            return;
        }
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let cwd = repo.cwd();
        let branch = repo.refs.head.branch_name().map(str::to_owned);
        let chosen = self
            .push_choice
            .clone()
            .filter(|(picked, _)| Some(picked) == branch.as_ref())
            .map(|(_, remote)| remote);
        let fetch_one = self.fetch_one.take();
        let submodule_update = self.submodule_update.take();
        let (progress_tx, progress_rx) = async_channel::unbounded::<Progress>();
        let job = self.jobs.spawn(
            Lane::Write,
            move |cancel| -> Result<(String, Option<UndoEntry>), GitError> {
                // Git names a submodule's folder in full; the repository's root goes.
                let root = format!("{}/", cwd.display());
                let forward = move |event: StderrEvent| match event {
                    StderrEvent::Progress(progress) => {
                        let _ = progress_tx.try_send(progress);
                    }
                    // A submodule update's lines say which one it's at ("Cloning into
                    // '…'…"): shown as the phase until the next meter.
                    StderrEvent::Line(line)
                        if matches!(kind, RemoteOpKind::SubmoduleUpdate)
                            && !line.trim().is_empty() =>
                    {
                        let _ = progress_tx.try_send(Progress {
                            phase: line.trim().replace(root.as_str(), ""),
                            percent: None,
                            current: 0,
                            total: None,
                            remote: false,
                        });
                    }
                    StderrEvent::Line(_) => {}
                };
                match kind {
                    RemoteOpKind::SubmoduleUpdate => {
                        let (superproject, paths) = submodule_update.unwrap_or_default();
                        let folder = cwd.join(&superproject);
                        update_submodules_with_events(
                            &git, &folder, &paths, true, cancel, forward,
                        )?;
                        let message = match paths.as_slice() {
                            [path] => t!(
                                "submodule.updated",
                                name = superproject.join(path).display()
                            ),
                            _ => t!("submodule.updated_all"),
                        };
                        Ok((message.into_owned(), None))
                    }
                    RemoteOpKind::Fetch => {
                        fetch(&git, &cwd, FetchTarget::All, true, cancel, forward)?;
                        Ok((t!("remote.fetched").into_owned(), None))
                    }
                    RemoteOpKind::FetchOne => {
                        let remote = fetch_one.unwrap_or_default();
                        fetch(
                            &git,
                            &cwd,
                            FetchTarget::Remote(&remote),
                            true,
                            cancel,
                            forward,
                        )?;
                        Ok((
                            t!("remote.fetched_named", remote = remote).into_owned(),
                            None,
                        ))
                    }
                    RemoteOpKind::LfsPull => {
                        lfs_pull(&git, &cwd, cancel, forward)?;
                        Ok((t!("lfs.pulled").into_owned(), None))
                    }
                    RemoteOpKind::LfsFetch => {
                        lfs_fetch(&git, &cwd, cancel, forward)?;
                        Ok((t!("lfs.fetched").into_owned(), None))
                    }
                    RemoteOpKind::LfsFetchAll => {
                        lfs_fetch_all(&git, &cwd, cancel, forward)?;
                        Ok((t!("lfs.fetched_all").into_owned(), None))
                    }
                    RemoteOpKind::Deepen(how) => {
                        deepen(&git, &cwd, how, cancel, forward)?;
                        let message = match how {
                            Deepen::By(count) => tn!("shallow.deepened", count = count),
                            Deepen::All => t!("shallow.unshallowed"),
                        };
                        Ok((message.into_owned(), None))
                    }
                    RemoteOpKind::Pull(mode) => {
                        // Only the local branch can be undone; what was fetched stays.
                        let record =
                            Record::checked_out(t!("undo.op_pull").into_owned(), Worktree::Follow);
                        let (_, entry) = recorded(&git, &cwd, Some(record), cancel, || {
                            pull(&git, &cwd, mode, false, cancel, forward)
                        })?;
                        Ok((t!("remote.pulled").into_owned(), entry))
                    }
                    RemoteOpKind::Push(force) => {
                        let failed = |message: String| {
                            GitError::new(GitErrorKind::Other, "git push", message)
                        };
                        let branch =
                            branch.ok_or_else(|| failed(t!("remote.detached").into_owned()))?;
                        // HEAD's branch by its bytes when its name isn't UTF-8: the name
                        // shown is lossy, and push takes such a name encoded (#18).
                        let branch = match gitweft_git::undo::checked_out_ref(&git, &cwd, cancel)? {
                            full if full.starts_with('\0') => {
                                let bytes = gitweft_git::undo::ref_name_bytes(&full);
                                let short = bytes.strip_prefix(b"refs/heads/").unwrap_or(&bytes);
                                gitweft_git::undo::ref_name_text(short)
                            }
                            _ => branch,
                        };
                        let tracking = branch_tracking(&git, &cwd, cancel)?;
                        let remotes = remote_names(&git, &cwd, cancel)?;
                        // Otherwise the remote picked for this branch's first push.
                        let mut request = push_target(&branch, &tracking, &remotes)
                            .or_else(|| {
                                chosen.map(|remote| PushRequest {
                                    remote,
                                    branch: branch.clone(),
                                    remote_branch: branch.clone(),
                                    set_upstream: true,
                                    force: ForceMode::None,
                                })
                            })
                            .ok_or_else(|| failed(t!("remote.no_remote").into_owned()))?;
                        request.force = force;
                        push(&git, &cwd, &request, cancel, forward)?;
                        // Names shown as text (an encoded one decoded, lossily).
                        let shown = |text: &str| {
                            let bytes = gitweft_git::undo::ref_name_bytes(text);
                            String::from_utf8_lossy(&bytes).into_owned()
                        };
                        let target =
                            format!("{}/{}", request.remote, shown(&request.remote_branch));
                        let message = t!(
                            "remote.pushed",
                            branch = shown(&request.branch),
                            target = target
                        );
                        Ok((message.into_owned(), None))
                    }
                }
            },
        );
        self.remote_op = Some(RemoteOp {
            kind,
            progress: None,
            cancel: job.cancel_token().clone(),
        });
        cx.notify();

        // Progress ends when the job drops its sender.
        cx.spawn(async move |this, cx| {
            while let Ok(progress) = progress_rx.recv().await {
                let updated = this.update(cx, |this, cx| {
                    if let Some(op) = &mut this.remote_op {
                        op.progress = Some(progress);
                        cx.notify();
                    }
                });
                if updated.is_err() {
                    break;
                }
            }
        })
        .detach();
        cx.spawn_in(window, async move |this, cx| {
            let result = job.join().await;
            this.update_in(cx, |this, window, cx| {
                let result = result.map(|result| this.take_entry(result, cx));
                this.remote_finished(kind, result, window, cx)
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn remote_finished(
        &mut self,
        kind: RemoteOpKind,
        result: Option<Result<String, GitError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A job cancelled while it ran hands back no result at all (`JobHandle::join`).
        let cancelled = self
            .remote_op
            .take()
            .is_some_and(|op| op.cancel.is_cancelled());
        match result {
            None if cancelled => note(Toast::info(t!("remote.cancelled").into_owned()), cx),
            Some(Ok(message)) => note(Toast::success(message), cx),
            Some(Err(err)) => match (kind, &err.kind) {
                (_, GitErrorKind::Cancelled) => {
                    note(Toast::info(t!("remote.cancelled").into_owned()), cx)
                }
                (RemoteOpKind::Pull(_), GitErrorKind::DivergentBranches) => {
                    self.ask_how_to_combine(window, cx)
                }
                (RemoteOpKind::Push(ForceMode::None), GitErrorKind::NonFastForward) => {
                    self.ask_about_rejected_push(window, cx)
                }
                _ => self.show_git_error(kind.failed_title(), &err, window, cx),
            },
            // The job panicked (it was logged): its result is lost, and the user hears of it
            // rather than nothing (#12).
            None => note(
                Toast::error(t!("job.lost").into_owned()).title(kind.failed_title()),
                cx,
            ),
        }
        // The watcher sees the changes too; refreshing here makes it immediate.
        self.refresh_refs(cx);
        self.refresh_status(cx);
        if matches!(kind, RemoteOpKind::Deepen(_)) {
            // The refs stay where they were, so the refs check wouldn't reload the
            // graph; the walk now goes further back.
            self.load(cx);
        }
        if matches!(kind, RemoteOpKind::SubmoduleUpdate) {
            self.load_submodules(cx);
        }
        self.load_extras(cx);
        cx.notify();
    }

    pub(super) fn cancel_remote(&mut self, cx: &mut Context<Self>) {
        if let Some(op) = &self.remote_op {
            op.cancel.cancel();
        }
        cx.notify();
    }

    /// Pull found new commits on both sides: merge or rebase?
    pub(super) fn ask_how_to_combine(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_remote_choice(
            t!("remote.diverged_title").into_owned(),
            t!("remote.diverged_text").into_owned(),
            vec![
                (
                    t!("remote.rebase").into_owned(),
                    RemoteOpKind::Pull(PullMode::Rebase),
                ),
                (
                    t!("remote.merge").into_owned(),
                    RemoteOpKind::Pull(PullMode::Merge),
                ),
            ],
            window,
            cx,
        );
    }

    /// The remote has commits we don't: pull them, or (after an amend or rebase) force
    /// push with lease.
    pub(super) fn ask_about_rejected_push(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_remote_choice(
            t!("remote.rejected_title").into_owned(),
            t!("remote.rejected_text").into_owned(),
            vec![
                (
                    t!("toolbar.force_push").into_owned(),
                    RemoteOpKind::Push(ForceMode::WithLease),
                ),
                (
                    t!("remote.pull_first").into_owned(),
                    RemoteOpKind::Pull(PullMode::Configured),
                ),
            ],
            window,
            cx,
        );
    }

    /// A dialog explaining what happened, with remote operations as the ways out (the
    /// last one is the suggested one).
    pub(super) fn open_remote_choice(
        &mut self,
        title: String,
        text: String,
        choices: Vec<(String, RemoteOpKind)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // `open_choice`, its buttons running the network operations.
        let choices = choices
            .into_iter()
            .map(|(label, kind)| {
                let action: ChoiceAction =
                    Rc::new(move |this, window, cx| this.run_remote(kind, window, cx));
                (label, action)
            })
            .collect();
        let cancel = t!("toolbar.cancel").into_owned();
        self.open_choice(title, text, cancel, choices, window, cx);
    }
}
