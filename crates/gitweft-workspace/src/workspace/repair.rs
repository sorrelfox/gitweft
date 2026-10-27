//! Getting past git's refusals: a lock file left behind by a git that crashed, a folder git
//! doesn't trust (it belongs to another user), and a commit identity git doesn't know.

use std::time::Duration;

use gitweft_git::ops::CommitOptions;
use gitweft_git::repair::{self, LockInfo};

use super::*;

/// How long ago a lock file was made.
fn age_text(age: Option<Duration>) -> String {
    let Some(age) = age else {
        return t!("repair.age_unknown").into_owned();
    };
    crate::util::age_text(age.as_secs())
}

impl Workspace {
    /// Shows a failed git command. A lock left behind, an untrusted folder and an unknown
    /// identity each offer their way out; anything else is a notification titled `title`.
    pub(super) fn show_git_error(
        &mut self,
        title: String,
        err: &GitError,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A tab that isn't shown says so when it comes forward.
        if !crate::tab_notes::is_active(cx.entity_id(), cx) {
            // Only the refusals that ask something (else it's a note, kept as such).
            if matches!(
                err.kind,
                GitErrorKind::LockHeld { .. }
                    | GitErrorKind::DubiousOwnership
                    | GitErrorKind::IdentityUnknown
            ) {
                crate::tab_notes::hold_question(cx);
            }
            let err = err.clone();
            self.with_window(
                move |this, window, cx| this.show_git_error(title, &err, window, cx),
                cx,
            );
            return;
        }
        match &err.kind {
            GitErrorKind::LockHeld { path } => {
                self.offer_lock_removal(PathBuf::from(path), window, cx)
            }
            GitErrorKind::DubiousOwnership => {
                let path = repair::untrusted_folder(&err.stderr, self.current_path.as_deref())
                    .or_else(|| self.current_path.clone());
                if let Some(path) = path {
                    self.untrusted = Some(path.clone());
                    self.offer_trust(path, window, cx);
                }
            }
            GitErrorKind::IdentityUnknown => self.open_identity_dialog(None, window, cx),
            _ => note(Toast::error(err.summary()).title(title), cx),
        }
    }

    /// The repository's git dirs (where its locks are), and every folder a git working on
    /// it may run in (those and the working tree).
    fn lock_dirs(&self) -> Option<(Vec<PathBuf>, Vec<PathBuf>)> {
        let RepoState::Loaded(repo) = &self.state else {
            return None;
        };
        let local = repo.repo.to_thread_local();
        let mut git_dirs = vec![local.git_dir().to_owned()];
        let common = local.common_dir().to_owned();
        if !git_dirs.contains(&common) {
            git_dirs.push(common);
        }
        let mut dirs = git_dirs.clone();
        dirs.extend(repo.workdir.clone());
        Some((git_dirs, dirs))
    }

