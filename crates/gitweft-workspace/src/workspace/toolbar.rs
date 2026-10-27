//! The toolbar.

use super::*;

impl Workspace {
    /// Undo and Redo; Fetch, and Pull and Push as split buttons (the action with the number
    /// of commits it would bring or send, and a menu of the other modes); Stash, Pop and
    /// More; Reflog and Activity on the right. A running operation shows in the status bar.
    pub(super) fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);
        let busy = self.remote_op.is_some();
        // What Pull would bring and Push would send, as chips on the buttons.
        let (behind, ahead) = self
            .status
            .as_ref()
            .map_or((0, 0), |status| (status.branch.behind, status.branch.ahead));

        let tool = |id: &'static str,
                    icon: AppIcon,
                    label: String,
                    count: u32,
                    kind: RemoteOpKind,
                    cx: &mut Context<Self>| {
            Button::new(id)
                .ghost()
                .small()
                .icon(Icon::from(icon))
                .label(label)
                .when(count > 0, |button| {
                    button.child(widgets::count_chip(count, &p, &m))
                })
                .disabled(busy)
                .on_click(cx.listener(move |this, _, window, cx| this.run_remote(kind, window, cx)))
        };
        // Fetch's menu: every remote, or one of them.
        let fetch_menu = {
            let this = cx.entity().downgrade();
            let remotes: Vec<String> = self.remotes.iter().map(|r| r.name.clone()).collect();
            Button::new("fetch-remotes")
                .ghost()
                .small()
                .icon(Icon::from(AppIcon::ChevronDown))
                .disabled(busy || remotes.is_empty())
                .dropdown_menu(move |menu, _, _| {
                    let item = |label: String, remote: Option<String>| {
                        let this = this.clone();
                        PopupMenuItem::new(label).on_click(move |_, window, cx| {
                            let remote = remote.clone();
                            this.update(cx, |this, cx| this.fetch_from(remote, window, cx))
                                .ok();
                        })
                    };
                    let all = item(t!("remote.fetch_all").into_owned(), None);
                    remotes
                        .iter()
                        .fold(menu.item(all).separator(), |menu, name| {
                            let label = t!("remote.fetch_named", remote = name).into_owned();
                            menu.item(item(label, Some(name.clone())))
                        })
                })
        };
        let pull_focus = self.focus.clone();
        let pull_menu = Button::new("pull-modes")
            .ghost()
            .small()
            .icon(Icon::from(AppIcon::ChevronDown))
            .disabled(busy)
            .dropdown_menu(move |menu, _, _| {
                menu.action_context(pull_focus.clone())
                    .menu(t!("toolbar.pull_merge").into_owned(), Box::new(PullMerge))
                    .menu(t!("toolbar.pull_rebase").into_owned(), Box::new(PullRebase))
                    .menu(
                        t!("toolbar.pull_ff_only").into_owned(),
                        Box::new(PullFastForward),
                    )
            });
        let push_focus = self.focus.clone();
        let push_menu = Button::new("push-modes")
            .ghost()
            .small()
            .icon(Icon::from(AppIcon::ChevronDown))
            .disabled(busy)
            .dropdown_menu(move |menu, _, _| {
                menu.action_context(push_focus.clone())
                    .menu(t!("toolbar.force_push").into_owned(), Box::new(ForcePush))
            });

        // A split button: the action, a hairline, and the menu of its other modes.
        let split = |action: Button, menu: AnyElement| {
            h_flex()
                .items_center()
                .child(action)
                .child(div().w(m.dp(1.)).h(m.dp(16.)).bg(p.line))
                .child(menu)
        };

        h_flex()
            .h(m.toolbar)
            .flex_shrink_0()
            .px(m.dp(10.))
            .gap(m.dp(4.))
            .bg(p.panel)
            .border_b_1()
            .border_color(p.line)
            .child(
                h_flex()
                    .gap(m.dp(2.))
                    .pr(m.dp(8.))
                    .mr(m.dp(6.))
                    .border_r_1()
                    .border_color(p.line)
                    .child(self.render_undo_button(false, cx))
                    .child(self.render_undo_button(true, cx)),
            )
            .child(split(
                tool(
                    "fetch",
                    AppIcon::Fetch,
                    t!("toolbar.fetch").into_owned(),
                    0,
                    RemoteOpKind::Fetch,
                    cx,
                )
                .when_some(self.fetch_tooltip(), |button, tip| button.tooltip(tip)),
                fetch_menu.into_any_element(),
            ))
            .child(split(
                tool(
                    "pull",
                    AppIcon::Pull,
                    t!("toolbar.pull").into_owned(),
                    behind,
                    RemoteOpKind::Pull(PullMode::Configured),
                    cx,
                ),
                pull_menu.into_any_element(),
            ))
            .child(split(
                tool(
                    "push",
                    AppIcon::Push,
                    t!("toolbar.push").into_owned(),
                    ahead,
                    RemoteOpKind::Push(ForceMode::None),
                    cx,
                ),
                push_menu.into_any_element(),
            ))
            .child(widgets::divider(&p, &m))
            .child(
                Button::new("stash")
                    .ghost()
                    .small()
                    .icon(Icon::from(AppIcon::Stash))
                    .label(t!("toolbar.stash").into_owned())
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_stash_dialog(Vec::new(), window, cx)
                    })),
            )
            .child(
                Button::new("pop")
                    .ghost()
                    .small()
                    .label(t!("toolbar.pop").into_owned())
                    .disabled(busy || self.stashes.is_empty())
                    .on_click(cx.listener(|this, _, window, cx| this.pop_latest_stash(window, cx))),
            )
            .child({
                let focus = self.focus.clone();
                let lfs = self.sync.lfs_ready();
                let shallow = !self.sync.extras.shallow.is_empty();
                let background = self.sync.settings.background_fetch;
                let minutes = self.sync.settings.fetch_interval_minutes;
                Button::new("more-tools")
                    .ghost()
                    .small()
                    .icon(Icon::from(AppIcon::ChevronDown))
                    .label(t!("toolbar.more").into_owned())
                    .disabled(busy)
                    .dropdown_menu(move |menu, window, cx| {
                        let interval_focus = focus.clone();
                        menu.action_context(focus.clone())
                            .menu(
                                t!("external.open_terminal").into_owned(),
                                Box::new(OpenInTerminal),
                            )
                            .menu(
                                t!("external.open_file_manager").into_owned(),
                                Box::new(OpenInFileManager),
                            )
                            .separator()
                            .menu(
                                t!("files.apply_patches").into_owned(),
                                Box::new(ApplyPatches),
                            )
                            .separator()
                            .menu(
                                t!("files.clean_untracked").into_owned(),
                                Box::new(CleanUntracked),
                            )
                            .menu(
                                t!("files.clean_ignored").into_owned(),
                                Box::new(CleanIgnored),
                            )
                            .separator()
                            .menu(
                                t!("submodule.add_menu").into_owned(),
                                Box::new(AddSubmodule),
                            )
                            .when(lfs, |menu| {
                                menu.menu(t!("lfs.locks_menu").into_owned(), Box::new(ShowLfsLocks))
                            })
                            .separator()
                            .menu(t!("trash.show").into_owned(), Box::new(ShowTrash))
                            .menu(
                                t!("dangling.show").into_owned(),
                                Box::new(ShowDanglingCommits),
                            )
                            .when(lfs, |menu| {
                                menu.separator()
                                    .menu(t!("lfs.pull").into_owned(), Box::new(DownloadLfsFiles))
                                    .menu(t!("lfs.fetch").into_owned(), Box::new(FetchLfsObjects))
                                    .menu(
                                        t!("lfs.fetch_all").into_owned(),
                                        Box::new(FetchAllLfsObjects),
                                    )
                                    .menu(t!("lfs.prune").into_owned(), Box::new(PruneLfsObjects))
                            })
                            .when(shallow, |menu| {
                                menu.separator()
                                    .menu(
                                        tn!("shallow.deepen", count = sync::DEEPEN_BY).into_owned(),
                                        Box::new(DeepenHistory),
                                    )
                                    .menu(
                                        t!("shallow.unshallow").into_owned(),
                                        Box::new(FetchAllHistory),
                                    )
                            })
                            .separator()
                            .menu_with_check(
                                tn!("background_fetch.menu", count = minutes).into_owned(),
                                background,
                                Box::new(ToggleBackgroundFetch),
                            )
                            // How often, per repository (#36): until now only in its file.
                            .submenu(
                                t!("background_fetch.interval_menu").into_owned(),
                                window,
                                cx,
                                move |menu, _, _| {
                                    let mut menu = menu.action_context(interval_focus.clone());
                                    for every in sync::FETCH_INTERVALS {
                                        menu = menu.menu_with_check(
                                            tn!("background_fetch.every", count = every)
                                                .into_owned(),
                                            minutes == every,
                                            sync::fetch_every_action(every),
                                        );
                                    }
                                    menu
                                },
                            )
                    })
            })
            .child(div().flex_1())
            // On the right, like progress: appearing here moves no button under the pointer.
            .children(self.render_fetch_badge(cx))
            .child(
                Button::new("reflog")
                    .ghost()
                    .small()
                    .icon(Icon::from(AppIcon::History))
                    .label(t!("toolbar.reflog").into_owned())
                    .selected(self.reflog_view.is_some())
                    .on_click(cx.listener(|this, _, _, cx| {
                        if this.reflog_view.is_some() {
                            this.close_reflog();
                            cx.notify();
                        } else {
                            this.open_reflog("HEAD".into(), "HEAD".into(), cx);
                        }
                    })),
            )
            .child(
                Button::new("activity")
                    .ghost()
                    .small()
                    .icon(Icon::from(AppIcon::Activity))
                    .label(t!("toolbar.activity").into_owned())
                    .selected(self.show_activity)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_activity = !this.show_activity;
                        cx.notify();
                    })),
            )
    }
}
