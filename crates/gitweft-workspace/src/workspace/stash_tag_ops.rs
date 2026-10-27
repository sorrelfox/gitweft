//! Stashes and tags: stashing, applying, branching from and dropping stashes; creating, pushing and deleting tags.

use super::*;

impl Workspace {
    /// The "Stash changes" dialog: an optional message, untracked files included by default.
    /// With `paths`, only those files are stashed.
    pub(super) fn open_stash_dialog(
        &mut self,
        paths: Vec<BString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let include_untracked = Rc::new(Cell::new(true));
        let flag = include_untracked.clone();
        let title = match paths.as_slice() {
            [] => t!("stash.new_title").into_owned(),
            [path] => t!("stash.new_title_path", path = String::from_utf8_lossy(path)).into_owned(),
            _ => tn!("stash.new_title_paths", count = paths.len()).into_owned(),
        };
        let submit: SubmitText = Rc::new(move |this, message, _, window, cx| {
            this.stash_changes(message, flag.get(), paths.clone(), window, cx)
        });
        let spec = TextDialog {
            title,
            subtitle: None,
            placeholder: t!("stash.message_placeholder").into_owned(),
            initial: String::new(),
            detail: None,
            checkbox: Some((
                t!("stash.include_untracked").into_owned(),
                include_untracked,
            )),
            ok_label: t!("stash.save").into_owned(),
            allow_empty: true,
            multiline: false,
        };
        self.open_text_dialog(spec, submit, window, cx);
    }

    pub(super) fn stash_changes(
        &mut self,
        message: String,
        include_untracked: bool,
        paths: Vec<BString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let options = StashOptions {
            message: Some(message.trim().to_owned()).filter(|m| !m.is_empty()),
            include_untracked,
            keep_index: false,
            paths,
        };
        // A stash of some files says which.
        let saved_text = match options.paths.as_slice() {
            [] => t!("stash.saved").into_owned(),
            [path] => t!("stash.saved_path", path = String::from_utf8_lossy(path)).into_owned(),
            paths => tn!("stash.saved_paths", count = paths.len()).into_owned(),
        };
        self.run_write_op(
            t!("stash.failed").into_owned(),
            None,
            move |git, cwd, cancel| {
                let saved = stash_push(git, cwd, &options, cancel)?;
                Ok(if saved {
                    saved_text
                } else {
                    t!("stash.nothing").into_owned()
                })
            },
            window,
            cx,
        );
    }

    pub(super) fn pop_latest_stash(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(entry) = self.stashes.first().cloned() {
            self.apply_stash(entry, true, window, cx);
        }
    }

    pub(super) fn apply_stash(
        &mut self,
        entry: StashEntry,
        pop: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let conflict = Some((
            t!("stash.conflict_title").into_owned(),
            t!("stash.conflict_text").into_owned(),
        ));
        self.run_write_op(
            t!("stash.failed").into_owned(),
            conflict,
            move |git, cwd, cancel| {
                stash_apply(git, cwd, &entry, pop, cancel)?;
                let name = entry.revision();
                Ok(if pop {
                    t!("stash.popped", name = name)
                } else {
                    t!("stash.applied", name = name)
                }
                .into_owned())
            },
            window,
            cx,
        );
    }

    /// "Create branch from stash…": a new branch at the commit the stash was made on, with
    /// the stash applied there (its index too) and dropped. It switches branches, so it
    /// isn't undoable.
    pub(super) fn open_stash_branch(
        &mut self,
        entry: StashEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let spec = TextDialog {
            title: t!("stash.branch_title", name = entry.revision()).into_owned(),
            subtitle: Some(stash_label(&entry)),
            placeholder: t!("stash.branch_placeholder").into_owned(),
            initial: String::new(),
            detail: None,
            checkbox: None,
            ok_label: t!("stash.branch_ok").into_owned(),
            allow_empty: false,
            multiline: false,
        };
        let submit: SubmitText = Rc::new(move |this, name, _, window, cx| {
            let (entry, branch) = (entry.clone(), name.trim().to_owned());
            this.run_write_op(
                t!("stash.failed").into_owned(),
                None,
                move |git, cwd, cancel| {
                    // Local changes in the way: git may have created and switched to the
                    // branch already, and it kept the stash. Say so instead of git's text.
                    stash_branch(git, cwd, &entry, &branch, cancel).map_err(|err| {
                        match err.kind {
                            GitErrorKind::WouldOverwrite => GitError::new(
                                GitErrorKind::WouldOverwrite,
                                "git stash branch",
                                t!("stash.branch_blocked", branch = branch).into_owned(),
                            ),
                            _ => err,
                        }
                    })?;
                    Ok(t!("stash.branched", branch = branch, name = entry.revision()).into_owned())
                },
                window,
                cx,
            );
        });
        self.open_text_dialog(spec, submit, window, cx);
    }

