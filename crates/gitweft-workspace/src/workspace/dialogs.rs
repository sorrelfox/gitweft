//! Dialogs the operations share: choices, text, and confirmations.

use super::*;

impl Workspace {
    /// A dialog explaining something, with buttons for the ways out (the last one is the
    /// suggested one) and one that just closes it.
    pub(super) fn open_choice(
        &mut self,
        title: String,
        text: String,
        dismiss: String,
        choices: Vec<(String, ChoiceAction)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A tab that isn't shown asks when it comes forward, not over another one.
        if !crate::tab_notes::is_active(cx.entity_id(), cx) {
            crate::tab_notes::hold_question(cx);
            self.with_window(
                move |this, window, cx| this.open_choice(title, text, dismiss, choices, window, cx),
                cx,
            );
            return;
        }
        let this = cx.entity().downgrade();
        let dialog_bg = cx.palette().panel;
        let labels = choices.iter().map(|(label, _)| label.as_str());
        let width = choice_width(
            std::iter::once(dismiss.as_str()).chain(labels),
            cx.metrics().zoom,
        );
        window.open_dialog(cx, move |dialog, window, _| {
            // On the panel color, like the prompts, menus and toasts; wide enough for the
            // buttons in one row.
            let dialog = dialog
                .bg(dialog_bg)
                .width(width.min(window.viewport_size().width * 0.9));
            let dismiss = dismiss.clone();
            let mut footer = DialogFooter::new()
                .child(DialogClose::new().trigger(move |button| button.label(dismiss)));
            // Each choice in a cell of its own, as the kit's DialogClose is: the buttons share
            // the row evenly, rather than Cancel taking what the others leave.
            for (ix, (label, action)) in choices.iter().enumerate() {
                let (this, action) = (this.clone(), action.clone());
                footer = footer.child(
                    div().size_full().child(
                        Button::new(("choice", ix))
                            .w_full()
                            .label(label.clone())
                            .when(ix + 1 == choices.len(), |button| button.primary())
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                this.update(cx, |this, cx| action(this, window, cx)).ok();
                            }),
                    ),
                );
            }
            let text = text.clone();
            dialog
                .title(title.clone())
                .content(move |content, _, cx| {
                    content.child(
                        gitweft_ui::flow::flow("choice-text", text.clone())
                            .text_size(cx.metrics().text.small),
                    )
                })
                .footer(footer)
        });
    }

    /// A dialog asking for text (a name, a message); OK or Enter hands the inputs to
    /// `submit`.
    pub(super) fn open_text_dialog(
        &mut self,
        spec: TextDialog,
        submit: SubmitText,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let TextDialog {
            title,
            subtitle,
            placeholder,
            initial,
            detail,
            checkbox,
            ok_label,
            allow_empty,
            multiline,
        } = spec;
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder.clone()));
        let area = multiline.then(|| {
            cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(3, 12)
                    .submit_on_enter(true)
                    .placeholder(placeholder)
            })
        });
        if !initial.is_empty() {
            match &area {
                Some(area) => area.update(cx, |area, cx| area.set_value(initial, window, cx)),
                None => input.update(cx, |input, cx| input.set_value(initial, window, cx)),
            }
        }
        let detail_input = detail.map(|(placeholder, initial)| {
            let state = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
            if !initial.is_empty() {
                state.update(cx, |state, cx| state.set_value(initial, window, cx));
            }
            state
        });
        let this = cx.entity().downgrade();
        let (dialog_input, dialog_detail, dialog_submit) =
            (input.clone(), detail_input.clone(), submit.clone());
        let dialog_area = area.clone();
        let dialog_bg = cx.palette().panel;
        window.open_dialog(cx, move |dialog, _, _| {
            // On the panel color, like the prompts, menus and toasts.
            let dialog = dialog.bg(dialog_bg);
            let (content_input, ok_input) = (dialog_input.clone(), dialog_input.clone());
            let (content_area, ok_area) = (dialog_area.clone(), dialog_area.clone());
            let (content_detail, ok_detail) = (dialog_detail.clone(), dialog_detail.clone());
            let (this, submit) = (this.clone(), dialog_submit.clone());
            let closing = this.clone();
            let (subtitle, checkbox) = (subtitle.clone(), checkbox.clone());
            dialog
                .title(title.clone())
                .content(move |content, _, cx| {
                    let small = cx.metrics().text.small;
                    content
                        .gap_3()
                        .when_some(subtitle.clone(), |content, subtitle| {
                            content.child(div().text_size(small).child(subtitle))
                        })
                        .map(|content| match &content_area {
                            Some(area) => content.child(Textarea::new(area)),
                            None => content.child(Input::new(&content_input)),
                        })
                        .when_some(content_detail.clone(), |content, detail| {
                            content.child(Input::new(&detail))
                        })
                        .when_some(checkbox.clone(), |content, (label, state)| {
                            content.child(
                                Checkbox::new("text-dialog-option")
                                    .label(label)
                                    .checked(state.get())
                                    .on_click(move |checked: &bool, window, _| {
                                        state.set(*checked);
                                        window.refresh();
                                    }),
                            )
                        })
                })
                .footer(
                    DialogFooter::new()
                        .child(
                            DialogClose::new()
                                .trigger(|button| button.label(t!("toolbar.cancel").into_owned())),
                        )
                        .child(
                            DialogAction::new().child(
                                Button::new("text-dialog-ok")
                                    .primary()
                                    .label(ok_label.clone()),
                            ),
                        ),
                )
                .on_ok(move |_, window, cx| {
                    let text = match &ok_area {
                        Some(area) => area.read(cx).value().to_string(),
                        None => ok_input.read(cx).value().to_string(),
                    };
                    if text.trim().is_empty() && !allow_empty {
                        return false;
                    }
                    let detail = ok_detail
                        .as_ref()
                        .map(|detail| detail.read(cx).value().to_string())
                        .unwrap_or_default();
                    let submit = submit.clone();
                    this.update(cx, |this, cx| submit(this, text, detail, window, cx))
                        .ok();
                    true
                })
                // The keys go back to the view in the centre. GPUI gives them back to what had
                // the focus before the dialog, which is gone when that was a button the dialog
                // replaced (the diff's "Comment…"): no key reached anything until a click.
                .on_close(move |_, _, cx| {
                    closing.update(cx, |this, cx| this.refocus_centre(cx)).ok();
                })
        });
        // Enter submits through the dialog's Enter → Confirm → `on_ok` (the input lets Enter
        // propagate). A PressEnter handler of ours ran the operation a second time.
        match &area {
            Some(area) => area.update(cx, |area, cx| area.focus(window, cx)),
            None => input.update(cx, |input, cx| input.focus(window, cx)),
        }
    }

    /// Finds out, in the write lane (so after any write queued before), which commits
    /// moving the checked-out branch from `tip` to `keep` would leave behind although a
    /// remote has them, then calls `then`. A failed check counts as nothing found: the
    /// warning is a courtesy, never a guard.
    pub(super) fn check_published(
        &mut self,
        tip: String,
        keep: Option<String>,
        then: impl FnOnce(&mut Self, Published, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let cwd = repo.cwd();
        let job = self.jobs.spawn(Lane::Write, move |cancel| {
            // The names may be encoded (`undo::ref_name_text`): git gets their bytes.
            let os = |text: &str| -> std::ffi::OsString {
                gitweft_platform::bytes::os_string_from_vec(gitweft_git::undo::ref_name_bytes(text))
            };
            let keep = keep.as_deref().map(os);
            published_commits(&git, &cwd, os(&tip), keep.as_deref(), cancel)
        });
        cx.spawn_in(window, async move |this, cx| {
            let published = job.join().await.and_then(Result::ok).unwrap_or_default();
            this.update_in(cx, |this, window, cx| then(this, published, window, cx))
                .ok();
        })
        .detach();
    }

    /// A warning prompt with `button` (danger-styled) and Cancel; `then` learns which
    /// was chosen.
    pub(super) fn confirm(
        &mut self,
        message: String,
        detail: String,
        button: String,
        then: impl FnOnce(&mut Self, bool, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ask(
            PromptLevel::Warning,
            message,
            detail,
            button,
            then,
            window,
            cx,
        )
    }

    /// [`Self::confirm`] at any level; only Warning styles `button` as dangerous.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn ask(
        &mut self,
        level: PromptLevel,
        message: String,
        detail: String,
        button: String,
        then: impl FnOnce(&mut Self, bool, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let answer = window.prompt(
            level,
            &message,
            Some(&detail),
            &[
                PromptButton::ok(button),
                PromptButton::cancel(t!("toolbar.cancel").into_owned()),
            ],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let confirmed = answer.await == Ok(0);
            this.update_in(cx, |this, window, cx| then(this, confirmed, window, cx))
                .ok();
        })
        .detach();
    }
}
