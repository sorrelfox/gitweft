//! Branches: switching, creating, renaming and deleting, and the dialogs for them.

use super::*;

impl Workspace {
    /// A revision or ref name as git gets it: a name encoded by `undo::ref_name_text` (one
    /// that isn't UTF-8) as its bytes, any other text as it is.
    pub(super) fn git_arg(text: &str) -> std::ffi::OsString {
        gitweft_platform::bytes::os_string_from_vec(gitweft_git::undo::ref_name_bytes(text))
    }

    /// The name git has for the local branch shown as `shown`: its bytes, since the
    /// text shown is lossy for a name that isn't UTF-8; the text itself when no branch
    /// shows so. `Err` when several branches show alike.
    pub(super) fn local_branch_bytes(&self, shown: &str) -> Result<Vec<u8>, ()> {
        let RepoState::Loaded(repo) = &self.state else {
            return Ok(shown.as_bytes().to_vec());
        };
        let mut found = repo
            .refs
            .local_branches
            .iter()
            .filter(|entry| entry.short_name == shown);
        match (found.next(), found.next()) {
            (Some(entry), None) => {
                let full = entry.full_name.as_slice();
                Ok(full.strip_prefix(b"refs/heads/").unwrap_or(full).to_vec())
            }
            (Some(_), Some(_)) => Err(()),
            (None, _) => Ok(shown.as_bytes().to_vec()),
        }
    }

