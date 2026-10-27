//! The graph's own actions: how it walks the history, a double click's checkout, and
//! the merge base with HEAD.

use gitweft_core::repo::RefKind;
use gitweft_git::read::{count_commits, count_merges, merge_base};
use gitweft_git::rebase::{RebasePlan, TodoAction, TodoProblem, check};

use crate::graph_view::QuickEdit;

use super::commands::{command, heading};
use super::row_actions::{TipState, branch_in, remote_of};
use super::*;

impl Workspace {
    /// The view menu (or the palette) chose other walk options: they're saved for the
    /// repository and the graph reloads with them.
    pub(super) fn set_walk(&mut self, walk: WalkOptions, cx: &mut Context<Self>) {
        let tips = self.tips.clone().unwrap_or_default();
        self.set_graph(walk, tips, cx);
    }

    /// How the graph walks the history and which refs it starts from: saved for the
    /// repository, then the graph reloads (once, whatever changed).
    fn set_graph(&mut self, walk: WalkOptions, tips: TipFilter, cx: &mut Context<Self>) {
        if self.walk == Some(walk) && self.tips.as_ref() == Some(&tips) {
            return;
        }
        self.walk = Some(walk);
        self.tips = Some(tips.clone());
        self.sync.settings.walk = walk;
        self.sync.settings.tips = tips;
        self.save_repo_settings(cx);
        self.load(cx);
    }

    /// The view menu's All branches or Current branch only: either ends a solo.
    pub(super) fn set_scope(&mut self, current_branch: bool, cx: &mut Context<Self>) {
        let walk = WalkOptions {
            current_branch,
            ..self.walk.unwrap_or_default()
        };
        let tips = TipFilter {
            solo: None,
            ..self.tips.clone().unwrap_or_default()
        };
        self.set_graph(walk, tips, cx);
    }

    /// Nothing hidden, no solo, every branch walked.
    pub(super) fn show_every_branch(&mut self, cx: &mut Context<Self>) {
        let walk = WalkOptions {
            current_branch: false,
            ..self.walk.unwrap_or_default()
        };
        self.set_graph(walk, TipFilter::default(), cx);
    }

    /// Whether `full_name` (a ref, or a remote's `refs/remotes/<name>/`) is hidden from the
    /// graph, or soloed.
    pub(super) fn tip_state(&self, full_name: &str) -> TipState {
        let Some(tips) = &self.tips else {
            return TipState::default();
        };
        TipState {
            hidden: tips.hides(full_name.as_bytes()),
            solo: tips.solo.as_deref() == Some(full_name),
        }
    }

    /// A row's "Hide in graph" / "Show in graph". Showing a ref hidden with its whole
    /// remote shows that remote's branches again.
    pub(super) fn toggle_hidden(&mut self, full_name: String, cx: &mut Context<Self>) {
        let mut tips = self.tips.clone().unwrap_or_default();
        if tips.hides(full_name.as_bytes()) {
            tips.hidden.retain(|entry| {
                !(*entry == full_name || entry.ends_with('/') && full_name.starts_with(entry))
            });
        } else {
            if tips.solo.as_deref() == Some(full_name.as_str()) {
                tips.solo = None;
            }
            tips.hidden.push(full_name);
        }
        let walk = self.walk.unwrap_or_default();
        self.set_graph(walk, tips, cx);
    }

    /// The view menu's "Show …" for a hidden entry (a ref, or a remote's prefix).
    pub(super) fn show_hidden(&mut self, entry: String, cx: &mut Context<Self>) {
        let mut tips = self.tips.clone().unwrap_or_default();
        tips.hidden.retain(|hidden| *hidden != entry);
        let walk = self.walk.unwrap_or_default();
        self.set_graph(walk, tips, cx);
    }

