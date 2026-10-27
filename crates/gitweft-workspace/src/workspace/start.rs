//! The start screen: opening, creating and cloning repositories, and the recent list.

use super::*;

impl Workspace {
    pub(super) fn prompt_open(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t!("start.open_repository_prompt").into_owned().into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await
                && let Some(path) = paths.into_iter().next()
            {
                this.update(cx, |this, cx| this.open(path, cx)).ok();
            }
        })
        .detach();
    }

    /// Runs `work` on the recent-repositories file in a job, then shows the list it
    /// returns. Changes go through the write lane, so none is dropped or reordered.
    pub(super) fn update_recent(
        &mut self,
        work: impl FnOnce(&Path) -> Vec<PathBuf> + Send + 'static,
        lane: Lane,
        cx: &mut Context<Self>,
    ) {
        let file = self.env.recent_file.clone();
        let job = self.jobs.spawn(lane, move |_| work(&file));
        cx.spawn(async move |this, cx| {
            if let Some(list) = job.join().await {
                this.update(cx, |this, cx| {
                    this.recent = list;
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// Puts `root` at the top of the recent list, once per repository opened.
    pub(super) fn remember_recent(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        if self.remembered_root.as_ref() == Some(&root) {
            return;
        }
        self.remembered_root = Some(root.clone());
        let work =
            move |file: &Path| recent::remember(file, &root).unwrap_or_else(|_| recent::load(file));
        self.update_recent(work, Lane::Write, cx);
    }

    pub(super) fn forget_recent(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let work =
            move |file: &Path| recent::forget(file, &path).unwrap_or_else(|_| recent::load(file));
        self.update_recent(work, Lane::Write, cx);
    }

    /// "Create Repository…": `git init` in a chosen (possibly new) folder, then open it.
    pub(super) fn prompt_init(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t!("start.create_repository_prompt").into_owned().into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await
                && let Some(dir) = paths.into_iter().next()
            {
                this.update(cx, |this, cx| this.init_and_open(dir, cx)).ok();
            }
        })
        .detach();
    }

    pub(super) fn init_and_open(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        let (env, activity) = (self.askpass_env.clone(), self.activity.clone());
        let job_dir = dir.clone();
        let job = self.jobs.spawn(Lane::Write, move |cancel| {
            let git = standalone_git(env, activity)?;
            init_repository(&git, &job_dir, cancel)
        });
        cx.spawn(async move |this, cx| {
            let Some(result) = job.join().await else {
                return;
            };
            this.update(cx, |this, cx| match result {
                Ok(()) => this.open(dir, cx),
                Err(err) => {
                    let message = t!("init.failed", error = err.summary()).into_owned();
                    this.last_path = Some(dir.clone());
                    this.state = RepoState::Failed(message);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// "Clone Repository…": the address (and whether to include submodules), then the
    /// folder to clone into. The clone gets its own folder there, named like git would.
    pub(super) fn prompt_clone(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let submodules = Rc::new(Cell::new(false));
        let flag = submodules.clone();
        let submit: SubmitText = Rc::new(move |this, url, _, _, cx| {
            this.prompt_clone_folder(url.trim().to_owned(), flag.get(), cx)
        });
        let spec = TextDialog {
            title: t!("clone.title").into_owned(),
            subtitle: Some(t!("clone.subtitle").into_owned()),
            placeholder: t!("clone.placeholder").into_owned(),
            initial: String::new(),
            detail: None,
            checkbox: Some((t!("clone.submodules").into_owned(), submodules)),
            ok_label: t!("clone.choose_folder").into_owned(),
            allow_empty: false,
            multiline: false,
        };
        self.open_text_dialog(spec, submit, window, cx);
    }

    pub(super) fn prompt_clone_folder(
        &mut self,
        url: String,
        recurse: bool,
        cx: &mut Context<Self>,
    ) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t!("start.clone_into_prompt").into_owned().into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await
                && let Some(parent) = paths.into_iter().next()
            {
                let name = clone_dir_name(&url)
                    .unwrap_or_else(|| t!("start.clone_default_folder").into_owned());
                this.update(cx, |this, cx| {
                    this.start_clone(url, parent.join(name), recurse, cx)
                })
                .ok();
            }
        })
        .detach();
    }

    /// Clones in the write lane with progress on the start screen; the clone opens when
    /// done. Credential prompts work as for fetch.
    pub(super) fn start_clone(
        &mut self,
        url: String,
        dir: PathBuf,
        recurse: bool,
        cx: &mut Context<Self>,
    ) {
        if self.clone_op.is_some() {
            return;
        }
        let (env, activity) = (self.askpass_env.clone(), self.activity.clone());
        let (progress_tx, progress_rx) = async_channel::unbounded::<Progress>();
        let (job_url, job_dir) = (url.clone(), dir.clone());
        let job = self.jobs.spawn(Lane::Write, move |cancel| {
            let git = standalone_git(env, activity)?;
            let forward = move |event: StderrEvent| {
                if let StderrEvent::Progress(progress) = event {
                    let _ = progress_tx.try_send(progress);
                }
            };
            clone_repository(&git, &job_url, &job_dir, recurse, cancel, forward)
        });
        self.clone_op = Some(CloneOp {
            url,
            progress: None,
            cancel: job.cancel_token().clone(),
        });
        cx.notify();
        cx.spawn(async move |this, cx| {
            while let Ok(progress) = progress_rx.recv().await {
                let updated = this.update(cx, |this, cx| {
                    if let Some(op) = &mut this.clone_op {
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
        cx.spawn(async move |this, cx| {
            let result = job.join().await;
            this.update(cx, |this, cx| {
                this.clone_op = None;
                match result {
                    Some(Ok(_)) => this.open(dir, cx),
                    Some(Err(err)) if err.kind != GitErrorKind::Cancelled => {
                        let message = t!("clone.failed", error = err.summary()).into_owned();
                        this.last_path = Some(dir.clone());
                        this.state = RepoState::Failed(message);
                    }
                    Some(Err(_)) | None => {}
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Open, create or clone a repository, and the recently opened ones.
    /// Scrolls the recent list to the row that holds the keyboard focus (its Forget button),
    /// once each time the focus moves to another row: Tab can reach a row below the list's
    /// fold, and GPUI doesn't scroll to it. The mouse wheel is left alone while the
    /// focus stays put.
    pub(super) fn reveal_recent_focus(&mut self, window: &Window, cx: &mut Context<Self>) {
        while self.recent_focus.len() < self.recent.len() {
            self.recent_focus.push(cx.focus_handle());
        }
        let focused = self.recent_focus[..self.recent.len()]
            .iter()
            .position(|handle| handle.contains_focused(window, cx));
        if focused != self.recent_revealed {
            if let Some(ix) = focused {
                self.recent_scroll.scroll_to_item(ix);
            }
            self.recent_revealed = focused;
        }
    }

    pub(super) fn render_start(
        &self,
        error: Option<&str>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);
        let z = |value: f32| px(value * m.zoom);
        let busy = self.clone_op.is_some();

        let cloning = self.clone_op.as_ref().map(|op| {
            let detail = op.progress.as_ref().map(|progress| match progress.percent {
                Some(percent) => {
                    t!("remote.progress", phase = progress.phase, percent = percent).into_owned()
                }
                None => progress.phase.clone(),
            });
            h_flex()
                .gap(z(8.))
                .text_size(m.text.small)
                .text_color(p.fg2)
                .child(t!("clone.running", url = op.url).into_owned())
                .children(detail)
                .child(
                    Button::new("cancel-clone")
                        .ghost()
                        .xsmall()
                        .icon(AppIcon::Close)
                        .label(t!("toolbar.cancel").into_owned())
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(op) = &this.clone_op {
                                op.cancel.cancel();
                            }
                            cx.notify();
                        })),
                )
        });

        // Two-line rows in a card: the folder's name, and where it is in mono.
        let recent_rows: Vec<_> = (0u32..)
            .zip(&self.recent)
            .map(|(ix, path)| {
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                let location = path
                    .parent()
                    .map(|parent| parent.display().to_string())
                    .unwrap_or_default();
                let (open_path, forget_path) = (path.clone(), path.clone());
                let hover = p.hover;
                let focus = self.recent_focus.get(ix as usize).cloned();
                h_flex()
                    .id(("recent", ix))
                    .when_some(focus, |row, focus| row.track_focus(&focus))
                    .w_full()
                    .h(z(48.))
                    .flex_shrink_0()
                    .px(z(12.))
                    .gap(z(12.))
                    .cursor_pointer()
                    .when(ix > 0, |row| row.border_t_1().border_color(p.line))
                    .hover(move |row| row.bg(hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.clone_op.is_none() {
                            this.open(open_path.clone(), cx)
                        }
                    }))
                    .child(Icon::new(AppIcon::Folder).size(z(16.)).text_color(p.fg3))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                truncated("name", name)
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(p.fg),
                            )
                            .child(
                                truncated("path", location)
                                    .font_family(gitweft_ui::current::mono_font())
                                    .text_size(m.text.mono_small)
                                    .text_color(p.fg3),
                            ),
                    )
                    .child(
                        Button::new(("forget-recent", ix))
                            .ghost()
                            .xsmall()
                            .icon(AppIcon::Close)
                            .tooltip(t!("start.forget").into_owned())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.forget_recent(forget_path.clone(), cx)
                            })),
                    )
            })
            .collect();

        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap(z(16.))
            .px(z(24.))
            .child(
                div()
                    .text_size(z(26.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.fg)
                    .child(APP_NAME),
            )
            .child(
                div()
                    .text_color(p.fg2)
                    .child(t!("app.tagline").into_owned()),
            )
            .when_some(error, |this, error| {
                this.child(
                    div()
                        .max_w(z(640.))
                        .text_center()
                        .text_size(m.text.small)
                        .text_color(p.del)
                        .child(error.to_owned()),
                )
            })
            .child(
                h_flex()
                    .gap(z(8.))
                    .child(
                        Button::new("open-repo")
                            .primary()
                            .label(t!("start.open_repository").into_owned())
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, _, cx| this.prompt_open(cx))),
                    )
                    .child(
                        Button::new("init-repo")
                            .label(t!("start.create_repository").into_owned())
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, _, cx| this.prompt_init(cx))),
                    )
                    .child(
                        Button::new("clone-repo")
                            .label(t!("start.clone_repository").into_owned())
                            .disabled(busy)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.prompt_clone(window, cx)),
                            ),
                    ),
            )
            .children(cloning)
            .when(!recent_rows.is_empty(), |this| {
                this.child(
                    v_flex()
                        .w(z(480.))
                        .max_w_full()
                        .mt(z(16.))
                        .gap(z(6.))
                        // As the sidebar's section labels ("LOCAL").
                        .child(
                            div()
                                .px(z(4.))
                                .text_size(m.text.label)
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(p.fg3)
                                .child(t!("start.recent").to_uppercase()),
                        )
                        .child(
                            v_flex()
                                .id("recent-list")
                                .max_h(z(360.))
                                .overflow_y_scroll()
                                .track_scroll(&self.recent_scroll)
                                .rounded(m.radius_lg)
                                .border_1()
                                .border_color(p.line)
                                .bg(p.panel)
                                .children(recent_rows),
                        ),
                )
            })
    }
}