    /// Looks at the lock git reported (off the UI thread: it reads /proc), then explains it:
    /// gone already, held by a git running now, or left behind (with a way to remove it).
    fn offer_lock_removal(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some((git_dirs, dirs)) = self.lock_dirs() else {
            return;
        };
        let job = self.jobs.spawn(Lane::Latest("lock-check"), move |_| {
            repair::inspect_lock(&path, &dirs)
        });
        cx.spawn_in(window, async move |this, cx| {
            let Some(info) = job.join().await else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.explain_lock(info, git_dirs, window, cx)
            })
            .ok();
        })
        .detach();
    }

    fn explain_lock(
        &mut self,
        info: LockInfo,
        git_dirs: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = t!("repair.lock_title").into_owned();
        let path = info.path.display().to_string();
        let ok = t!("branch.ok").into_owned();
        if !info.exists {
            let text = t!("repair.lock_gone", path = path).into_owned();
            self.open_choice(title, text, ok, Vec::new(), window, cx);
            return;
        }
        if !info.holders.is_empty() {
            let commands =
                crate::util::join_list(info.holders.iter().map(|holder| holder.command.as_str()));
            let text = t!("repair.lock_busy", path = path, commands = commands).into_owned();
            self.open_choice(title, text, ok, Vec::new(), window, cx);
            return;
        }
        let lock = info.path.clone();
        let remove: ChoiceAction = Rc::new(move |this, window, cx| {
            this.remove_lock(lock.clone(), git_dirs.clone(), window, cx)
        });
        let text = t!("repair.lock_stale", path = path, age = age_text(info.age)).into_owned();
        self.open_choice(
            title,
            text,
            t!("toolbar.cancel").into_owned(),
            vec![(t!("repair.lock_remove").into_owned(), remove)],
            window,
            cx,
        );
    }

    fn remove_lock(
        &mut self,
        path: PathBuf,
        git_dirs: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let job = self.jobs.spawn(Lane::Write, move |_| {
            repair::remove_lock(&path, &git_dirs).map_err(|err| err.to_string())
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = job.join().await;
            this.update_in(cx, |this, _, cx| {
                match result {
                    Some(Ok(())) => {
                        note(Toast::success(t!("repair.lock_removed").into_owned()), cx)
                    }
                    Some(Err(err)) => note(
                        Toast::error(err).title(t!("repair.lock_remove_failed").into_owned()),
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

    /// A bare repository has no status read to fail: asks git about it instead, so a
    /// folder git refuses gets the banner and its Trust button too. (Gix opens it, and the
    /// graph's git walk sets `GIT_DIR`, which git doesn't check.)
    pub(super) fn check_bare_trust(&mut self, cx: &mut Context<Self>) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let git_dir = repo.repo.git_dir().to_owned();
        let job = self
            .jobs
            .spawn(Lane::Coalesce("bare-trust"), move |cancel| {
                git.command(&git_dir)
                    .args(["rev-parse", "--git-dir"])
                    .read_only()
                    .run(cancel)
                    .err()
            });
        cx.spawn(async move |this, cx| {
            let Some(Some(err)) = job.join().await else {
                return;
            };
            if err.kind != GitErrorKind::DubiousOwnership {
                return;
            }
            this.update(cx, |this, cx| {
                let opened = this.current_path.clone();
                this.untrusted =
                    repair::untrusted_folder(&err.stderr, opened.as_deref()).or(opened);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Asks before trusting `path`, a folder git refuses because another user owns it.
    pub(super) fn offer_trust(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let shown = path.display().to_string();
        let trust: ChoiceAction =
            Rc::new(move |this, window, cx| this.trust_folder(path.clone(), window, cx));
        self.open_choice(
            t!("repair.trust_title").into_owned(),
            t!("repair.trust_text", path = shown).into_owned(),
            t!("toolbar.cancel").into_owned(),
            vec![(t!("repair.trust").into_owned(), trust)],
            window,
            cx,
        );
    }

    /// Adds `path` to git's `safe.directory` list, then opens the repository again.
    fn trust_folder(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        // No repository may have loaded: use a git of its own.
        let job = self.jobs.spawn(Lane::Write, move |cancel| {
            let git = GitCli::discover()?;
            repair::trust_directory(&git, &path, cancel)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = job.join().await;
            this.update_in(cx, |this, _, cx| match result {
                Some(Ok(())) => {
                    this.untrusted = None;
                    if let Some(path) = this.current_path.clone() {
                        this.open(path, cx);
                    }
                }
                Some(Err(err)) => note(
                    Toast::error(err.summary()).title(t!("repair.trust_failed").into_owned()),
                    cx,
                ),
                None => {}
            })
            .ok();
        })
        .detach();
    }

    /// While git refuses the open repository's folder: why, and the way to trust it.
    pub(super) fn render_untrusted_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let path = self.untrusted.clone()?;
        let look = cx.look().clone();
        let trust = Button::new("trust-folder")
            .small()
            .label(t!("repair.trust_button").into_owned())
            .on_click(
                cx.listener(move |this, _, window, cx| this.offer_trust(path.clone(), window, cx)),
            )
            .into_any_element();
        Some(
            widgets::banner(
                AppIcon::Info,
                look.palette.modified,
                t!("repair.untrusted_banner").into_owned(),
                vec![trust],
                &look,
            )
            .into_any_element(),
        )
    }

    /// Name and email for commits, for this repository or all of them. With `then`, the
    /// commit that needed them runs once they're saved.
    pub(super) fn open_identity_dialog(
        &mut self,
        then: Option<(String, CommitOptions)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let identity = self.staging_view.read(cx).identity().clone();
        let global = Rc::new(Cell::new(true));
        let ok_label = if then.is_some() {
            t!("identity.save_commit")
        } else {
            t!("identity.save")
        };
        let spec = TextDialog {
            title: t!("identity.title").into_owned(),
            subtitle: Some(t!("identity.subtitle").into_owned()),
            placeholder: t!("identity.name_placeholder").into_owned(),
            initial: identity.name.unwrap_or_default(),
            detail: Some((
                t!("identity.email_placeholder").into_owned(),
                identity.email.unwrap_or_default(),
            )),
            checkbox: Some((t!("identity.global").into_owned(), global.clone())),
            ok_label: ok_label.into_owned(),
            allow_empty: false,
            multiline: false,
        };
        let submit: SubmitText = Rc::new(move |this, name, email, window, cx| {
            let (name, email) = (name.trim().to_owned(), email.trim().to_owned());
            if email.is_empty() {
                let message = t!("identity.email_needed").into_owned();
                note(Toast::error(message), cx);
                return;
            }
            this.save_identity(name, email, global.get(), then.clone(), window, cx);
        });
        self.open_text_dialog(spec, submit, window, cx);
    }

    /// Sets the profile's name, email and signing key in this repository's config (#45).
    pub(super) fn apply_identity_profile(
        &mut self,
        profile: gitweft_core::settings::IdentityProfile,
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
        let label = profile.label.clone();
        let job = self.jobs.spawn(Lane::Write, move |cancel| {
            repair::apply_identity(
                &git,
                &workdir,
                &profile.name,
                &profile.email,
                profile.signing_key.as_deref(),
                cancel,
            )
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = job.join().await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Some(Ok(())) => {
                        let text = t!("identity.profile_applied", profile = label).into_owned();
                        note(Toast::success(text), cx);
                    }
                    Some(Err(err)) => {
                        this.show_git_error(t!("identity.failed").into_owned(), &err, window, cx)
                    }
                    None => {}
                }
                this.refresh_status(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Asks for a label, then keeps this repository's name, email and signing key as an
    /// identity profile (#45); a profile with that label is replaced.
    pub(super) fn save_identity_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let identity = self.staging_view.read(cx).identity().clone();
        let (Some(name), Some(email)) = (identity.name, identity.email) else {
            note(Toast::error(t!("identity.email_needed").into_owned()), cx);
            return;
        };
        let spec = TextDialog {
            title: t!("identity.save_profile_title").into_owned(),
            subtitle: Some(
                t!(
                    "identity.profile_detail",
                    name = name.clone(),
                    email = email.clone()
                )
                .into_owned(),
            ),
            placeholder: t!("identity.profile_label").into_owned(),
            initial: String::new(),
            detail: None,
            checkbox: None,
            ok_label: t!("identity.save").into_owned(),
            allow_empty: false,
            multiline: false,
        };
        let submit: SubmitText = Rc::new(move |this, label, _, _, cx| {
            let label = label.trim().to_owned();
            let RepoState::Loaded(repo) = &this.state else {
                return;
            };
            let (git, cwd) = (repo.git.clone(), repo.cwd());
            let (name, email) = (name.clone(), email.clone());
            let job = this.jobs.spawn(Lane::Latest("signing-key"), move |cancel| {
                repair::signing_key(&git, &cwd, cancel)
            });
            cx.spawn(async move |_, cx| {
                let Some(signing_key) = job.join().await else {
                    return;
                };
                cx.update(|cx| {
                    SettingsStore::update(cx, |settings| {
                        let profiles = &mut settings.identity_profiles;
                        profiles.retain(|profile| profile.label != label);
                        profiles.push(gitweft_core::settings::IdentityProfile {
                            label: label.clone(),
                            name,
                            email,
                            signing_key,
                        });
                    });
                    let text = t!("identity.profile_saved", profile = label).into_owned();
                    crate::toast::push(Toast::success(text), cx);
                });
            })
            .detach();
        });
        self.open_text_dialog(spec, submit, window, cx);
    }

    pub(super) fn delete_identity_profile(&mut self, label: &str, cx: &mut Context<Self>) {
        SettingsStore::update(cx, |settings| {
            settings
                .identity_profiles
                .retain(|profile| profile.label != label);
        });
        let text = t!("identity.profile_deleted", profile = label).into_owned();
        note(Toast::info(text), cx);
    }

    fn save_identity(
        &mut self,
        name: String,
        email: String,
        global: bool,
        then: Option<(String, CommitOptions)>,
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
        let job = self.jobs.spawn(Lane::Write, move |cancel| {
            repair::set_identity(&git, &workdir, &name, &email, global, cancel)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = job.join().await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Some(Ok(())) => match then {
                        Some((message, options)) => this.run_commit(message, options, cx),
                        None => note(Toast::success(t!("identity.saved").into_owned()), cx),
                    },
                    Some(Err(err)) => {
                        this.show_git_error(t!("identity.failed").into_owned(), &err, window, cx)
                    }
                    None => {}
                }
                this.refresh_status(cx);
            })
            .ok();
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::age_text;
    use std::time::Duration;

    #[test]
    fn lock_ages() {
        let age = |secs| age_text(Some(Duration::from_secs(secs)));
        assert_eq!(age(20), "just now");
        assert_eq!(age(5 * 60), "5 min ago");
        assert_eq!(age(3 * 3600 + 5), "3 h ago");
        assert_eq!(age(2 * 86400), "2 d ago");
        assert_eq!(age_text(None), "at an unknown time");
    }
}
