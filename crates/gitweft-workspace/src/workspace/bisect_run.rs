//! `git bisect run`: a command decides each step (exit 0 good, 1–127 bad, 125 skip). The
//! banner shows its last line of output while it runs, with Cancel; "Show output" shows
//! every line, during the run and after it.

use gitweft_git::bisect::bisect_run;

use super::*;

/// A `git bisect run` going on.
pub(super) struct BisectRunning {
    pub command: String,
    /// The command's (or git's) last line of output.
    pub last_line: String,
    /// Every line so far, shared with the output dialog while it's open.
    pub log: Entity<BisectLog>,
    pub cancel: CancelToken,
}

/// What the last `git bisect run` printed, kept after it ends.
pub(super) struct BisectOutput {
    pub command: String,
    pub log: Entity<BisectLog>,
}

/// The most lines a run keeps.
const OUTPUT_LINES: usize = 2000;

/// A run's lines (the first ones dropped past [`OUTPUT_LINES`]), drawn in the output
/// dialog. The run adds to it, so an open dialog follows the run: it stays at the bottom
/// while it was there, and keeps its place when scrolled up (#39).
pub(super) struct BisectLog {
    lines: Vec<String>,
    scroll: ScrollHandle,
}

impl BisectLog {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            scroll: ScrollHandle::new(),
        }
    }

    fn push(&mut self, line: String, cx: &mut Context<Self>) {
        // Offsets are negative going down; before the first layout both are zero.
        let at_bottom = self.scroll.offset().y <= -self.scroll.max_offset().y + px(1.);
        self.lines.push(line);
        let excess = self.lines.len().saturating_sub(OUTPUT_LINES);
        self.lines.drain(..excess);
        if at_bottom {
            self.scroll.scroll_to_bottom();
        }
        cx.notify();
    }

    #[cfg(test)]
    pub(super) fn lines(&self) -> &[String] {
        &self.lines
    }
}

impl Render for BisectLog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);
        div()
            .id("bisect-output")
            .max_h(px(420. * m.zoom))
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .font_family(gitweft_ui::current::mono_font())
            .text_size(m.text.mono_small)
            .text_color(p.fg2)
            .when(self.lines.is_empty(), |this| {
                this.child(t!("bisect.output_empty").into_owned())
            })
            .children(
                self.lines
                    .iter()
                    .map(|line| div().whitespace_normal().child(line.clone())),
            )
    }
}

