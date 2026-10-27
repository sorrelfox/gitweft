//! Rewriting history: cherry-pick, revert and reset, with the published-commits check.

use super::*;

impl Workspace {
    /// The commit box's Commit. Amending a commit that a remote has asks first.
    pub(super) fn commit_checked(
        &mut self,
        message: String,
        options: CommitOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Git would refuse without a name and email: ask for them, then commit.
        let identity = self.staging_view.read(cx).identity();
        if identity.name.is_none() || identity.email.is_none() {
            self.staging_view
                .update(cx, |staging, cx| staging.commit_cancelled(cx));
            self.open_identity_dialog(Some((message, options)), window, cx);
            return;
        }
        if !options.amend {
            self.run_commit(message, options, cx);
            return;
        }
        // HEAD alone: `^HEAD^@` leaves out all of its parents (none for a root commit).
        self.check_published(
            "HEAD".to_owned(),
            Some("HEAD^@".to_owned()),
            move |this, published, window, cx| {
                if published.is_empty() {
                    this.run_commit(message, options, cx);
                    return;
                }
                let remotes = remote_list(&published);
                this.confirm(
                    t!("rewrite.amend_confirm", remotes = remotes).into_owned(),
                    t!("rewrite.amend_detail").into_owned(),
                    t!("rewrite.amend_anyway").into_owned(),
                    move |this, confirmed, _, cx| {
                        if confirmed {
                            this.run_commit(message, options, cx);
                        } else {
                            this.staging_view
                                .update(cx, |staging, cx| staging.commit_cancelled(cx));
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

    /// A commit dropped on the checked-out one: cherry-pick it once confirmed, since a
    /// drag can end in the wrong place.
    pub(super) fn confirm_drop_pick(
        &mut self,
        commit: RowCommit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let branch = head_label(&repo.refs.head);
        let summary = commit.summary.as_str();
        let message = t!(
            "rewrite.drop_pick_confirm",
            summary = summary,
            branch = branch
        )
        .into_owned();
        let detail = t!("rewrite.drop_pick_detail", branch = branch).into_owned();
        self.ask(
            PromptLevel::Info,
            message,
            detail,
            t!("rewrite.cherry_pick_button").into_owned(),
            move |this, confirmed, window, cx| {
                if confirmed {
                    this.pick_commit(commit, false, window, cx);
                }
            },
            window,
            cx,
        );
    }

    /// Row menu: cherry-picks `commit` onto the checked-out branch, or reverts it. A merge
    /// commit is taken against its first parent. A conflict leaves the operation in
    /// progress for the banner; a commit whose change is already there makes nothing.
    pub(super) fn pick_commit(
        &mut self,
        commit: RowCommit,
        reverting: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pick_commits(vec![commit], reverting, window, cx);
    }

    /// "Cherry-pick without committing": the commit's changes applied and staged, the
    /// branch left where it is (so there's no step to undo: the changes are the result).
    pub(super) fn pick_without_commit(
        &mut self,
        commit: RowCommit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let summary = commit.summary.as_str();
        let done = t!("rewrite.picked_no_commit", summary = summary).into_owned();
        let conflict = Some((
            t!("rewrite.conflict_title", summary = summary).into_owned(),
            t!("rewrite.pick_no_commit_conflict").into_owned(),
        ));
        let ids = [commit.id];
        let mainline = commit.merge.then_some(1);
        self.run_write_op(
            t!("rewrite.pick_failed").into_owned(),
            conflict,
            move |git, cwd, cancel| {
                rewrite::cherry_pick_no_commit(git, cwd, &ids, mainline, cancel).map_err(
                    |mut err| {
                        if err.kind == GitErrorKind::WouldOverwrite {
                            err.stderr = t!("rewrite.local_changes").into_owned();
                        }
                        err
                    },
                )?;
                Ok(done)
            },
            window,
            cx,
        );
    }

    /// Cherry-picks `commits` (oldest first) onto the checked-out branch, or reverts them.
    /// Merge commits are taken against their first parent (`-m 1`, which git ignores for
    /// the others); the checked-out commit itself isn't picked. A conflict leaves the
    /// operation in progress for the banner; changes already on the branch are skipped.
    pub(super) fn pick_commits(
        &mut self,
        mut commits: Vec<RowCommit>,
        reverting: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        if !reverting {
            let head = head_oid(&repo.refs.head);
            commits.retain(|commit| Some(commit.id) != head);
        }
        let count = commits.len();
        let Some(first) = commits.first() else {
            return;
        };
        let summary = first.summary.as_str();
        let (label, failed, done, empty) = match (reverting, count) {
            (true, _) => (
                t!("undo.op_revert", summary = summary),
                t!("rewrite.revert_failed"),
                t!("rewrite.reverted", summary = summary),
                t!("rewrite.revert_empty", summary = summary),
            ),
            (false, 1) => (
                t!("undo.op_cherry_pick", summary = summary),
                t!("rewrite.pick_failed"),
                t!("rewrite.picked", summary = summary),
                t!("rewrite.pick_empty", summary = summary),
            ),
            (false, _) => (
                tn!("undo.op_cherry_pick_multiple", count = count),
                t!("rewrite.pick_failed"),
                tn!("rewrite.picked_multiple", count = count),
                t!("rewrite.pick_empty_multiple"),
            ),
        };
        let (done, empty) = (done.into_owned(), empty.into_owned());
        let title = if count == 1 {
            t!("rewrite.conflict_title", summary = summary)
        } else {
            tn!("rewrite.conflict_title_multiple", count = count)
        };
        let conflict = Some((title.into_owned(), t!("rewrite.conflict_text").into_owned()));
        let record = Record::checked_out(label.into_owned(), Worktree::Follow);
        let ids: Vec<ObjectId> = commits.iter().map(|commit| commit.id).collect();
        let mainline = commits.iter().any(|commit| commit.merge).then_some(1);
        self.run_recorded_op(
            failed.into_owned(),
            conflict,
            Some(record),
            move |git, cwd, cancel| {
                let outcome = if reverting {
                    rewrite::revert(git, cwd, &ids, mainline, cancel)
                } else {
                    rewrite::cherry_pick(git, cwd, &ids, mainline, cancel)
                };
                // Git says "your local changes would be overwritten" even for a staged
                // file the commit doesn't touch; say what to do instead.
                let outcome = outcome.map_err(|mut err| {
                    if err.kind == GitErrorKind::WouldOverwrite {
                        err.stderr = t!("rewrite.local_changes").into_owned();
                    }
                    err
                })?;
                Ok(match outcome.skipped {
                    0 => done,
                    skipped if skipped >= count => empty,
                    skipped => {
                        let picked = count - skipped;
                        tn!("rewrite.picked_some", count = count, picked = picked).into_owned()
                    }
                })
            },
            window,
            cx,
        );
    }

    /// Several commits dropped on the checked-out one: cherry-pick them once confirmed.
    pub(super) fn confirm_drop_picks(
        &mut self,
        commits: Vec<RowCommit>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let branch = head_label(&repo.refs.head);
        let count = commits.len();
        self.ask(
            PromptLevel::Info,
            tn!("rewrite.drop_picks_confirm", count = count, branch = branch).into_owned(),
            t!("rewrite.drop_picks_detail", branch = branch).into_owned(),
            t!("rewrite.cherry_pick_button").into_owned(),
            move |this, confirmed, window, cx| {
                if confirmed {
                    this.pick_commits(commits, false, window, cx);
                }
            },
            window,
            cx,
        );
    }

    /// Row menu: moves the checked-out branch to `commit`. A hard reset asks first, and
    /// so does one that leaves behind commits a remote has.
    pub(super) fn reset_to(
        &mut self,
        commit: RowCommit,
        mode: ResetMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keep = commit.id.to_string();
        self.check_published(
            "HEAD".to_owned(),
            Some(keep),
            move |this, published, window, cx| {
                this.confirm_reset(commit, mode, published, window, cx)
            },
            window,
            cx,
        );
    }

    /// One confirmation covering both reasons to ask: the files (hard) and the remote.
    pub(super) fn confirm_reset(
        &mut self,
        commit: RowCommit,
        mode: ResetMode,
        published: Published,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let hard = mode == ResetMode::Hard;
        if !hard && published.is_empty() {
            self.run_reset(commit, mode, window, cx);
            return;
        }
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let branch = head_label(&repo.refs.head);
        let sha = commit.id.to_hex_with_len(8).to_string();
        let (message, button) = if hard {
            (
                t!("rewrite.hard_confirm", branch = branch, sha = sha),
                t!("rewrite.reset"),
            )
        } else {
            (
                t!("rewrite.reset_confirm", branch = branch, sha = sha),
                t!("rewrite.reset_anyway"),
            )
        };
        let files = if hard {
            t!("rewrite.hard_confirm_detail").into_owned()
        } else {
            String::new()
        };
        let detail = if published.is_empty() {
            files
        } else {
            if files.is_empty() {
                published_text(&published)
            } else {
                t!(
                    "common.two_sentences",
                    first = published_text(&published),
                    second = files
                )
                .into_owned()
            }
        };
        self.confirm(
            message.into_owned(),
            detail,
            button.into_owned(),
            move |this, confirmed, window, cx| {
                if confirmed {
                    this.run_reset(commit, mode, window, cx);
                }
            },
            window,
            cx,
        );
    }

    pub(super) fn run_reset(
        &mut self,
        commit: RowCommit,
        mode: ResetMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let branch = head_label(&repo.refs.head);
        let sha = commit.id.to_hex_with_len(8).to_string();
        let label = t!("undo.op_reset", branch = branch, sha = sha).into_owned();
        let done = t!("rewrite.reset_done", branch = branch, sha = sha).into_owned();
        let (done_branch, done_sha) = (branch.clone(), sha.clone());
        let trash_root = trash_dir();
        self.run_entry_op(
            t!("rewrite.reset_failed").into_owned(),
            None,
            move |git, cwd, cancel| {
                let target = commit.id.to_string();
                let (backup, entry) =
                    recorded_reset(git, cwd, &target, mode, &trash_root, label, cancel)?;
                let message = match backup.trash_dir {
                    Some(dir) => {
                        let dir = dir.display().to_string();
                        t!(
                            "rewrite.reset_done_trashed",
                            branch = done_branch,
                            sha = done_sha,
                            dir = dir
                        )
                        .into_owned()
                    }
                    None => done,
                };
                Ok((message, entry))
            },
            window,
            cx,
        );
    }
}
