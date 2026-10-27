//! Interactive rebase, planned in the app and run by git.
//!
//! [`plan`] lists the commits from a base (exclusive) to HEAD, oldest first. The user
//! reorders them and picks an action for each ([`TodoItem`]); [`check`] explains what
//! can't work, and [`compile`] turns the list into git's todo. A new message (reword)
//! is applied by an `exec git commit --amend` after the commit and whatever is folded
//! into it, so git never needs an editor. [`autosquash`] moves `fixup!`/`squash!`
//! commits under their targets, as `git rebase --autosquash` would.
//!
//! [`rebase_interactive`] runs `git rebase -i` with our own binary as the sequence
//! editor ([`run_sequence_editor`]). That replaces git's todo with ours only if git
//! listed exactly the planned commits, in order; otherwise it fails, and git gives up
//! before touching anything. A conflict stops with the rebase in progress, as with any
//! rebase: continue, skip or abort it from the banner. So do an `edit` and a stop the
//! user asked for ([`TodoItem::stop_after`]), without the conflict.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use gix::ObjectId;

use crate::cancel::CancelToken;
use crate::cli::{GitCli, GitError, GitErrorKind};

/// Where a running rebase keeps its plan and new messages, inside the git dir. Its exec
/// lines read the messages, so the folder stays while the rebase is stopped.
const PLAN_DIR: &str = "gitweft-rebase";
const PLAN_HEADER: &str = "gitweft rebase plan 1";
/// Our sequence editor's complaint when git's todo isn't the planned one.
pub const MISMATCH: &str = "the branch no longer has the commits the rebase was planned for";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TodoAction {
    Pick,
    /// Pick, with [`TodoItem::message`] as its new message.
    Reword,
    /// Pick, then stop so the commit can be amended (`edit`).
    Edit,
    /// Fold into the commit kept before it, adding this message to that one.
    Squash,
    /// Fold into the commit kept before it, dropping this message.
    Fixup,
    Drop,
}

impl TodoAction {
    /// Whether the commit is folded into the one kept before it.
    pub fn folds(self) -> bool {
        matches!(self, TodoAction::Squash | TodoAction::Fixup)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoItem {
    pub commit: ObjectId,
    /// The commit's message; for [`TodoAction::Reword`], the message it gets.
    pub message: String,
    pub action: TodoAction,
    /// Stop the rebase once this item is done (a `break`): after the commit, what is
    /// folded into it and its new message; a dropped item stops where it was.
    pub stop_after: bool,
}

/// How [`rebase_interactive`] ended, short of an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebaseOutcome {
    Finished,
    /// An `edit` or a `break` paused it: the rebase is in progress, to be continued or
    /// aborted like any other.
    Stopped,
}

impl TodoItem {
    pub fn summary(&self) -> &str {
        self.message.lines().next().unwrap_or("")
    }
}

/// An interactive rebase of the commits after `base`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebasePlan {
    /// `None`: every commit from the first one on (`git rebase --root`), so the first
    /// commit can be reworded, dropped or folded into too.
    pub base: Option<ObjectId>,
    /// The commits after `base`, oldest first, as git will list them.
    pub original: Vec<ObjectId>,
    /// What to do with them, in the new order.
    pub items: Vec<TodoItem>,
}

/// Why a todo can't run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoProblem {
    /// Every commit is dropped.
    NothingKept,
    /// The squash or fixup at this index has no kept commit before it.
    NothingToFoldInto(usize),
    /// The reword at this index has an empty message.
    EmptyMessage(usize),
    /// The items aren't the planned commits, each once.
    OtherCommits,
    /// A path the todo would name (git, a message file, the plan) isn't UTF-8.
    PathNotUtf8,
}

impl std::fmt::Display for TodoProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TodoProblem::NothingKept => write!(f, "every commit is dropped"),
            TodoProblem::NothingToFoldInto(index) => {
                write!(
                    f,
                    "commit {} has no commit before it to fold into",
                    index + 1
                )
            }
            TodoProblem::EmptyMessage(index) => {
                write!(f, "commit {} would get an empty message", index + 1)
            }
            TodoProblem::OtherCommits => write!(f, "the list isn't the branch's commits"),
            TodoProblem::PathNotUtf8 => write!(
                f,
                "a path the rebase's commands would name isn't UTF-8 (the repository's or git's)"
            ),
        }
    }
}

/// What makes `items` impossible to run, if anything.
pub fn check(items: &[TodoItem]) -> Result<(), TodoProblem> {
    let mut kept = false;
    for (index, item) in items.iter().enumerate() {
        match item.action {
            TodoAction::Drop => {}
            action if action.folds() && !kept => {
                return Err(TodoProblem::NothingToFoldInto(index));
            }
            TodoAction::Reword if item.message.trim().is_empty() => {
                return Err(TodoProblem::EmptyMessage(index));
            }
            _ => kept = true,
        }
    }
    if kept {
        Ok(())
    } else {
        Err(TodoProblem::NothingKept)
    }
}