impl Workspace {
    /// Asks for the command, then runs the bisect with it.
    pub(super) fn open_bisect_run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let spec = TextDialog {
            title: t!("bisect.run_title").into_owned(),
            subtitle: Some(t!("bisect.run_hint").into_owned()),
            placeholder: t!("bisect.run_placeholder").into_owned(),
            initial: self.last_bisect_command.clone(),
            detail: None,
            checkbox: None,
            ok_label: t!("bisect.run").into_owned(),
            allow_empty: false,
            multiline: false,
        };
        let submit: SubmitText = Rc::new(|this, command, _, window, cx| {
            this.run_bisect_command(command.trim().to_owned(), window, cx)
        });
        self.open_text_dialog(spec, submit, window, cx);
    }

    fn run_bisect_command(&mut self, command: String, window: &mut Window, cx: &mut Context<Self>) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let Some(cwd) = repo.workdir.clone() else {
            return;
        };
        self.last_bisect_command = command.clone();
        let (tx, rx) = async_channel::unbounded::<String>();
        let run = command.clone();
        let job = self.jobs.spawn(Lane::Write, move |cancel| {
            bisect_run(&git, &cwd, &run, cancel, |line| {
                let _ = tx.try_send(line.to_owned());
            })
        });
        self.bisect_running = Some(BisectRunning {
            command,
            last_line: String::new(),
            log: cx.new(|_| BisectLog::new()),
            cancel: job.cancel_token().clone(),
        });
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            // The channel closes when the job ends.
            while let Ok(line) = rx.recv().await {
                let line = line.trim().to_owned();
                if line.is_empty() {
                    continue;
                }
                this.update(cx, |this, cx| {
                    if let Some(running) = &mut this.bisect_running {
                        running.last_line = line.clone();
                        running.log.update(cx, |log, cx| log.push(line, cx));
                        cx.notify();
                    }
                })
                .ok();
            }
            let result = job.join().await;
            this.update_in(cx, |this, _, cx| {
                if let Some(running) = this.bisect_running.take() {
                    this.last_bisect_output = Some(BisectOutput {
                        command: running.command,
                        log: running.log,
                    });
                }
                match result {
                    Some(Ok(outcome)) => match outcome.first_bad {
                        Some(id) => {
                            let sha = id.to_hex_with_len(8).to_string();
                            let message = t!("bisect.run_found", sha = sha).into_owned();
                            note(Toast::success(message), cx);
                            this.select_quietly(id, cx);
                        }
                        None => note(Toast::warning(t!("bisect.run_no_result").into_owned()), cx),
                    },
                    Some(Err(err)) if err.kind == GitErrorKind::Cancelled => {
                        note(Toast::info(t!("bisect.run_stopped").into_owned()), cx)
                    }
                    Some(Err(err)) => note(
                        Toast::error(err.summary()).title(t!("bisect.run_failed").into_owned()),
                        cx,
                    ),
                    None => note(Toast::info(t!("bisect.run_stopped").into_owned()), cx),
                }
                this.refresh_refs(cx);
                this.refresh_status(cx);
                this.load_bisect(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Every line the running (or last) `git bisect run` printed, in a dialog that scrolls.
    /// While it runs, the lines so far, and those that come while it's open.
    pub(super) fn open_bisect_output(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (command, log) = match (&self.bisect_running, &self.last_bisect_output) {
            (Some(running), _) => (running.command.clone(), running.log.clone()),
            (None, Some(output)) => (output.command.clone(), output.log.clone()),
            (None, None) => return,
        };
        // Opened at the end, where the new lines come.
        log.read(cx).scroll.scroll_to_bottom();
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);
        let title = t!("bisect.output_title", command = command).into_owned();
        let dismiss = t!("bisect.output_close").into_owned();
        window.open_dialog(cx, move |dialog, window, _| {
            let (log, dismiss) = (log.clone(), dismiss.clone());
            dialog
                .bg(p.panel)
                .width((px(720.) * m.zoom).min(window.viewport_size().width * 0.9))
                .title(title.clone())
                .content(move |content, _, _| content.child(log.clone()))
                .footer(
                    DialogFooter::new()
                        .child(DialogClose::new().trigger(move |button| button.label(dismiss))),
                )
        });
    }

    /// Stops the running `git bisect run` (the bisect stays at the commit being tested).
    pub(super) fn cancel_bisect_run(&mut self) {
        if let Some(running) = &self.bisect_running {
            running.cancel.cancel();
        }
    }
}

impl Workspace {
    /// Reads where the bisect in progress stands, then shows its marks in the graph.
    pub(super) fn load_bisect(&mut self, cx: &mut Context<Self>) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let cwd = repo.cwd();
        let job = self.jobs.spawn(Lane::Coalesce("bisect"), move |cancel| {
            bisect_state(&git, &cwd, cancel).ok().flatten()
        });
        cx.spawn(async move |this, cx| {
            if let Some(state) = job.join().await {
                this.update(cx, |this, cx| {
                    if this.bisect != state {
                        this.bisect = state;
                        this.push_bisect_marks(cx);
                        cx.notify();
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    /// Hands the bisect's marks to the graph (none when no bisect runs).
    pub(super) fn push_bisect_marks(&mut self, cx: &mut Context<Self>) {
        let marks = self.bisect.as_ref().map(|state| {
            let mut marks = HashMap::new();
            for id in &state.good {
                marks.insert(*id, BisectMark::Good);
            }
            for id in &state.skipped {
                marks.insert(*id, BisectMark::Skip);
            }
            if let Some(bad) = state.bad {
                marks.insert(bad, BisectMark::Bad);
            }
            marks
        });
        if let Some(graph) = &self.graph_view {
            graph.update(cx, |graph, cx| graph.set_bisect(marks, cx));
        }
    }

    /// The graph's row menu: starts a bisect with this commit as bad or good, or marks
    /// it in the bisect that runs.
    pub(super) fn bisect_from_graph(
        &mut self,
        id: ObjectId,
        mark: BisectMark,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let running = matches!(self.operation, Some(Operation::Bisect));
        let rev = id.to_string();
        self.run_bisect(
            move |git, cwd, cancel| {
                if running {
                    bisect_mark(git, cwd, mark, Some(&rev), cancel)?;
                } else {
                    match mark {
                        BisectMark::Bad => bisect_start(git, cwd, Some(&rev), &[], &[], cancel)?,
                        BisectMark::Good | BisectMark::Skip => {
                            bisect_start(git, cwd, None, &[rev], &[], cancel)?
                        }
                    };
                }
                Ok(())
            },
            window,
            cx,
        );
    }

    /// The bisect banner's buttons: marks the checked-out commit.
    pub(super) fn bisect_mark_head(
        &mut self,
        mark: BisectMark,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_bisect(
            move |git, cwd, cancel| bisect_mark(git, cwd, mark, None, cancel).map(|_| ()),
            window,
            cx,
        );
    }

    /// Runs a bisect command in the write lane. Only failures get a toast: the banner
    /// shows where the bisect stands. It moves HEAD, so it isn't undoable (Reset goes back).
    pub(super) fn run_bisect(
        &mut self,
        job: impl FnOnce(&GitCli, &Path, &CancelToken) -> Result<(), GitError> + Send + 'static,
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
            .spawn(Lane::Write, move |cancel| job(&git, &cwd, cancel));
        cx.spawn_in(window, async move |this, cx| {
            let result = task.join().await;
            this.update_in(cx, |this, _, cx| {
                if let Some(Err(err)) = result {
                    note(
                        Toast::error(err.summary()).title(t!("bisect.failed").into_owned()),
                        cx,
                    );
                }
                this.refresh_refs(cx);
                this.refresh_status(cx);
                this.load_bisect(cx);
            })
            .ok();
        })
        .detach();
    }

    /// The banner while bisecting: what git needs next, the commit under test with Good /
    /// Bad / Skip, or the first bad commit; Reset always.
    pub(super) fn render_bisect_banner(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let look = cx.look().clone();
        let found = matches!(
            self.bisect.as_ref().map(|state| &state.progress),
            Some(BisectProgress::Found { .. })
        );
        let tint = if found {
            look.palette.add
        } else {
            look.palette.modified
        };
        let short = |id: &ObjectId| id.to_hex_with_len(8).to_string();
        let (text, testing, first_bad) = match self.bisect.as_ref().map(|state| &state.progress) {
            None => (t!("bisect.in_progress").into_owned(), false, None),
            Some(BisectProgress::NeedsMarks {
                needs_bad,
                needs_good,
            }) => (
                match (needs_bad, needs_good) {
                    (true, true) => t!("bisect.needs_both"),
                    (true, false) => t!("bisect.needs_bad"),
                    _ => t!("bisect.needs_good"),
                }
                .into_owned(),
                false,
                None,
            ),
            // Git's pick isn't checked out (local changes blocked it, or the user moved
            // HEAD): say so rather than asking about a commit git didn't choose.
            Some(BisectProgress::Testing {
                commit, suggested, ..
            }) if commit != suggested => (
                t!(
                    "bisect.testing_mismatch",
                    sha = short(commit),
                    next = short(suggested)
                )
                .into_owned(),
                true,
                None,
            ),
            Some(BisectProgress::Testing {
                commit,
                revisions_left,
                steps_left,
                ..
            }) => (
                tn!(
                    "bisect.testing",
                    count = *revisions_left,
                    sha = short(commit),
                    steps = tn!("bisect.steps", count = *steps_left)
                )
                .into_owned(),
                true,
                None,
            ),
            // Git tests where the histories meet first; it's outside the counted range.
            Some(BisectProgress::MergeBase { commit }) => (
                t!("bisect.merge_base", sha = short(commit)).into_owned(),
                true,
                None,
            ),
            Some(BisectProgress::Contradictory) => {
                (t!("bisect.contradictory").into_owned(), false, None)
            }
            Some(BisectProgress::Found { first_bad }) => (
                t!("bisect.found", sha = short(first_bad)).into_owned(),
                false,
                Some(*first_bad),
            ),
            Some(BisectProgress::OnlySkipped { candidates }) => (
                tn!("bisect.only_skipped", count = candidates.len()).into_owned(),
                false,
                None,
            ),
        };
        // A bisect of some paths only says which.
        let text = match self.bisect.as_ref().filter(|state| !state.paths.is_empty()) {
            Some(state) => t!(
                "bisect.with_paths",
                status = text,
                paths = crate::util::join_list(state.paths.iter().map(|p| p.display().to_string()))
            )
            .into_owned(),
            None => text,
        };
        let busy = self.remote_op.is_some();
        // A `git bisect run` going on: what it runs and its last line, with Cancel.
        if let Some(running) = &self.bisect_running {
            let text = t!(
                "bisect.running",
                command = running.command.clone(),
                line = running.last_line.clone()
            )
            .into_owned();
            let output = Button::new("bisect-run-output")
                .small()
                .ghost()
                .label(t!("bisect.show_output").into_owned())
                .on_click(cx.listener(|this, _, window, cx| this.open_bisect_output(window, cx)))
                .into_any_element();
            let cancel = Button::new("bisect-run-cancel")
                .small()
                .label(t!("toolbar.cancel").into_owned())
                .on_click(cx.listener(|this, _, _, _| this.cancel_bisect_run()))
                .into_any_element();
            return widgets::banner(
                AppIcon::History,
                look.palette.modified,
                text,
                vec![output, cancel],
                &look,
            );
        }
        let mark_button = |id: &'static str, label: String, mark: BisectMark| {
            Button::new(id)
                .small()
                .label(label)
                .disabled(busy)
                .on_click(
                    cx.listener(move |this, _, window, cx| this.bisect_mark_head(mark, window, cx)),
                )
        };
        let buttons: Vec<AnyElement> = if testing {
            vec![
                mark_button(
                    "bisect-good",
                    t!("bisect.good").into_owned(),
                    BisectMark::Good,
                )
                .into_any_element(),
                mark_button("bisect-bad", t!("bisect.bad").into_owned(), BisectMark::Bad)
                    .into_any_element(),
                mark_button(
                    "bisect-skip",
                    t!("bisect.skip").into_owned(),
                    BisectMark::Skip,
                )
                .into_any_element(),
                Button::new("bisect-run")
                    .small()
                    .label(t!("bisect.run_button").into_owned())
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, window, cx| this.open_bisect_run(window, cx)))
                    .into_any_element(),
            ]
        } else {
            Vec::new()
        };
        let mut buttons = buttons;
        if self.last_bisect_output.is_some() {
            buttons.push(
                Button::new("bisect-output")
                    .small()
                    .ghost()
                    .label(t!("bisect.show_output").into_owned())
                    .on_click(
                        cx.listener(|this, _, window, cx| this.open_bisect_output(window, cx)),
                    )
                    .into_any_element(),
            );
        }
        if let Some(first_bad) = first_bad {
            buttons.push(
                Button::new("bisect-show")
                    .small()
                    .label(t!("bisect.show").into_owned())
                    .on_click(cx.listener(move |this, _, _, cx| this.select_quietly(first_bad, cx)))
                    .into_any_element(),
            );
        }
        buttons.push(
            Button::new("bisect-reset")
                .small()
                .when(found, |this| this.primary())
                .label(t!("bisect.reset").into_owned())
                .disabled(busy)
                .on_click(
                    cx.listener(|this, _, window, cx| this.run_bisect(bisect_reset, window, cx)),
                )
                .into_any_element(),
        );
        widgets::banner(AppIcon::History, tint, text, buttons, &look)
    }
}

#[cfg(test)]
mod tests {
    use super::{BisectLog, OUTPUT_LINES};
    use gpui_kit::{AppContext as _, TestAppContext};
    // Rust's own `#[test]`: `gpui_kit::*` brings GPUI's `test` macro.
    use std::prelude::v1::test;

    /// The log keeps the last lines (the dialog's following is click-tested).
    #[test]
    fn the_log_keeps_the_last_lines() {
        let mut cx = TestAppContext::single();
        let log = cx.new(|_| BisectLog::new());
        cx.update(|cx| {
            log.update(cx, |log, cx| {
                for n in 0..OUTPUT_LINES + 5 {
                    log.push(format!("line {n}"), cx);
                }
            })
        });
        cx.read(|cx| {
            let lines = log.read(cx).lines();
            assert_eq!(lines.len(), OUTPUT_LINES);
            assert_eq!(lines[0], "line 5");
            assert_eq!(lines.last().unwrap(), &format!("line {}", OUTPUT_LINES + 4));
        });
    }
}
