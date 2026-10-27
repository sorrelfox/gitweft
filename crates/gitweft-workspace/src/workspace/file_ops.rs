//! Files: stashes list, patches, archives, cleaning and ignore rules.

use super::*;

impl Workspace {
    pub(super) fn load_stashes(&mut self, cx: &mut Context<Self>) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let cwd = repo.cwd();
        let job = self.jobs.spawn(Lane::Coalesce("stashes"), move |cancel| {
            list_stashes(&git, &cwd, cancel).unwrap_or_default()
        });
        cx.spawn(async move |this, cx| {
            if let Some(stashes) = job.join().await {
                this.update(cx, |this, cx| {
                    if this.stashes != stashes {
                        this.stashes = stashes;
                        cx.notify();
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    /// "Apply patches…": picks patch files, then applies them.
    pub(super) fn prompt_apply_patches(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some(t!("files.apply_prompt").into_owned().into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(files))) = paths.await else {
                return;
            };
            if files.is_empty() {
                return;
            }
            this.update_in(cx, |this, window, cx| {
                this.apply_patch_files(files, window, cx)
            })
            .ok();
        })
        .detach();
    }

    /// `git format-patch` files (mailboxes) become commits with `git am` (undoable; a
    /// conflict leaves it to the operation banner); plain diffs change the files
    /// (`git apply`, not undoable). A plain diff that conflicts leaves markers and unmerged
    /// files with no operation in progress: that is said as such and the working changes
    /// are shown, where the conflicted files are listed (#34); the diffs after it aren't
    /// applied.
    pub(super) fn apply_patch_files(
        &mut self,
        files: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        /// What applying the files came to.
        enum Applied {
            /// The note to show, and an am's undo entry.
            Done(String, Option<UndoEntry>),
            /// A plain diff left conflicts in this many files.
            DiffConflicts(usize),
        }
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let cwd = repo.cwd();
        let count = files.len();
        let task = self.jobs.spawn(Lane::Write, move |cancel| {
            let (git, cwd) = (&git, cwd.as_path());
            if files.iter().all(|file| is_mailbox(file)) {
                let record = Record::checked_out(t!("undo.op_am").into_owned(), Worktree::Follow);
                let ((), entry) = recorded(git, cwd, Some(record), cancel, || {
                    apply_mailbox(git, cwd, &files, cancel)
                })?;
                let text = match count {
                    1 => t!("files.patches_applied_single"),
                    _ => tn!("files.patches_applied_multiple", count = count),
                };
                return Ok(Applied::Done(text.into_owned(), entry));
            }
            for file in &files {
                match apply_patch_file(git, cwd, file, cancel) {
                    Ok(()) => {}
                    Err(err) if err.kind == GitErrorKind::Conflict => {
                        return Ok(Applied::DiffConflicts(unmerged_count(git, cwd, cancel)?));
                    }
                    Err(err) => return Err(err),
                }
            }
            let text = match count {
                1 => t!("files.diff_applied_single"),
                _ => tn!("files.diff_applied_multiple", count = count),
            };
            Ok(Applied::Done(text.into_owned(), None))
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.join().await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Some(Ok(Applied::Done(message, entry))) => {
                        if let Some(entry) = entry {
                            this.push_undo(entry, cx);
                        }
                        note(Toast::success(message), cx);
                    }
                    Some(Ok(Applied::DiffConflicts(files))) => {
                        let text = tn!("files.diff_conflicts", count = files).into_owned();
                        note(Toast::warning(text), cx);
                        // The status that follows selects the working changes.
                        this.select_wip_when_dirty = true;
                    }
                    // A stopped am: the operation banner takes it from here.
                    Some(Err(err)) if err.kind == GitErrorKind::Conflict => this.open_choice(
                        t!("files.am_conflict_title").into_owned(),
                        t!("files.am_conflict_text").into_owned(),
                        t!("branch.ok").into_owned(),
                        Vec::new(),
                        window,
                        cx,
                    ),
                    Some(Err(err)) => {
                        this.show_git_error(t!("files.apply_failed").into_owned(), &err, window, cx)
                    }
                    None => {}
                }
                this.refresh_after_write(cx);
            })
            .ok();
        })
        .detach();
    }