/// Git's todo for `items`. The new message of the reword at index `i` must be in
/// `message_file(i)`; it's applied once the commit and everything folded into it are in.
///
/// A [`TodoItem::stop_after`] becomes a `break` once its group is complete: after the
/// commit, the squashes and fixups that follow it, and the reword's `exec`. A dropped
/// item's `break` comes right after its `drop` line, unless the group it sits in goes on
/// (a fold follows), which then completes first. Stops that land at the same place make
/// one `break`.
///
/// `git`: the program the rewords' `exec` runs, quoted for `sh` (the configured git, not
/// whatever `git` is on PATH, #16).
pub fn compile(
    items: &[TodoItem],
    git: &str,
    message_file: impl Fn(usize) -> PathBuf,
) -> Result<String, TodoProblem> {
    check(items)?;
    // The rewords' message files as the todo names them: a path it can't hold as text is
    // refused, rather than written lossily (naming another file).
    let mut messages = std::collections::HashMap::new();
    for (index, item) in items.iter().enumerate() {
        if item.action == TodoAction::Reword {
            let quoted = shell_quote(&message_file(index)).ok_or(TodoProblem::PathNotUtf8)?;
            messages.insert(index, quoted);
        }
    }
    let mut todo = String::new();
    // What completes the open group: the reword's exec, then the break.
    let mut reword: Option<usize> = None;
    let mut stop = false;
    let close = |todo: &mut String, reword: &mut Option<usize>, stop: &mut bool| {
        if let Some(index) = reword.take() {
            let _ = writeln!(
                todo,
                "exec {git} commit --amend --only --allow-empty --quiet --file={}",
                messages[&index]
            );
        }
        if std::mem::take(stop) {
            todo.push_str("break\n");
        }
    };
    for (index, item) in items.iter().enumerate() {
        let verb = match item.action {
            TodoAction::Pick | TodoAction::Reword | TodoAction::Edit => {
                close(&mut todo, &mut reword, &mut stop);
                match item.action {
                    TodoAction::Reword => {
                        reword = Some(index);
                        "pick"
                    }
                    TodoAction::Edit => "edit",
                    _ => "pick",
                }
            }
            TodoAction::Squash => "squash",
            TodoAction::Fixup => "fixup",
            TodoAction::Drop => "drop",
        };
        let summary = item.summary().replace('\r', "");
        let _ = writeln!(todo, "{verb} {} {summary}", item.commit);
        stop |= item.stop_after;
        // Drops change nothing, so the group is complete unless a fold is still to come.
        let folds_follow = items[index + 1..]
            .iter()
            .find(|next| next.action != TodoAction::Drop)
            .is_some_and(|next| next.action.folds());
        if stop && !folds_follow {
            close(&mut todo, &mut reword, &mut stop);
        }
    }
    close(&mut todo, &mut reword, &mut stop);
    Ok(todo)
}

/// `items` (oldest first) rearranged the way `git rebase --autosquash` would: an item whose
/// summary starts with `fixup! `, `amend! ` or `squash! ` (repeated prefixes count once)
/// moves right after the last item of its target's group, as a fixup (`fixup!`,
/// `amend!`) or squash (`squash!`). The target is the first earlier item whose summary
/// is the rest of the line, or whose commit id starts with it (4 hex digits or more).
/// Items without an earlier target stay as they are. `amend!` doesn't replace the
/// target's message here.
pub fn autosquash(items: &[TodoItem]) -> Vec<TodoItem> {
    let mut groups: Vec<Vec<TodoItem>> = Vec::new();
    // The group each item so far went to.
    let mut group_of: Vec<usize> = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let earlier = &items[..index];
        let target = fixup_target(item.summary()).and_then(|(action, rest)| {
            let by_summary = earlier.iter().position(|e| e.summary() == rest);
            let by_id = || {
                let hex = rest.to_ascii_lowercase();
                // Up to a whole SHA-256 id (64 digits); SHA-1 ids are 40.
                let is_id = (4..=64).contains(&hex.len())
                    && hex.bytes().all(|byte| byte.is_ascii_hexdigit());
                is_id
                    .then(|| {
                        earlier
                            .iter()
                            .position(|e| e.commit.to_string().starts_with(&hex))
                    })
                    .flatten()
            };
            Some((action, by_summary.or_else(by_id)?))
        });
        match target {
            Some((action, target)) => {
                let group = group_of[target];
                groups[group].push(TodoItem {
                    action,
                    ..item.clone()
                });
                group_of.push(group);
            }
            None => {
                group_of.push(groups.len());
                groups.push(vec![item.clone()]);
            }
        }
    }
    groups.into_iter().flatten().collect()
}

/// The squashes (not fixups) folded into the kept item at `head`: the items after it up to
/// the next kept one, drops passed over.
pub fn squashes_into(items: &[TodoItem], head: usize) -> Vec<usize> {
    let mut found = Vec::new();
    for (index, item) in items.iter().enumerate().skip(head + 1) {
        match item.action {
            TodoAction::Squash => found.push(index),
            TodoAction::Fixup | TodoAction::Drop => {}
            _ => break,
        }
    }
    found
}