    /// The graph column's width as dragged (or `None`: fit to the lanes), kept for the
    /// repository.
    pub(super) fn set_graph_width(&mut self, width: Option<f32>, cx: &mut Context<Self>) {
        let width = gitweft_core::repo_settings::clamp_graph_width(width);
        if self.sync.settings.graph_width != width {
            self.sync.settings.graph_width = width;
            self.save_repo_settings(cx);
        }
    }

    /// A row's "Solo in graph" (`Some`) or "Stop solo" (`None`).
    pub(super) fn set_solo(&mut self, solo: Option<String>, cx: &mut Context<Self>) {
        let mut tips = self.tips.clone().unwrap_or_default();
        if let Some(name) = &solo {
            tips.hidden.retain(|entry| entry != name);
        }
        tips.solo = solo;
        let walk = self.walk.unwrap_or_default();
        self.set_graph(walk, tips, cx);
    }

    /// A double click on a commit. A local branch on it is checked out (the palette
    /// offers them when there are several), else a remote branch (its local branch, as
    /// the sidebar's Check out does), else, once confirmed, the commit itself. The
    /// checked-out commit does nothing.
    pub(super) fn checkout_from_graph(
        &mut self,
        id: ObjectId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        if head_oid(&repo.refs.head) == Some(id) {
            return;
        }
        let labels = repo.graph.labels.get(&id).cloned().unwrap_or_default();
        let mut choices: Vec<(String, BranchOp)> = labels
            .iter()
            .filter(|label| label.kind == RefKind::Local)
            .map(|label| {
                let op = BranchOp::Switch {
                    name: label.name.clone(),
                };
                (label.name.clone(), op)
            })
            .collect();
        if choices.is_empty() {
            let remotes: Vec<&str> = self.remotes.iter().map(|r| r.name.as_str()).collect();
            choices = labels
                .iter()
                .filter(|label| label.kind == RefKind::Remote)
                .map(|label| {
                    let remote = remote_of(&remotes, &label.name);
                    let op = BranchOp::SwitchRemote {
                        remote: remote.to_owned(),
                        branch: branch_in(remote, &label.name).to_owned(),
                    };
                    (label.name.clone(), op)
                })
                .collect();
        }
        match choices.len() {
            0 => self.confirm_detached_checkout(id, window, cx),
            1 => {
                let (_, op) = choices.remove(0);
                self.run_branch_op(op, window, cx);
            }
            _ => {
                let this = cx.entity().downgrade();
                let group = heading("palette.group.checkout");
                let commands = choices
                    .into_iter()
                    .map(|(name, op)| {
                        let label = t!("branch.checkout_named", name = name);
                        command(&this, &group, label, move |w, window, cx| {
                            w.run_branch_op(op.clone(), window, cx)
                        })
                    })
                    .collect();
                crate::palette::open(commands, window, cx);
            }
        }
    }

    /// A branch label dropped on a commit with local branches (`targets`): the dragged
    /// branch itself isn't one; with several left, the palette asks which.
    pub(super) fn drop_branch_on_branches(
        &mut self,
        full_name: String,
        shown: String,
        targets: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut targets = drop_targets(&full_name, targets);
        match targets.len() {
            0 => {}
            1 => {
                let target = targets.remove(0);
                self.drop_branch_on_branch(full_name, shown, target, window, cx);
            }
            _ => {
                let this = cx.entity().downgrade();
                let group = heading("graph.drop_pick_group");
                let commands = targets
                    .into_iter()
                    .map(|target| {
                        let label = t!("graph.drop_pick_target", branch = shown, target = target);
                        let (full_name, shown) = (full_name.clone(), shown.clone());
                        command(&this, &group, label, move |w, window, cx| {
                            let (full_name, shown) = (full_name.clone(), shown.clone());
                            w.drop_branch_on_branch(full_name, shown, target.clone(), window, cx)
                        })
                    })
                    .collect();
                crate::palette::open(commands, window, cx);
            }
        }
    }