    /// "Save as patch…": a save dialog in the repository's parent folder, the file named
    /// after the repository and the commit, then the commit is written there as a mailbox
    /// patch (`format-patch --stdout`).
    pub(super) fn save_patch(&mut self, id: ObjectId, window: &mut Window, cx: &mut Context<Self>) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let Some(workdir) = repo.workdir.clone() else {
            return;
        };
        let dir = workdir
            .parent()
            .map_or_else(|| workdir.clone(), Path::to_path_buf);
        let suggested = format!("{}-{}.patch", repo.name, id.to_hex_with_len(8));
        let chosen = cx.prompt_for_new_path(&dir, Some(&suggested));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(file))) = chosen.await else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.run_write_op(
                    t!("files.save_failed").into_owned(),
                    None,
                    move |git, cwd, cancel| {
                        write_patch(git, cwd, &id, &file, cancel)?;
                        Ok(t!("files.patch_saved", name = file.display()).into_owned())
                    },
                    window,
                    cx,
                )
            })
            .ok();
        })
        .detach();
    }

    /// "Save N commits as patches…": asks for a folder (GPUI's folder prompt can't be told
    /// where to start), then writes the commits there as numbered patch files, oldest first.
    pub(super) fn save_patches(
        &mut self,
        commits: Vec<RowCommit>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ids: Vec<ObjectId> = commits.iter().map(|commit| commit.id).collect();
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t!("files.save_patches_prompt").into_owned().into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(folder) = paths.into_iter().next() else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.run_write_op(
                    t!("files.save_failed").into_owned(),
                    None,
                    move |git, cwd, cancel| {
                        let files = format_patches(git, cwd, &ids, &folder, cancel)?;
                        let saved = tn!(
                            "files.patches_saved",
                            count = files.len(),
                            folder = folder.display()
                        );
                        Ok(saved.into_owned())
                    },
                    window,
                    cx,
                )
            })
            .ok();
        })
        .detach();
    }

    /// "Export as archive…": asks where (a `.tar.gz` or `.tgz` name makes a tarball,
    /// anything else a zip), then writes the commit's files there.
    pub(super) fn export_archive(
        &mut self,
        id: ObjectId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let Some(workdir) = repo.workdir.clone() else {
            return;
        };
        let dir = workdir
            .parent()
            .map_or_else(|| workdir.clone(), Path::to_path_buf);
        let suggested = format!("{}-{}.zip", repo.name, id.to_hex_with_len(8));
        let chosen = cx.prompt_for_new_path(&dir, Some(&suggested));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(file))) = chosen.await else {
                return;
            };
            let name = file.to_string_lossy().to_lowercase();
            let format = if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
                ArchiveFormat::TarGz
            } else {
                ArchiveFormat::Zip
            };
            this.update_in(cx, |this, window, cx| {
                this.run_write_op(
                    t!("files.save_failed").into_owned(),
                    None,
                    move |git, cwd, cancel| {
                        archive(git, cwd, &id, format, &file, cancel)?;
                        Ok(t!("files.archive_saved", name = file.display()).into_owned())
                    },
                    window,
                    cx,
                )
            })
            .ok();
        })
        .detach();
    }

    /// "Remove untracked files…": lists what would go (ignored files too when asked),
    /// then asks.
    pub(super) fn ask_clean(
        &mut self,
        include_ignored: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let Some(workdir) = repo.workdir.clone() else {
            return;
        };
        let git = repo.git.clone();
        let job = self
            .jobs
            .spawn(Lane::Latest("clean-preview"), move |cancel| {
                clean_candidates(&git, &workdir, include_ignored, cancel)
            });
        cx.spawn_in(window, async move |this, cx| {
            let Some(result) = job.join().await else {
                return;
            };
            this.update_in(cx, |this, window, cx| match result {
                Ok(paths) if paths.is_empty() => {
                    note(Toast::info(t!("files.nothing_to_clean").into_owned()), cx)
                }
                Ok(paths) => this.confirm_clean(paths, include_ignored, window, cx),
                Err(err) => note(
                    Toast::error(err.summary()).title(t!("files.clean_failed").into_owned()),
                    cx,
                ),
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn confirm_clean(
        &mut self,
        paths: Vec<BString>,
        include_ignored: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        const SHOWN: usize = 12;
        let mut list: Vec<String> = paths
            .iter()
            .take(SHOWN)
            .map(|path| t!("common.bullet", item = String::from_utf8_lossy(path)).into_owned())
            .collect();
        if paths.len() > SHOWN {
            list.push(tn!("files.clean_more", count = paths.len() - SHOWN).into_owned());
        }
        let title = if include_ignored {
            tn!("files.clean_title_ignored", count = paths.len())
        } else {
            tn!("files.clean_title", count = paths.len())
        }
        .into_owned();
        let text = t!("files.clean_text", files = list.join("\n")).into_owned();
        let action: ChoiceAction =
            Rc::new(move |this, window, cx| this.run_clean(paths.clone(), window, cx));
        self.open_choice(
            title,
            text,
            t!("toolbar.cancel").into_owned(),
            vec![(t!("files.clean_ok").into_owned(), action)],
            window,
            cx,
        );
    }

    /// Moves the paths to the app's trash (like discarding), so nothing is lost for good.
    pub(super) fn run_clean(
        &mut self,
        paths: Vec<BString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let Some(workdir) = repo.workdir.clone() else {
            return;
        };
        let git = repo.git.clone();
        let count = paths.len();
        let task = self.jobs.spawn(Lane::Write, move |cancel| {
            gitweft_git::ops::clean_paths(&git, &workdir, &paths, &trash_dir(), cancel)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.join().await;
            this.update_in(cx, |this, _, cx| {
                match result {
                    Some(Ok(backup)) => {
                        let dir = backup
                            .trash_dir
                            .as_ref()
                            .map(|dir| dir.display().to_string())
                            .unwrap_or_default();
                        note(
                            Toast::success(
                                tn!("files.cleaned", count = count, dir = dir).into_owned(),
                            ),
                            cx,
                        );
                    }
                    Some(Err(err)) => note(
                        Toast::error(err.summary()).title(t!("files.clean_failed").into_owned()),
                        cx,
                    ),
                    None => {}
                }
                this.refresh_status(cx);
            })
            .ok();
        })
        .detach();
    }

    /// An untracked file's ignore items: appends the rule to `.gitignore` (or, only for
    /// this clone, to `info/exclude`), in the write lane.
    pub(super) fn add_ignore(
        &mut self,
        path: BString,
        choice: IgnoreChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let Some(workdir) = repo.workdir.clone() else {
            return;
        };
        let git_dir = repo.repo.to_thread_local().common_dir().to_owned();
        let is_dir = path.ends_with(b"/");
        let trimmed: &[u8] = path.strip_suffix(b"/").unwrap_or(&path);
        let (scope, pattern) = match choice {
            IgnoreChoice::Path => (
                IgnoreScope::Shared,
                ignore_pattern_for_path(trimmed, is_dir),
            ),
            IgnoreChoice::OnlyHere => {
                (IgnoreScope::Local, ignore_pattern_for_path(trimmed, is_dir))
            }
            IgnoreChoice::Extension => match ignore_pattern_for_extension(trimmed) {
                Some(pattern) => (IgnoreScope::Shared, pattern),
                None => return,
            },
            IgnoreChoice::Folder => {
                let Some(slash) = trimmed.iter().rposition(|b| *b == b'/') else {
                    return;
                };
                (
                    IgnoreScope::Shared,
                    ignore_pattern_for_path(&trimmed[..slash], true),
                )
            }
        };
        // The file as it really is: a linked worktree's `info/exclude` is in the common git
        // folder, not in its own `.git` file's folder (#47). Shown relative to the working
        // tree when it's under it.
        let file = match scope {
            IgnoreScope::Shared => ".gitignore".to_owned(),
            IgnoreScope::Local => {
                let exclude = git_dir.join("info").join("exclude");
                exclude
                    .strip_prefix(&workdir)
                    .unwrap_or(&exclude)
                    .display()
                    .to_string()
            }
        };
        let shown = String::from_utf8_lossy(&pattern).into_owned();
        let task = self.jobs.spawn(Lane::Write, move |_| {
            add_ignore_rule(&workdir, &git_dir, scope, &pattern)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.join().await;
            this.update_in(cx, |this, _, cx| {
                match result {
                    Some(Ok(())) => note(
                        Toast::success(
                            t!("files.ignored", pattern = shown, file = file).into_owned(),
                        ),
                        cx,
                    ),
                    Some(Err(err)) => note(
                        Toast::error(err.to_string()).title(t!("files.ignore_failed").into_owned()),
                        cx,
                    ),
                    None => {}
                }
                this.refresh_status(cx);
            })
            .ok();
        })
        .detach();
    }
}