/// The message the kept item at `head` ends up with when the squashes after it are folded
/// in, as `git rebase -i` combines them: `head_message` (its message, or its new one), then
/// each squash's, a blank line apart. Fixups add nothing.
pub fn combined_message(items: &[TodoItem], head: usize, head_message: &str) -> String {
    let squashes = squashes_into(items, head);
    std::iter::once(head_message)
        .chain(squashes.iter().map(|&index| items[index].message.as_str()))
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// A commit as the rebase leaves it, for a preview of the result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The kept commit it's made from.
    pub commit: ObjectId,
    /// Its summary afterwards (a reword's new one).
    pub summary: String,
    /// How many commits are folded into it (squashes and fixups).
    pub folded: usize,
    /// The rebase stops at it (an edit) or after it (a pause: its own, a fold's, or that
    /// of a dropped commit just after it).
    pub stops: bool,
}

/// What the branch becomes when `items` run, oldest first: one entry per kept commit, with
/// the commits folded into it counted. Drops leave nothing.
pub fn outcome(items: &[TodoItem]) -> Vec<Outcome> {
    let mut result: Vec<Outcome> = Vec::new();
    for item in items {
        match item.action {
            TodoAction::Drop | TodoAction::Squash | TodoAction::Fixup => {
                if let Some(last) = result.last_mut() {
                    if item.action.folds() {
                        last.folded += 1;
                    }
                    last.stops |= item.stop_after;
                }
            }
            action => result.push(Outcome {
                commit: item.commit,
                summary: item.summary().to_owned(),
                folded: 0,
                stops: item.stop_after || action == TodoAction::Edit,
            }),
        }
    }
    result
}

/// For a `fixup! `, `amend! ` or `squash! ` summary: the action it asks for (from its
/// first prefix) and what names the target (after all of them).
fn fixup_target(summary: &str) -> Option<(TodoAction, &str)> {
    fn strip(text: &str) -> Option<(TodoAction, &str)> {
        [
            ("fixup! ", TodoAction::Fixup),
            ("amend! ", TodoAction::Fixup),
            ("squash! ", TodoAction::Squash),
        ]
        .into_iter()
        .find_map(|(prefix, action)| Some((action, text.strip_prefix(prefix)?)))
    }
    let (action, mut rest) = strip(summary)?;
    loop {
        rest = rest.trim_start();
        match strip(rest) {
            Some((_, inner)) => rest = inner,
            None => return Some((action, rest)),
        }
    }
}

/// The commits git's own todo picks, in order; `None` if it holds anything but picks
/// (merges, labels, `update-ref`…), which no plan of ours can match.
pub fn parse_picks(todo: &str) -> Option<Vec<String>> {
    let mut ids = Vec::new();
    for line in todo.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut words = line.split_whitespace();
        match (words.next(), words.next()) {
            (Some("pick" | "p"), Some(id)) => ids.push(id.to_owned()),
            (Some("noop"), _) => {}
            _ => return None,
        }
    }
    Some(ids)
}

/// The commits after `base` (which must be in HEAD's history), oldest first, each
/// picked. Merges in between are refused: they'd need `--rebase-merges`.
pub fn plan(
    git: &GitCli,
    workdir: &Path,
    base: &str,
    cancel: &CancelToken,
) -> Result<RebasePlan, GitError> {
    let base = resolve_commit(git, workdir, base, cancel)?;
    plan_after(git, workdir, Some(base), cancel)
}

/// The plan that starts `back` commits before `commit` (along first parents), so that
/// `commit` and the `back - 1` commits before it are in it. A commit with fewer than
/// `back` commits in its first-parent history gets the plan from the first commit on
/// (`base: None`): reworking the first commit, or folding into it, needs no base.
pub fn plan_before(
    git: &GitCli,
    workdir: &Path,
    commit: ObjectId,
    back: usize,
    cancel: &CancelToken,
) -> Result<RebasePlan, GitError> {
    let depth = git
        .command(workdir)
        .args(["rev-list", "--first-parent", "--count"])
        .arg(commit.to_string())
        .arg("--")
        .read_only()
        .run(cancel)?;
    let depth: usize = depth.stdout_str().trim().parse().map_err(|_| {
        GitError::new(
            GitErrorKind::Other,
            "git rev-list",
            "unexpected `git rev-list --count` output",
        )
    })?;
    if depth > back {
        plan(git, workdir, &format!("{commit}~{back}"), cancel)
    } else {
        plan_after(git, workdir, None, cancel)
    }
}

fn resolve_commit(
    git: &GitCli,
    workdir: &Path,
    revision: &str,
    cancel: &CancelToken,
) -> Result<ObjectId, GitError> {
    let output = git
        .command(workdir)
        .args(["rev-parse", "--verify", "--end-of-options"])
        .arg(format!("{revision}^{{commit}}"))
        .read_only()
        .run(cancel)?;
    ObjectId::from_hex(output.stdout_str().trim().as_bytes())
        .map_err(|e| GitError::new(GitErrorKind::Other, "git rev-parse", e.to_string()))
}

