//! Adding a submodule, and taking one out: Deinitialize keeps it in `.gitmodules` (its
//! folder is emptied), Remove also takes it out of the index (staged, to be committed).

use gitweft_git::submodules::{add_submodule, deinit_submodule, remove_submodule};

use super::*;

/// The folder a submodule gets when none is given: the URL's last part without `.git`.
fn default_path(url: &str) -> String {
    let last = url
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or_default();
    last.strip_suffix(".git").unwrap_or(last).to_owned()
}

impl Workspace {
    /// Asks for the URL and the folder, then adds the submodule.
    pub(super) fn open_add_submodule(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let spec = TextDialog {
            title: t!("submodule.add_title").into_owned(),
            subtitle: Some(t!("submodule.add_hint").into_owned()),
            placeholder: t!("submodule.add_url").into_owned(),
            initial: String::new(),
            detail: Some((t!("submodule.add_path").into_owned(), String::new())),
            checkbox: None,
            ok_label: t!("submodule.add").into_owned(),
            allow_empty: false,
            multiline: false,
        };
        let submit: SubmitText = Rc::new(|this, url, path, window, cx| {
            let url = url.trim().to_owned();
            let path = match path.trim() {
                "" => default_path(&url),
                given => given.to_owned(),
            };
            this.run_submodule_job(
                t!("submodule.add_failed").into_owned(),
                t!("submodule.added", name = path).into_owned(),
                move |git, cwd, cancel| {
                    add_submodule(git, cwd, &url, Path::new(&path), None, cancel, |_| {})
                },
                None,
                window,
                cx,
            );
        });
        self.open_text_dialog(spec, submit, window, cx);
    }

    /// Asks, then deinitializes (or removes, with `remove`) the submodule at `path` inside
    /// the repository at `superproject` (relative to the root; empty for this one).
    pub(super) fn confirm_submodule_out(
        &mut self,
        superproject: PathBuf,
        path: PathBuf,
        remove: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = superproject.join(&path).display().to_string();
        let (message, detail, ok) = if remove {
            (
                t!("submodule.remove_confirm", name = name),
                t!("submodule.remove_detail"),
                t!("submodule.remove"),
            )
        } else {
            (
                t!("submodule.deinit_confirm", name = name),
                t!("submodule.deinit_detail"),
                t!("submodule.deinit"),
            )
        };
        self.confirm(
            message.into_owned(),
            detail.into_owned(),
            ok.into_owned(),
            move |this, confirmed, window, cx| {
                if confirmed {
                    this.run_submodule_out(superproject, path, remove, false, window, cx)
                }
            },
            window,
            cx,
        );
    }

    /// Deinitializes or removes the submodule; when its work tree has changes of its own,
    /// asks before doing it anyway (`-f`, which throws them away).
    fn run_submodule_out(
        &mut self,
        superproject: PathBuf,
        path: PathBuf,
        remove: bool,
        force: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = superproject.join(&path).display().to_string();
        let (failed, done) = if remove {
            (
                t!("submodule.remove_failed"),
                t!("submodule.removed", name = name),
            )
        } else {
            (
                t!("submodule.deinit_failed"),
                t!("submodule.deinited", name = name),
            )
        };
        let retry = (!force).then(|| (superproject.clone(), path.clone(), remove));
        self.run_submodule_job(
            failed.into_owned(),
            done.into_owned(),
            move |git, cwd, cancel| {
                let folder = cwd.join(&superproject);
                if remove {
                    remove_submodule(git, &folder, &path, force, cancel)
                } else {
                    deinit_submodule(git, &folder, &path, force, cancel)
                }
            },
            retry,
            window,
            cx,
        );
    }

    /// Runs a submodule change in the write lane, then reads the submodules and the status
    /// again. `retry`: on a refusal for local changes, offer to do it with `-f`.
    fn run_submodule_job(
        &mut self,
        failed_title: String,
        done: String,
        job: impl FnOnce(&GitCli, &Path, &CancelToken) -> Result<(), GitError> + Send + 'static,
        retry: Option<(PathBuf, PathBuf, bool)>,
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
        let task = self
            .jobs
            .spawn(Lane::Write, move |cancel| job(&git, &cwd, cancel));
        cx.spawn_in(window, async move |this, cx| {
            let result = task.join().await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Some(Ok(())) => note(Toast::success(done), cx),
                    Some(Err(err)) if err.kind == GitErrorKind::SubmoduleModified => {
                        if let Some((superproject, path, remove)) = retry {
                            // Git says the same for a submodule only added (staged, not
                            // committed): its first line tells them apart.
                            let staged = err.stderr.contains("changes staged in the index");
                            this.confirm_submodule_force(
                                superproject,
                                path,
                                remove,
                                staged,
                                window,
                                cx,
                            );
                        } else {
                            this.show_git_error(failed_title, &err, window, cx);
                        }
                    }
                    Some(Err(err)) => this.show_git_error(failed_title, &err, window, cx),
                    None => {}
                }
                this.load_submodules(cx);
                this.refresh_status(cx);
            })
            .ok();
        })
        .detach();
    }

    /// The submodule has changes of its own, or (`staged`) its addition isn't committed
    /// yet: asks before going on anyway.
    fn confirm_submodule_force(
        &mut self,
        superproject: PathBuf,
        path: PathBuf,
        remove: bool,
        staged: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = superproject.join(&path).display().to_string();
        let (message, detail) = if staged {
            (
                t!("submodule.staged", name = name),
                t!("submodule.staged_detail"),
            )
        } else {
            (
                t!("submodule.modified", name = name),
                t!("submodule.modified_detail"),
            )
        };
        let (message, detail) = (message.into_owned(), detail.into_owned());
        self.confirm(
            message,
            detail,
            if staged {
                t!("submodule.force_staged").into_owned()
            } else {
                t!("submodule.force").into_owned()
            },
            move |this, confirmed, window, cx| {
                if confirmed {
                    this.run_submodule_out(superproject, path, remove, true, window, cx)
                }
            },
            window,
            cx,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::default_path;

    #[test]
    fn a_submodule_folder_from_its_url() {
        assert_eq!(default_path("https://example.com/team/lib.git"), "lib");
        assert_eq!(default_path("git@example.com:team/tool.git/"), "tool");
        assert_eq!(default_path("/srv/repos/core"), "core");
        assert_eq!(default_path("../sibling.git"), "sibling");
    }
}