    /// Runs a branch operation in the write lane. The outcome is a notification or, for
    /// refusals that have a way out, a dialog.
    pub(super) fn run_branch_op(
        &mut self,
        op: BranchOp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let cwd = repo.cwd();
        // A remote branch by its bytes (`refs/remotes/<remote>/` off its full name), and
        // whether a local branch has exactly those bytes.
        let remote_branch_bytes: Option<Vec<u8>> = match &op {
            BranchOp::SwitchRemote { remote, branch } => {
                let shown = format!("{remote}/{branch}");
                let prefix = format!("refs/remotes/{remote}/");
                repo.refs
                    .remote_branches
                    .iter()
                    .find(|entry| entry.short_name == shown)
                    .and_then(|entry| entry.full_name.strip_prefix(prefix.as_bytes()))
                    .map(<[u8]>::to_vec)
            }
            _ => None,
        };
        let local_exists = match &op {
            BranchOp::SwitchRemote { branch, .. } => {
                let bytes = remote_branch_bytes.as_deref().unwrap_or(branch.as_bytes());
                repo.refs
                    .local_branches
                    .iter()
                    .any(|entry| entry.full_name.strip_prefix(b"refs/heads/") == Some(bytes))
            }
            _ => false,
        };
        // A branch's name for git: its bytes, not the text shown, which git refuses for a
        // name that isn't UTF-8. Names that show alike can't be told apart: refused.
        let shown = match &op {
            BranchOp::Switch { name } | BranchOp::Delete { name, .. } => Some(name.as_str()),
            BranchOp::Rename { from, .. } => Some(from.as_str()),
            _ => None,
        };
        let mut job_git_name: Option<Vec<u8>> = None;
        if let Some(shown) = shown {
            let mut found = repo
                .refs
                .local_branches
                .iter()
                .filter(|entry| entry.short_name == shown);
            match (found.next(), found.next()) {
                (Some(entry), None) => {
                    let full = entry.full_name.as_slice();
                    job_git_name = Some(full.strip_prefix(b"refs/heads/").unwrap_or(full).to_vec());
                }
                (Some(_), Some(_)) => {
                    let text = t!("branch.ambiguous_name", name = shown).into_owned();
                    note(Toast::error(text), cx);
                    return;
                }
                (None, _) => {}
            }
        }
        let job_op = op.clone();
        // Creating a branch (without switching to it) and deleting one can be undone.
        let record = match &op {
            BranchOp::Create {
                name,
                switch: false,
                ..
            } => Some(Record::branch(
                t!("undo.op_create_branch", name = name).into_owned(),
                format!("refs/heads/{name}"),
            )),
            // The branch's bytes when it was found: its undo must name it as git does.
            BranchOp::Delete { name, .. } => Some(Record::branch(
                t!("undo.op_delete_branch", name = name).into_owned(),
                job_git_name.as_deref().map_or_else(
                    || format!("refs/heads/{name}"),
                    |bytes| {
                        gitweft_git::undo::ref_name_text(
                            &[b"refs/heads/".as_slice(), bytes].concat(),
                        )
                    },
                ),
            )),
            _ => None,
        };
        let job = self.jobs.spawn(
            Lane::Write,
            move |cancel| -> Result<(String, Option<UndoEntry>), GitError> {
                let check_name = |name: &str| {
                    if is_valid_branch_name(&git, &cwd, name, cancel) {
                        Ok(())
                    } else {
                        let message = t!("branch.invalid_name", name = name).into_owned();
                        Err(GitError::new(GitErrorKind::Other, "git branch", message))
                    }
                };
                // The branch's name as git has it (bytes), else as it was given.
                let git_name = |given: &str| -> std::ffi::OsString {
                    job_git_name
                        .clone()
                        .map_or_else(|| given.into(), gitweft_platform::bytes::os_string_from_vec)
                };
                let run = || -> Result<String, GitError> {
                    let message = match job_op {
                        BranchOp::Create {
                            name,
                            start,
                            switch,
                        } => {
                            check_name(&name)?;
                            create_branch(&git, &cwd, &name, start.as_deref(), switch, cancel)?;
                            if switch {
                                t!("branch.switched", name = name)
                            } else {
                                t!("branch.created", name = name)
                            }
                        }
                        BranchOp::Rename { from, to } => {
                            check_name(&to)?;
                            rename_branch(&git, &cwd, git_name(&from), &to, cancel)?;
                            t!("branch.renamed", name = to)
                        }
                        BranchOp::Delete { name, force } => {
                            delete_branch(&git, &cwd, git_name(&name), force, cancel)?;
                            t!("branch.deleted", name = name)
                        }
                        BranchOp::Switch { name } => {
                            switch_branch(&git, &cwd, git_name(&name), cancel)?;
                            t!("branch.switched", name = name)
                        }
                        BranchOp::SwitchRemote { remote, branch } => {
                            let name = match &remote_branch_bytes {
                                Some(bytes) => gitweft_platform::bytes::os_str_from_bytes(bytes),
                                None => std::ffi::OsStr::new(branch.as_str()).into(),
                            };
                            let local = switch_to_remote_branch(
                                &git,
                                &cwd,
                                &remote,
                                name,
                                local_exists,
                                cancel,
                            )?;
                            t!("branch.switched", name = local)
                        }
                        BranchOp::Detach { commit } => {
                            switch_detached(&git, &cwd, &commit.to_string(), cancel)?;
                            t!("head.detached", sha = commit.to_hex_with_len(8))
                        }
                    };
                    Ok(message.into_owned())
                };
                recorded(&git, &cwd, record, cancel, run)
            },
        );
        cx.spawn_in(window, async move |this, cx| {
            let result = job.join().await;
            this.update_in(cx, |this, window, cx| {
                let result = result.map(|result| this.take_entry(result, cx));
                this.branch_finished(op, result, window, cx)
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn branch_finished(
        &mut self,
        op: BranchOp,
        result: Option<Result<String, GitError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Work waiting for this switch runs once it has happened, and never after a failure.
        let after = match &op {
            BranchOp::Switch { name } => self
                .after_switch
                .take()
                .filter(|(branch, _)| branch == name)
                .map(|(_, then)| then),
            _ => None,
        };
        match result {
            Some(Ok(message)) => {
                note(Toast::success(message), cx);
                if let Some(then) = after {
                    then(self, window, cx);
                }
            }
            Some(Err(err)) => match (&op, &err.kind) {
                (BranchOp::Delete { name, force: false }, GitErrorKind::NotFullyMerged) => {
                    let branch = name.clone();
                    let delete: ChoiceAction = Rc::new(move |this, window, cx| {
                        let op = BranchOp::Delete {
                            name: branch.clone(),
                            force: true,
                        };
                        this.run_branch_op(op, window, cx)
                    });
                    self.open_choice(
                        t!("branch.unmerged_title", name = name).into_owned(),
                        t!("branch.unmerged_text").into_owned(),
                        t!("toolbar.cancel").into_owned(),
                        vec![(t!("branch.delete_anyway").into_owned(), delete)],
                        window,
                        cx,
                    );
                }
                (_, GitErrorKind::WouldOverwrite) => self.open_choice(
                    t!("branch.would_overwrite_title").into_owned(),
                    t!("branch.would_overwrite_text").into_owned(),
                    t!("branch.ok").into_owned(),
                    Vec::new(),
                    window,
                    cx,
                ),
                _ => self.show_git_error(t!("branch.failed").into_owned(), &err, window, cx),
            },
            None => {}
        }
        self.refresh_refs(cx);
        self.refresh_status(cx);
        cx.notify();
    }

    /// Asks (natively, like discarding) before deleting a branch.
    pub(super) fn confirm_delete_branch(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let message = t!("branch.delete_confirm", name = name).into_owned();
        let detail = t!("branch.delete_confirm_detail").into_owned();
        self.confirm(
            message,
            detail,
            t!("branch.delete").into_owned(),
            move |this, confirmed, window, cx| {
                if confirmed {
                    this.run_branch_op(BranchOp::Delete { name, force: false }, window, cx)
                }
            },
            window,
            cx,
        );
    }

    /// The "New branch" dialog. `start` is (revision, how to show it); HEAD when `None`.
    pub(super) fn open_new_branch(
        &mut self,
        start: Option<(String, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let switch = Rc::new(Cell::new(true));
        let subtitle = start
            .as_ref()
            .map(|(_, shown)| t!("branch.new_from", start = shown).into_owned());
        // A start that's a local branch whose name isn't UTF-8 goes by its commit, the
        // same start: its name, shown lossily, names nothing to git.
        let revision = start.map(|(revision, _)| {
            let RepoState::Loaded(repo) = &self.state else {
                return revision;
            };
            repo.refs
                .local_branches
                .iter()
                .find(|entry| {
                    std::str::from_utf8(&entry.full_name).is_err()
                        && String::from_utf8_lossy(&entry.full_name) == revision.as_str()
                })
                .map_or(revision, |entry| entry.target.to_string())
        });
        let submit_switch = switch.clone();
        let submit: SubmitText = Rc::new(move |this, name, _, window, cx| {
            let op = BranchOp::Create {
                name: name.trim().to_owned(),
                start: revision.clone(),
                switch: submit_switch.get(),
            };
            this.run_branch_op(op, window, cx);
        });
        let spec = TextDialog {
            title: t!("branch.new_title").into_owned(),
            subtitle,
            placeholder: t!("branch.name_placeholder").into_owned(),
            initial: String::new(),
            detail: None,
            checkbox: Some((t!("branch.switch_to_new").into_owned(), switch)),
            ok_label: t!("branch.create").into_owned(),
            allow_empty: false,
            multiline: false,
        };
        self.open_text_dialog(spec, submit, window, cx);
    }

    pub(super) fn open_rename_branch(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let from = name.clone();
        let submit: SubmitText = Rc::new(move |this, to, _, window, cx| {
            let to = to.trim().to_owned();
            if to != from {
                let op = BranchOp::Rename {
                    from: from.clone(),
                    to,
                };
                this.run_branch_op(op, window, cx);
            }
        });
        let spec = TextDialog {
            title: t!("branch.rename_title", name = name).into_owned(),
            subtitle: None,
            placeholder: t!("branch.name_placeholder").into_owned(),
            initial: name,
            detail: None,
            checkbox: None,
            ok_label: t!("branch.rename_button").into_owned(),
            allow_empty: false,
            multiline: false,
        };
        self.open_text_dialog(spec, submit, window, cx);
    }
}