    /// A branch label dropped on another local branch's commit (not the checked-out one):
    /// counts the commits it would bring (a rebase copies every one the target lacks, not
    /// only the branch's own), then asks: merge it into that branch (switching there
    /// first), or, for a local branch, rebase it onto that branch (switching to it first).
    pub(super) fn drop_branch_on_branch(
        &mut self,
        full_name: String,
        shown: String,
        target: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if full_name == format!("refs/heads/{target}") {
            return;
        }
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let refs = &repo.refs;
        let target_id = refs
            .local_branches
            .iter()
            .find(|entry| entry.short_name == target)
            .map(|entry| entry.target);
        let dragged_id = refs
            .local_branches
            .iter()
            .chain(&refs.remote_branches)
            .find(|entry| entry.full_name.as_slice() == full_name.as_bytes())
            .map(|entry| entry.target);
        let (Some(from), Some(to)) = (target_id, dragged_id) else {
            return;
        };
        let git = repo.git.clone();
        let cwd = repo.cwd();
        let job = self.jobs.spawn(Lane::Latest("drop-count"), move |cancel| {
            let commits = count_commits(&git, &cwd, &from, &to, cancel).ok();
            let (from_hex, to_hex) = (from.to_string(), to.to_string());
            let merges = count_merges(&git, &cwd, &from_hex, &to_hex, cancel).unwrap_or(0);
            (commits, merges)
        });
        cx.spawn_in(window, async move |this, cx| {
            let Some((count, merges)) = job.join().await else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.open_drop_choice(full_name, shown, target, count, merges, window, cx)
            })
            .ok();
        })
        .detach();
    }

    /// The choice for a dropped branch, saying how many commits it has that `target` lacks
    /// (`None` when they couldn't be counted). With none there's nothing to merge. With
    /// merges among them (a plain rebase leaves those out), a rebase that keeps them too.
    #[allow(clippy::too_many_arguments)]
    fn open_drop_choice(
        &mut self,
        full_name: String,
        shown: String,
        target: String,
        count: Option<usize>,
        merges: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (branch, into) = (shown.as_str(), target.as_str());
        let mut text = match count {
            Some(0) => t!("graph.drop_text_none", branch = branch, target = into),
            Some(count) => tn!(
                "graph.drop_text",
                count = count,
                branch = branch,
                target = into
            ),
            None => t!("graph.drop_text_any", branch = branch, target = into),
        }
        .into_owned();
        let local = full_name.strip_prefix("refs/heads/");
        if merges > 0 && local.is_some() {
            let note = tn!("graph.drop_merges_note", count = merges);
            text = format!("{text}\n\n{note}");
        }
        let mut choices: Vec<(String, ChoiceAction)> = Vec::new();
        if let Some(local) = local {
            let rebase = |keep_merges: bool| -> ChoiceAction {
                let (local, onto) = (local.to_owned(), target.clone());
                Rc::new(move |this, window, cx| {
                    this.rebase_branch_checked(local.clone(), onto.clone(), keep_merges, window, cx)
                })
            };
            if merges > 0 {
                let label = t!("graph.drop_rebase_merges").into_owned();
                choices.push((label, rebase(true)));
            }
            choices.push((
                t!("graph.drop_rebase", target = into).into_owned(),
                rebase(false),
            ));
        }
        if count != Some(0) {
            let (revision, name, switch_to) = (full_name.clone(), shown.clone(), target.clone());
            let merge: ChoiceAction = Rc::new(move |this, window, cx| {
                let (revision, name, head) = (revision.clone(), name.clone(), switch_to.clone());
                let then: AfterSwitch = Box::new(move |this, window, cx| {
                    this.merge_into(revision, name, head, window, cx)
                });
                this.switch_then(switch_to.clone(), then, window, cx)
            });
            choices.push((t!("graph.drop_merge", target = into).into_owned(), merge));
        }
        self.open_choice(
            t!("graph.drop_title", branch = branch, target = into).into_owned(),
            text,
            t!("toolbar.cancel").into_owned(),
            choices,
            window,
            cx,
        );
    }

    /// Switches to `branch`, then runs `then` once git has done it (never after a failure).
    fn switch_then(
        &mut self,
        branch: String,
        then: AfterSwitch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.after_switch = Some((branch.clone(), then));
        self.run_branch_op(BranchOp::Switch { name: branch }, window, cx);
    }

    /// The rebase of a dropped branch, after the published-commits check: `branch`'s
    /// commits that `onto` lacks are made again, so when a remote already has some of them
    /// it asks first (as a reset or an amend of the checked-out branch does).
    fn rebase_branch_checked(
        &mut self,
        branch: String,
        onto: String,
        keep_merges: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Both by the names git has for them; names shown alike can't be told apart.
        let heads = |bytes: &[u8]| {
            gitweft_git::undo::ref_name_text(&[b"refs/heads/".as_slice(), bytes].concat())
        };
        let resolve =
            |this: &Self, name: &str| this.local_branch_bytes(name).map_err(|()| name.to_owned());
        let (tip, keep) = match (resolve(self, &branch), resolve(self, &onto)) {
            (Ok(tip), Ok(keep)) => (heads(&tip), heads(&keep)),
            (Err(name), _) | (_, Err(name)) => {
                let text = t!("branch.ambiguous_name", name = name).into_owned();
                note(Toast::error(text), cx);
                return;
            }
        };
        self.check_published(
            tip,
            Some(keep),
            move |this, published, window, cx| {
                if published.is_empty() {
                    this.rebase_branch_onto(branch, onto, keep_merges, window, cx);
                    return;
                }
                let message =
                    t!("graph.rebase_published", branch = branch, target = onto).into_owned();
                this.confirm(
                    message,
                    published_text(&published),
                    t!("graph.rebase_anyway").into_owned(),
                    move |this, go, window, cx| {
                        if go {
                            this.rebase_branch_onto(branch, onto, keep_merges, window, cx);
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

    /// `git rebase <onto> <branch>` in two steps: the switch to `branch` (not recorded, as
    /// no switch is), then the rebase of the branch now checked out, recorded for undo.
    /// `keep_merges`: `--rebase-merges` (merges among the commits are made again).
    fn rebase_branch_onto(
        &mut self,
        branch: String,
        onto: String,
        keep_merges: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The names git has for them; `rebase_branch_checked` refused ambiguous ones.
        let (Ok(branch_git), Ok(onto_git)) = (
            self.local_branch_bytes(&branch),
            self.local_branch_bytes(&onto),
        ) else {
            return;
        };
        let label = t!("undo.op_rebase_branch", branch = branch, target = onto).into_owned();
        let done = t!("graph.rebased", branch = branch, target = onto).into_owned();
        let conflict = Some((
            t!("graph.rebase_conflict_title", branch = branch).into_owned(),
            t!("rewrite.conflict_text").into_owned(),
        ));
        self.run_entry_op(
            t!("graph.rebase_failed").into_owned(),
            conflict,
            move |git, cwd, cancel| {
                use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
                switch_branch(
                    git,
                    cwd,
                    std::ffi::OsString::from_vec(branch_git.clone()),
                    cancel,
                )?;
                let record = Record::checked_out(label, Worktree::Follow);
                let onto = [b"refs/heads/".as_slice(), &onto_git].concat();
                recorded(git, cwd, Some(record), cancel, || {
                    let onto = std::ffi::OsStr::from_bytes(&onto);
                    rewrite::rebase_onto(git, cwd, onto, keep_merges, cancel)
                })
                .map(|((), entry)| (done, entry))
            },
            window,
            cx,
        );
    }

    /// The graph's quick rebase actions: one commit reworded, squashed into its parent or
    /// dropped, through a plan of the commits after it (no editor). The plan is read in the
    /// background; [`Self::quick_plan`] edits and starts it.
    pub(super) fn quick_rebase(
        &mut self,
        commit: RowCommit,
        edit: QuickEdit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let git = repo.git.clone();
        let cwd = repo.cwd();
        let id = commit.id;
        let job = self.jobs.spawn(Lane::Write, move |cancel| {
            // A commit too close to the first one gets the plan from the first commit on.
            let from = |back| gitweft_git::rebase::plan_before(&git, &cwd, id, back, cancel);
            match edit {
                QuickEdit::Reword => from(1),
                QuickEdit::SquashIntoParent | QuickEdit::FixupIntoParent => from(2),
                // Dropping the newest commit alone would keep nothing: start one further back.
                QuickEdit::Drop => {
                    let plan = from(1)?;
                    if plan.items.len() == 1 && plan.base.is_some() {
                        from(2)
                    } else {
                        Ok(plan)
                    }
                }
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let Some(result) = job.join().await else {
                return;
            };
            this.update_in(cx, |this, window, cx| match result {
                Ok(plan) => this.quick_plan(plan, commit, edit, window, cx),
                Err(err) => this.plan_failed(err, false, window, cx),
            })
            .ok();
        })
        .detach();
    }

    /// Sets the commit's action in `plan` and starts it (a reword asks for the message, a
    /// drop asks first). The commit must be in the plan: one on another branch isn't.
    fn quick_plan(
        &mut self,
        mut plan: RebasePlan,
        commit: RowCommit,
        edit: QuickEdit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = plan.items.iter().position(|item| item.commit == commit.id) else {
            let text = t!("rebase.not_on_branch").into_owned();
            note(Toast::warning(text), cx);
            return;
        };
        let folds = matches!(
            edit,
            QuickEdit::SquashIntoParent | QuickEdit::FixupIntoParent
        );
        // The parent is planned before the commit, unless there's none: the first commit.
        if folds && index == 0 {
            let text = t!("rebase.no_parent").into_owned();
            note(Toast::warning(text), cx);
            return;
        }
        let summary = commit.summary.as_str();
        match edit {
            QuickEdit::SquashIntoParent | QuickEdit::FixupIntoParent => {
                plan.items[index].action = if edit == QuickEdit::SquashIntoParent {
                    TodoAction::Squash
                } else {
                    TodoAction::Fixup
                };
                self.start_rebase(plan, window, cx);
            }
            QuickEdit::Drop => {
                let RepoState::Loaded(repo) = &self.state else {
                    return;
                };
                let branch = head_label(&repo.refs.head);
                plan.items[index].action = TodoAction::Drop;
                // The branch's only commit.
                if check(&plan.items) == Err(TodoProblem::NothingKept) {
                    let text = t!("rebase.drop_only").into_owned();
                    note(Toast::warning(text), cx);
                    return;
                }
                self.ask(
                    PromptLevel::Warning,
                    t!("rebase.drop_confirm", summary = summary).into_owned(),
                    t!("rebase.drop_detail", branch = branch).into_owned(),
                    t!("rebase.drop_button").into_owned(),
                    move |this, confirmed, window, cx| {
                        if confirmed {
                            this.start_rebase(plan, window, cx);
                        }
                    },
                    window,
                    cx,
                );
            }
            QuickEdit::Reword => {
                let spec = TextDialog {
                    title: t!("rebase.reword_title", summary = summary).into_owned(),
                    subtitle: None,
                    placeholder: t!("rebase.reword_placeholder").into_owned(),
                    initial: plan.items[index].message.clone(),
                    detail: None,
                    checkbox: None,
                    ok_label: t!("rebase.reword_ok").into_owned(),
                    allow_empty: false,
                    multiline: true,
                };
                // The dialog's submit can be called more than once; the plan runs once.
                let plan = Rc::new(RefCell::new(Some(plan)));
                let submit: SubmitText = Rc::new(move |this, text, _, window, cx| {
                    let Some(mut plan) = plan.borrow_mut().take() else {
                        return;
                    };
                    plan.items[index].action = TodoAction::Reword;
                    plan.items[index].message = text.trim().to_owned();
                    this.start_rebase(plan, window, cx);
                });
                self.open_text_dialog(spec, submit, window, cx);
            }
        }
    }

    /// Row menu: commits what's staged as `fixup! <summary>` (for a later autosquash),
    /// recorded for undo like any commit.
    pub(super) fn create_fixup(
        &mut self,
        commit: RowCommit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let summary = commit.summary.as_str();
        let label = t!("undo.op_fixup", summary = summary).into_owned();
        let done = t!("rebase.fixup_done", summary = summary).into_owned();
        let id = commit.id;
        self.run_recorded_op(
            t!("rebase.fixup_failed").into_owned(),
            None,
            Some(Record::checked_out(label, Worktree::Soft)),
            move |git, cwd, cancel| {
                rewrite::commit_fixup(git, cwd, id, cancel)?;
                Ok(done)
            },
            window,
            cx,
        );
    }

    /// No branch points at the commit: checking it out detaches HEAD, so ask first.
    fn confirm_detached_checkout(
        &mut self,
        id: ObjectId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sha = id.to_hex_with_len(8).to_string();
        self.ask(
            PromptLevel::Info,
            t!("graph.checkout_detached_confirm", sha = sha).into_owned(),
            t!("graph.checkout_detached_detail").into_owned(),
            t!("branch.checkout").into_owned(),
            move |this, confirmed, window, cx| {
                if confirmed {
                    this.run_branch_op(BranchOp::Detach { commit: id }, window, cx);
                }
            },
            window,
            cx,
        );
    }

    /// Row menu: selects the newest commit that the commit's history shares with HEAD's
    /// (`git merge-base`), or says why there's none to show.
    pub(super) fn select_merge_base(&mut self, id: ObjectId, cx: &mut Context<Self>) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let Some(head) = head_oid(&repo.refs.head) else {
            return;
        };
        let git = repo.git.clone();
        let cwd = repo.cwd();
        let job = self.jobs.spawn(Lane::Latest("merge-base"), move |cancel| {
            merge_base(&git, &cwd, head, id, cancel).map_err(|err| err.to_string())
        });
        cx.spawn(async move |this, cx| {
            let Some(result) = job.join().await else {
                return;
            };
            this.update(cx, |this, cx| {
                let notification = match result {
                    Ok(Some(base)) if base != id => return this.show_commit(base, cx),
                    Ok(Some(_)) => Toast::info(t!("graph.merge_base_is_commit")),
                    Ok(None) => Toast::warning(t!("graph.no_merge_base")),
                    Err(error) => Toast::error(t!("graph.merge_base_failed", error = error)),
                };
                this.with_window(move |_, _, cx| note(notification, cx), cx);
            })
            .ok();
        })
        .detach();
    }
}

/// The branches a dropped branch can go onto: the local branches of the commit dropped
/// on, less the dragged branch itself.
fn drop_targets(full_name: &str, targets: Vec<String>) -> Vec<String> {
    targets
        .into_iter()
        .filter(|target| full_name.strip_prefix("refs/heads/") != Some(target.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::drop_targets;

    #[test]
    fn a_branch_is_never_dropped_onto_itself() {
        let targets = |names: &[&str]| names.iter().map(|name| name.to_string()).collect();
        assert_eq!(
            drop_targets("refs/heads/topic", targets(&["release", "topic", "hotfix"])),
            ["release", "hotfix"]
        );
        assert!(drop_targets("refs/heads/topic", targets(&["topic"])).is_empty());
        // A remote branch is never among the local targets' names.
        assert_eq!(
            drop_targets("refs/remotes/origin/topic", targets(&["topic"])),
            ["topic"]
        );
    }
}