    /// Shows a stash in the right panel: what it holds, each file opening its diff.
    pub(super) fn show_stash(&mut self, entry: StashEntry, cx: &mut Context<Self>) {
        if self.showing_wip {
            self.showing_wip = false;
            cx.notify();
        }
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let cwd = repo.cwd();
        let job = self
            .jobs
            .spawn(Lane::Latest("commit-details"), move |cancel| {
                let label = stash_label(&entry);
                stash_contents(&git, &cwd, &entry, cancel)
                    .map(|contents| StashDetails {
                        entry,
                        label,
                        contents,
                    })
                    .map_err(|err| err.summary().to_string())
            });
        cx.spawn(async move |this, cx| {
            let Some(result) = job.join().await else {
                return;
            };
            let state = match result {
                Ok(stash) => DetailsState::Stash(Box::new(stash)),
                Err(message) => DetailsState::Failed(message),
            };
            this.update(cx, |this, cx| {
                if !this.showing_wip {
                    this.details_view
                        .update(cx, |details, cx| details.set_state(state, cx));
                }
            })
            .ok();
        })
        .detach();
    }

    /// Asks natively before dropping a stash.
    pub(super) fn confirm_drop_stash(
        &mut self,
        entry: StashEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let message = t!("stash.drop_confirm", name = entry.revision()).into_owned();
        let detail = t!("stash.drop_confirm_detail").into_owned();
        self.confirm(
            message,
            detail,
            t!("stash.drop")
                .into_owned()
                .trim_end_matches('…')
                .to_owned(),
            move |this, confirmed, window, cx| {
                if confirmed {
                    this.run_write_op(
                        t!("stash.failed").into_owned(),
                        None,
                        move |git, cwd, cancel| {
                            stash_drop(git, cwd, &entry, cancel)?;
                            Ok(t!("stash.dropped", name = entry.revision()).into_owned())
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

    /// The "New tag" dialog: a name, and a message that makes the tag annotated.
    pub(super) fn open_new_tag(
        &mut self,
        target: ObjectId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let submit: SubmitText = Rc::new(move |this, name, message, window, cx| {
            let name = name.trim().to_owned();
            let record = Record::new(
                t!("undo.op_create_tag", name = name).into_owned(),
                format!("refs/tags/{name}"),
            );
            this.run_recorded_op(
                t!("tag.failed").into_owned(),
                None,
                Some(record),
                move |git, cwd, cancel| {
                    if !is_valid_tag_name(git, cwd, &name, cancel) {
                        let message = t!("tag.invalid_name", name = name).into_owned();
                        return Err(GitError::new(GitErrorKind::Other, "git tag", message));
                    }
                    create_tag(git, cwd, &name, &target.to_string(), Some(&message), cancel)?;
                    Ok(t!("tag.created", name = name).into_owned())
                },
                window,
                cx,
            );
        });
        let spec = TextDialog {
            title: t!("tag.new_title").into_owned(),
            subtitle: Some(t!("branch.new_from", start = target.to_hex_with_len(8)).into_owned()),
            placeholder: t!("tag.name_placeholder").into_owned(),
            initial: String::new(),
            detail: Some((t!("tag.message_placeholder").into_owned(), String::new())),
            checkbox: None,
            ok_label: t!("tag.create").into_owned(),
            allow_empty: false,
            multiline: false,
        };
        self.open_text_dialog(spec, submit, window, cx);
    }

    pub(super) fn push_tag_to(
        &mut self,
        remote: String,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_write_op(
            t!("tag.failed").into_owned(),
            None,
            move |git, cwd, cancel| {
                push_tag(git, cwd, &remote, &name, cancel, |_| {})?;
                Ok(t!("tag.pushed", name = name, remote = remote).into_owned())
            },
            window,
            cx,
        );
    }

    pub(super) fn delete_tag_on_remote(
        &mut self,
        remote: String,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_write_op(
            t!("tag.failed").into_owned(),
            None,
            move |git, cwd, cancel| {
                delete_remote_tag(git, cwd, &remote, &name, cancel)?;
                Ok(t!("tag.deleted_remote", name = name, remote = remote).into_owned())
            },
            window,
            cx,
        );
    }

    /// Asks natively before deleting a (local) tag.
    pub(super) fn confirm_delete_tag(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let message = t!("tag.delete_confirm", name = name).into_owned();
        let detail = t!("tag.delete_confirm_detail").into_owned();
        self.confirm(
            message,
            detail,
            t!("branch.delete").into_owned(),
            move |this, confirmed, window, cx| {
                if confirmed {
                    let record = Record::new(
                        t!("undo.op_delete_tag", name = name).into_owned(),
                        format!("refs/tags/{name}"),
                    );
                    this.run_recorded_op(
                        t!("tag.failed").into_owned(),
                        None,
                        Some(record),
                        move |git, cwd, cancel| {
                            delete_tag(git, cwd, &name, cancel)?;
                            Ok(t!("tag.deleted", name = name).into_owned())
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