/// In `range` (which ends at HEAD and holds merges): the merge after which HEAD's commits
/// hold none, so a plan from it is a straight line. That's the first merge in topological
/// order: another merge among the commits after it that doesn't descend from it would
/// meet its line at a merge that does, which would come first. `None` when HEAD is that
/// merge: nothing follows it.
fn after_last_merge(
    git: &GitCli,
    workdir: &Path,
    range: &str,
    cancel: &CancelToken,
) -> Result<Option<ObjectId>, GitError> {
    let output = git
        .command(workdir)
        .args([
            "rev-list",
            "-1",
            "--topo-order",
            "--min-parents=2",
            range,
            "--",
        ])
        .read_only()
        .run(cancel)?;
    let Some(hex) = output
        .stdout_str()
        .split_whitespace()
        .next()
        .map(str::to_owned)
    else {
        return Ok(None);
    };
    let merge = ObjectId::from_hex(hex.as_bytes())
        .map_err(|e| GitError::new(GitErrorKind::Other, "git rev-list", e.to_string()))?;
    let head = resolve_commit(git, workdir, "HEAD", cancel)?;
    Ok((merge != head).then_some(merge))
}

/// [`plan`] of the commits after `base`, or of every commit in HEAD's history when
/// `None`.
fn plan_after(
    git: &GitCli,
    workdir: &Path,
    base: Option<ObjectId>,
    cancel: &CancelToken,
) -> Result<RebasePlan, GitError> {
    // The texts are for the activity log; the UI words each kind itself.
    let refuse = |kind, text: &str| GitError::new(kind, "git rebase", text);
    if let Some(base) = base {
        let ancestor = git
            .command(workdir)
            .args(["merge-base", "--is-ancestor"])
            .arg(base.to_string())
            .arg("HEAD")
            .allow_exit_codes(&[1])
            .read_only()
            .run(cancel)?;
        if ancestor.status == 1 {
            return Err(refuse(
                GitErrorKind::NotInHistory,
                "That commit isn't in the current branch's history, so there is nothing to \
                 rebase onto it.",
            ));
        }
    }
    let range = match base {
        Some(base) => format!("{base}..HEAD"),
        None => "HEAD".to_owned(),
    };
    let merges = git
        .command(workdir)
        .args(["rev-list", "--count", "--min-parents=2", &range, "--"])
        .read_only()
        .run(cancel)?;
    let count: usize = merges.stdout_str().trim().parse().unwrap_or(1);
    if count > 0 {
        let after = after_last_merge(git, workdir, &range, cancel)?;
        return Err(refuse(
            GitErrorKind::MergesInRange { count, after },
            "The commits after that one include merges, which interactive rebase can't keep \
             yet.",
        ));
    }
    let output = git
        .command(workdir)
        .args(["log", "-z", "--reverse", "--format=%H%n%B", &range, "--"])
        .read_only()
        .run(cancel)?;
    let text = output.stdout_str();
    let items: Vec<TodoItem> = text
        .split('\0')
        .filter_map(|record| {
            let (id, message) = record.trim_start_matches('\n').split_once('\n')?;
            Some(TodoItem {
                commit: ObjectId::from_hex(id.as_bytes()).ok()?,
                message: message.trim_end().to_owned(),
                action: TodoAction::Pick,
                stop_after: false,
            })
        })
        .collect();
    if items.is_empty() {
        return Err(refuse(
            GitErrorKind::NothingToRebase,
            "There are no commits after that one.",
        ));
    }
    Ok(RebasePlan {
        base,
        original: items.iter().map(|item| item.commit).collect(),
        items,
    })
}

/// Runs the plan (`git rebase -i`, with `helper --sequence-editor <plan>` as the
/// sequence editor). A conflict stops with the rebase in progress
/// ([`GitErrorKind::Conflict`]); an `edit` or `break` stops it too, but that's
/// [`RebaseOutcome::Stopped`]. If the branch changed since [`plan`], nothing happens and
/// the error is [`GitErrorKind::RefMoved`].
pub fn rebase_interactive(
    git: &GitCli,
    workdir: &Path,
    plan: &RebasePlan,
    helper: &Path,
    cancel: &CancelToken,
) -> Result<RebaseOutcome, GitError> {
    let problem = |p: TodoProblem| GitError::new(GitErrorKind::Other, "git rebase", p.to_string());
    let mut planned = plan.original.clone();
    let mut listed: Vec<ObjectId> = plan.items.iter().map(|item| item.commit).collect();
    planned.sort();
    listed.sort();
    if planned != listed {
        return Err(problem(TodoProblem::OtherCommits));
    }
    check(&plan.items).map_err(problem)?;

    let output = git
        .command(workdir)
        .args(["rev-parse", "--absolute-git-dir"])
        .read_only()
        .run(cancel)?;
    // As bytes: a lossy path would put the message files somewhere else.
    let listed = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
    let git_dir =
        PathBuf::from(<std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(listed));
    let dir = git_dir.join(PLAN_DIR);
    let io = |e: std::io::Error| GitError::new(GitErrorKind::Other, "rebase plan", e.to_string());
    // Left over from a rebase that was aborted or finished in a terminal.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(io)?;
    let message_file = |index: usize| dir.join(format!("{index}.msg"));
    for (index, item) in plan.items.iter().enumerate() {
        if item.action == TodoAction::Reword {
            std::fs::write(message_file(index), &item.message).map_err(io)?;
        }
    }
    // The configured git (`GITWEFT_GIT`, or `git` on the host in the Flatpak, where the
    // rebase runs too), not whatever `git` is on PATH (#16).
    let not_text = || problem(TodoProblem::PathNotUtf8);
    let program = shell_quote(git.program()).ok_or_else(not_text)?;
    let todo = compile(&plan.items, &program, message_file).map_err(problem)?;
    let plan_file = dir.join("plan");
    std::fs::write(&plan_file, plan_text(&plan.original, &todo)).map_err(io)?;

    let (Some(helper), Some(plan_path)) = (shell_quote(helper), shell_quote(&plan_file)) else {
        return Err(not_text());
    };
    let editor = format!("{helper} --sequence-editor {plan_path}");
    let result = git
        .command(workdir)
        .args([
            "rebase",
            "--interactive",
            "--no-autosquash",
            "--no-update-refs",
            "--no-rebase-merges",
        ])
        .arg(
            plan.base
                .map_or_else(|| "--root".to_owned(), |base| base.to_string()),
        )
        .env("GIT_SEQUENCE_EDITOR", editor)
        // Squashes combine the messages without asking.
        .env("GIT_EDITOR", "true")
        .run(cancel);
    let stopped = git_dir.join("rebase-merge").is_dir();
    if !stopped {
        let _ = std::fs::remove_dir_all(&dir);
    }
    match result {
        Ok(_) if stopped => Ok(RebaseOutcome::Stopped),
        Ok(_) => Ok(RebaseOutcome::Finished),
        Err(err) if err.stderr.contains(MISMATCH) => Err(GitError::new(
            GitErrorKind::RefMoved,
            "git rebase",
            "The branch changed since the rebase was planned, so nothing was changed. Open \
             the rebase again.",
        )),
        Err(err) => Err(err),
    }
}

/// Our side of `GIT_SEQUENCE_EDITOR`: replaces git's todo (at `todo`) with the one in
/// `plan`, if git listed exactly the planned commits in order. Otherwise returns
/// [`MISMATCH`] and leaves the todo alone; the caller exits non-zero, and git aborts.
pub fn run_sequence_editor(plan: &Path, todo: &Path) -> Result<(), String> {
    let text =
        std::fs::read_to_string(plan).map_err(|e| format!("can't read the rebase plan: {e}"))?;
    let (expected, ours) = parse_plan(&text).ok_or("the rebase plan is unreadable")?;
    let theirs =
        std::fs::read_to_string(todo).map_err(|e| format!("can't read git's todo: {e}"))?;
    let picks = parse_picks(&theirs).ok_or(MISMATCH)?;
    let same = picks.len() == expected.len()
        && picks
            .iter()
            .zip(&expected)
            .all(|(short, full)| short.len() >= 4 && full.starts_with(short.as_str()));
    if !same {
        return Err(MISMATCH.to_owned());
    }
    std::fs::write(todo, ours).map_err(|e| format!("can't write git's todo: {e}"))
}

fn plan_text(expected: &[ObjectId], todo: &str) -> String {
    let mut text = format!("{PLAN_HEADER}\n");
    for id in expected {
        let _ = writeln!(text, "expect {id}");
    }
    text.push_str("todo\n");
    text.push_str(todo);
    text
}

/// The expected commits and our todo.
fn parse_plan(text: &str) -> Option<(Vec<String>, &str)> {
    let rest = text.strip_prefix(PLAN_HEADER)?.strip_prefix('\n')?;
    let (head, todo) = rest.split_once("todo\n")?;
    let expected = head
        .lines()
        .map(|line| line.strip_prefix("expect ").map(str::to_owned))
        .collect::<Option<Vec<_>>>()?;
    Some((expected, todo))
}

/// `path` in single quotes for `sh`; `None` when it isn't UTF-8 (the todo is text, and a
/// lossy path would name another file, #16).
fn shell_quote(path: &Path) -> Option<String> {
    Some(format!("'{}'", path.to_str()?.replace('\'', "'\\''")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn id(byte: u8) -> ObjectId {
        ObjectId::from_bytes_or_panic(&[byte; 20])
    }

    fn item(byte: u8, action: TodoAction) -> TodoItem {
        TodoItem {
            commit: id(byte),
            message: format!("m{byte}\n\nbody"),
            action,
            stop_after: false,
        }
    }

    fn stop(item: TodoItem) -> TodoItem {
        TodoItem {
            stop_after: true,
            ..item
        }
    }

    fn named(byte: u8, summary: &str) -> TodoItem {
        TodoItem {
            message: format!("{summary}\n\nbody"),
            ..item(byte, TodoAction::Pick)
        }
    }

    /// Commit bytes and actions of `items`.
    fn shape(items: &[TodoItem]) -> Vec<(u8, TodoAction)> {
        items
            .iter()
            .map(|item| (item.commit.as_bytes()[0], item.action))
            .collect()
    }

    fn file(index: usize) -> PathBuf {
        PathBuf::from(format!("/g/it's/{index}.msg"))
    }

    #[test]
    fn a_squash_groups_message_is_the_kept_ones_then_each_squash() {
        use TodoAction::*;
        let items = [
            item(1, Pick),
            item(2, Squash),
            item(3, Fixup),
            item(4, Drop),
            item(5, Squash),
            item(6, Pick),
            item(7, Fixup),
        ];
        assert_eq!(squashes_into(&items, 0), [1, 4]);
        assert_eq!(
            combined_message(&items, 0, "m1\n\nbody"),
            "m1\n\nbody\n\nm2\n\nbody\n\nm5\n\nbody"
        );
        assert!(
            squashes_into(&items, 5).is_empty(),
            "a fixup adds no message"
        );
        assert_eq!(combined_message(&items, 5, "  New  \n"), "New");
    }

    #[test]
    fn the_outcome_is_one_commit_per_kept_one() {
        use TodoAction::*;
        let reworded = TodoItem {
            message: "New\n\nbody".into(),
            ..item(5, Reword)
        };
        let items = [
            item(1, Pick),
            item(2, Squash),
            stop(item(3, Fixup)),
            item(4, Drop),
            reworded,
            item(6, Edit),
            item(7, Pick),
            stop(item(8, Drop)),
        ];
        let got: Vec<_> = outcome(&items)
            .into_iter()
            .map(|o| (o.commit.as_bytes()[0], o.summary, o.folded, o.stops))
            .collect();
        assert_eq!(
            got,
            [
                (1, "m1".to_owned(), 2, true),
                (5, "New".to_owned(), 0, false),
                (6, "m6".to_owned(), 0, true),
                (7, "m7".to_owned(), 0, true),
            ]
        );
    }

    #[test]
    fn problems() {
        use TodoAction::*;
        assert_eq!(check(&[item(1, Drop)]), Err(TodoProblem::NothingKept));
        assert_eq!(
            check(&[item(1, Drop), item(2, Fixup), item(3, Pick)]),
            Err(TodoProblem::NothingToFoldInto(1))
        );
        let mut empty = item(1, Reword);
        empty.message = " \n".into();
        assert_eq!(check(&[empty]), Err(TodoProblem::EmptyMessage(0)));
        assert_eq!(
            check(&[item(1, Pick), item(2, Drop), item(3, Squash)]),
            Ok(())
        );
    }

    #[test]
    fn rewords_apply_after_what_is_folded_into_them() {
        use TodoAction::*;
        let items = [
            item(1, Reword),
            item(2, Fixup),
            item(3, Drop),
            item(4, Squash),
            item(5, Pick),
            item(6, Reword),
        ];
        let exec = |index| {
            format!(
                "exec git commit --amend --only --allow-empty --quiet --file='/g/it'\\''s/{index}.msg'\n"
            )
        };
        let expected = format!(
            "pick {} m1\nfixup {} m2\ndrop {} m3\nsquash {} m4\n{}pick {} m5\npick {} m6\n{}",
            id(1),
            id(2),
            id(3),
            id(4),
            exec(0),
            id(5),
            id(6),
            exec(5)
        );
        assert_eq!(compile(&items, "git", file).unwrap(), expected);
    }

    #[test]
    fn the_rewords_exec_runs_the_configured_git_and_refuses_paths_that_arent_text() {
        use std::os::unix::ffi::OsStrExt as _;
        let items = [item(1, TodoAction::Reword)];
        let file = |index| PathBuf::from(format!("/g/{index}.msg"));
        // The program comes quoted, as `rebase_interactive` gives it (#16).
        let todo = compile(&items, "'/opt/my git/bin/git'", file).unwrap();
        assert!(
            todo.contains("exec '/opt/my git/bin/git' commit --amend"),
            "{todo}"
        );
        // A message file whose path isn't UTF-8 would be named wrongly: refused.
        let odd = |_| PathBuf::from(std::ffi::OsStr::from_bytes(b"/g/caf\xe9/0.msg"));
        assert_eq!(compile(&items, "git", odd), Err(TodoProblem::PathNotUtf8));
        assert_eq!(
            shell_quote(Path::new("/a b/it's")).as_deref(),
            Some("'/a b/it'\\''s'")
        );
    }

    #[test]
    fn stops_come_after_their_group_and_edits_stop_at_the_commit() {
        use TodoAction::*;
        let items = [
            stop(item(0, Drop)),
            stop(item(1, Reword)),
            item(2, Fixup),
            stop(item(3, Drop)),
            item(4, Squash),
            item(5, Edit),
            stop(item(6, Drop)),
            item(7, Drop),
            stop(item(8, Pick)),
        ];
        let exec = "exec git commit --amend --only --allow-empty --quiet \
                    --file='/g/it'\\''s/1.msg'";
        let expected = [
            format!("drop {} m0", id(0)),
            "break".into(),
            format!("pick {} m1", id(1)),
            format!("fixup {} m2", id(2)),
            format!("drop {} m3", id(3)),
            format!("squash {} m4", id(4)),
            exec.into(),
            "break".into(),
            format!("edit {} m5", id(5)),
            format!("drop {} m6", id(6)),
            "break".into(),
            format!("drop {} m7", id(7)),
            format!("pick {} m8", id(8)),
            "break".into(),
        ];
        assert_eq!(
            compile(&items, "git", file).unwrap(),
            expected.join("\n") + "\n"
        );
        assert_eq!(check(&[item(1, Edit), item(2, Fixup)]), Ok(()));
        assert!(!Edit.folds());
    }

    #[test]
    fn autosquash_moves_fixups_and_squashes_under_their_targets() {
        use TodoAction::*;
        let items = [
            named(1, "a"),
            named(2, "b"),
            named(3, "fixup! a"),
            named(4, "squash! b"),
            named(5, "c"),
            named(6, "amend! a"),
        ];
        assert_eq!(
            shape(&autosquash(&items)),
            [
                (1, Pick),
                (3, Fixup),
                (6, Fixup),
                (2, Pick),
                (4, Squash),
                (5, Pick)
            ]
        );
    }

    #[test]
    fn autosquash_keeps_a_chain_in_order_and_counts_repeated_prefixes_once() {
        use TodoAction::*;
        let items = [
            named(1, "x"),
            named(2, "y"),
            named(3, "fixup! x"),
            named(4, "fixup! fixup! x"),
            named(5, "squash!  fixup! x"),
            named(6, "fixup! y"),
        ];
        assert_eq!(
            shape(&autosquash(&items)),
            [
                (1, Pick),
                (3, Fixup),
                (4, Fixup),
                (5, Squash),
                (2, Pick),
                (6, Fixup)
            ]
        );
        // The rest of the item is kept.
        let moved = autosquash(&[named(1, "x"), stop(named(2, "fixup! x"))]);
        assert_eq!(moved[1].message, "fixup! x\n\nbody");
        assert!(moved[1].stop_after);
    }

    #[test]
    fn autosquash_finds_targets_by_commit_id() {
        use TodoAction::*;
        let items = [
            named(0xab, "a"),
            named(0xcd, "b"),
            named(1, "fixup! abab"),
            named(2, "fixup! ABABAB"),
            // Too short to be an id.
            named(3, "fixup! aba"),
        ];
        assert_eq!(
            shape(&autosquash(&items)),
            [
                (0xab, Pick),
                (1, Fixup),
                (2, Fixup),
                (0xcd, Pick),
                (3, Pick)
            ]
        );
    }

    #[test]
    fn autosquash_finds_a_target_by_a_whole_sha256_id() {
        use TodoAction::*;
        let long = ObjectId::from_bytes_or_panic(&[0xab; 32]);
        assert_eq!(long.to_string().len(), 64);
        let items = [
            TodoItem {
                commit: long,
                ..named(0xab, "a")
            },
            named(0xcd, "b"),
            named(1, &format!("fixup! {long}")),
        ];
        assert_eq!(
            shape(&autosquash(&items)),
            [(0xab, Pick), (1, Fixup), (0xcd, Pick)]
        );
    }

    #[test]
    fn autosquash_leaves_items_without_an_earlier_target() {
        let items = [
            named(1, "fixup! b"),
            named(2, "b"),
            named(3, "fixup! nothing"),
            named(4, "fixup!b"),
            named(5, "not fixup! b"),
        ];
        assert_eq!(autosquash(&items), items);
    }

    #[test]
    fn gits_todo_is_read_in_every_style() {
        let todo = "pick 41a6cee # c2\np 8a815d2 c3\n\n# Rebase 1..2 onto 1\n";
        assert_eq!(
            parse_picks(todo),
            Some(vec!["41a6cee".into(), "8a815d2".into()])
        );
        assert_eq!(parse_picks("noop\n"), Some(Vec::new()));
        assert_eq!(parse_picks("label onto\npick 41a6cee x\n"), None);
        assert_eq!(
            parse_picks("pick 41a6cee x\nupdate-ref refs/heads/x\n"),
            None
        );
    }

    #[test]
    fn the_sequence_editor_replaces_only_the_planned_todo() {
        let dir = tempfile::tempdir().unwrap();
        let (plan, todo) = (dir.path().join("plan"), dir.path().join("todo"));
        let ours = "drop x\npick y\n";
        std::fs::write(&plan, plan_text(&[id(0xab), id(0xcd)], ours)).unwrap();

        let theirs = format!("pick {} # a\npick {} # b\n", "abab", "cdcdcd");
        std::fs::write(&todo, &theirs).unwrap();
        run_sequence_editor(&plan, &todo).unwrap();
        assert_eq!(std::fs::read_to_string(&todo).unwrap(), ours);

        // Reordered, missing, extra or too short: git's todo stays, and it's an error.
        for theirs in [
            "pick cdcd a\npick abab b\n",
            "pick abab a\n",
            "pick abab a\npick cdcd b\npick efef c\n",
            "pick ab a\npick cdcd b\n",
            "pick abab a\nexec make\npick cdcd b\n",
        ] {
            std::fs::write(&todo, theirs).unwrap();
            assert_eq!(run_sequence_editor(&plan, &todo), Err(MISMATCH.to_owned()));
            assert_eq!(std::fs::read_to_string(&todo).unwrap(), theirs);
        }
    }

    fn action() -> impl Strategy<Value = TodoAction> {
        prop_oneof![
            Just(TodoAction::Pick),
            Just(TodoAction::Reword),
            Just(TodoAction::Edit),
            Just(TodoAction::Squash),
            Just(TodoAction::Fixup),
            Just(TodoAction::Drop),
        ]
    }

    proptest! {
        /// Every item appears once, in order, with its verb; each reword gets one exec,
        /// right after its group (before the next kept commit, after its folds). Each
        /// stop gets a break after its item and before the next kept commit, never
        /// before a fold of the open group or the open reword's exec.
        #[test]
        fn compiled_todos_follow_the_items(
            specs in prop::collection::vec((action(), any::<bool>()), 1..10),
        ) {
            let items: Vec<TodoItem> = specs
                .iter()
                .enumerate()
                .map(|(n, &(action, stop_after))| TodoItem { stop_after, ..item(n as u8, action) })
                .collect();
            let Ok(todo) = compile(&items, "git", file) else {
                prop_assert!(check(&items).is_err());
                return Ok(());
            };
            let lines: Vec<&str> = todo.lines().collect();
            let is_command = |l: &&str| !l.starts_with("exec ") && *l != "break";
            // The line of each item.
            let positions: Vec<usize> = (0..lines.len()).filter(|&p| is_command(&lines[p])).collect();
            prop_assert_eq!(positions.len(), items.len());
            for (&p, item) in positions.iter().zip(&items) {
                let verb = match item.action {
                    TodoAction::Pick | TodoAction::Reword => "pick",
                    TodoAction::Edit => "edit",
                    TodoAction::Squash => "squash",
                    TodoAction::Fixup => "fixup",
                    TodoAction::Drop => "drop",
                };
                let expected = format!("{verb} {}", item.commit);
                prop_assert!(lines[p].starts_with(&expected));
            }
            // Walk the lines: an exec closes the reword opened last, and comes before
            // the next pick and any break.
            let mut open: Option<usize> = None;
            let mut index = 0;
            for (p, line) in lines.iter().enumerate() {
                if let Some(rest) = line.strip_prefix("exec ") {
                    let reword = open.take();
                    prop_assert!(reword.is_some());
                    let ending = format!("/{}.msg'", reword.unwrap());
                    prop_assert!(rest.ends_with(&ending));
                    continue;
                }
                if *line == "break" {
                    prop_assert!(open.is_none(), "a break before the exec of the last reword");
                    prop_assert!(p > 0 && lines[p - 1] != "break", "two breaks in a row");
                    let next = items[index..].iter().find(|i| i.action != TodoAction::Drop);
                    prop_assert!(!next.is_some_and(|i| i.action.folds()), "a break before a fold");
                    continue;
                }
                if line.starts_with("pick ") || line.starts_with("edit ") {
                    prop_assert!(open.is_none(), "a pick before the exec of the last reword");
                    if items[index].action == TodoAction::Reword {
                        open = Some(index);
                    }
                }
                index += 1;
            }
            prop_assert!(open.is_none());
            // Every stop has a break before the next kept commit's line.
            for (index, item) in items.iter().enumerate() {
                if !item.stop_after {
                    continue;
                }
                let kept = |i: &TodoItem| !i.action.folds() && i.action != TodoAction::Drop;
                let end = items[index + 1..]
                    .iter()
                    .position(kept)
                    .map_or(lines.len(), |k| positions[index + 1 + k]);
                prop_assert!(lines[positions[index]..end].contains(&"break"));
                // A drop at the end of a group stops right there.
                let next = items[index + 1..].iter().find(|i| i.action != TodoAction::Drop);
                if item.action == TodoAction::Drop && !next.is_some_and(|i| i.action.folds()) {
                    let after = &lines[positions[index] + 1..];
                    let exec_first = after.first().is_some_and(|l| l.starts_with("exec "));
                    prop_assert_eq!(after.get(usize::from(exec_first)), Some(&"break"));
                }
            }
            let rewords = items.iter().filter(|i| i.action == TodoAction::Reword).count();
            let breaks = lines.iter().filter(|l| **l == "break").count();
            prop_assert_eq!(lines.len() - positions.len(), rewords + breaks);
            prop_assert_eq!(breaks > 0, items.iter().any(|i| i.stop_after));
        }
    }
}
