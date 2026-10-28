//! Rewriting the current branch: cherry-pick, revert and reset.
//!
//! * [`cherry_pick`] and [`revert`] run git's sequencer. A conflict stops with the
//!   operation in progress ([`GitErrorKind::Conflict`]), to be continued, skipped or
//!   aborted like any other ([`crate::merge::run_operation_action`]). A commit whose change
//!   is already on the branch (it would come out empty) is skipped and counted.
//! * [`reset`] moves the checked-out branch. A hard reset first saves what it throws
//!   away: the local changes as a `git stash create` commit, and untracked files that
//!   the target's files would overwrite are moved to the trash. [`recorded_reset`] also
//!   returns the undo entry, which puts the saved changes (or the old index) back.

use std::path::{Path, PathBuf};

use gitweft_platform::bytes::{git_path, path_from_bytes};
use gix::ObjectId;

use crate::cancel::CancelToken;
use crate::cli::{GitCli, GitError, GitErrorKind};
use crate::trash::{self, TrashAction, TrashFile, TrashFileKind, TrashRecord};
use crate::undo::{self, RefChange, UndoEntry, Worktree};

/// What a cherry-pick or revert did besides making commits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PickOutcome {
    /// Commits skipped because their change is already on the branch.
    pub skipped: usize,
}

/// Applies `commits` (oldest first) on top of the checked-out branch. `mainline` picks the
/// parent a merge commit is compared with (`-m`, 1 for the first parent).
pub fn cherry_pick(
    git: &GitCli,
    workdir: &Path,
    commits: &[ObjectId],
    mainline: Option<u32>,
    cancel: &CancelToken,
) -> Result<PickOutcome, GitError> {
    sequence(
        git,
        workdir,
        Sequencer::CherryPick,
        commits,
        mainline,
        cancel,
    )
}

/// Applies the changes of `commits` (oldest first) to the index and the working tree
/// without committing (`git cherry-pick --no-commit`): they end up staged and the branch
/// doesn't move. `mainline` as for [`cherry_pick`]. A conflict stops it with
/// `GitErrorKind::Conflict`: for one commit that leaves only the conflicted files (no
/// operation to continue); for several, git keeps its sequencer to continue the rest.
pub fn cherry_pick_no_commit(
    git: &GitCli,
    workdir: &Path,
    commits: &[ObjectId],
    mainline: Option<u32>,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    if commits.is_empty() {
        return Ok(());
    }
    git.command(workdir)
        .args(["cherry-pick", "--no-commit"])
        .args(
            mainline
                .iter()
                .flat_map(|parent| ["-m".to_owned(), parent.to_string()]),
        )
        .args(commits.iter().map(ObjectId::to_string))
        .run(cancel)
        .map(drop)
}

/// Adds commits that undo `commits`, in the order given, with git's default messages
/// ("Revert …").
pub fn revert(
    git: &GitCli,
    workdir: &Path,
    commits: &[ObjectId],
    mainline: Option<u32>,
    cancel: &CancelToken,
) -> Result<PickOutcome, GitError> {
    sequence(git, workdir, Sequencer::Revert, commits, mainline, cancel)
}

/// Rebases the checked-out branch onto `onto` (`git rebase <onto>`). A conflict stops the
/// rebase for the operation banner (`GitErrorKind::Conflict`); a branch already on top of
/// `onto` is left as it is. With `rebase_merges` the branch's merges are recreated
/// (`--rebase-merges`); without, they are flattened away as git does by default.
pub fn rebase_onto(
    git: &GitCli,
    workdir: &Path,
    onto: impl AsRef<std::ffi::OsStr>,
    rebase_merges: bool,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    // `onto` as bytes: a branch's name needn't be UTF-8.
    git.command(workdir)
        .arg("rebase")
        .args(rebase_merges.then_some("--rebase-merges"))
        .arg("--end-of-options")
        .arg(onto)
        .run(cancel)
        .map(drop)
}

/// Commits what's staged as a fix for `target` (`git commit --fixup`): its message is
/// `fixup! <target's summary>`, for an autosquash rebase to fold it into `target`.
/// Nothing staged fails with [`GitErrorKind::NothingToCommit`].
pub fn commit_fixup(
    git: &GitCli,
    workdir: &Path,
    target: ObjectId,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    git.command(workdir)
        .arg("commit")
        .arg(format!("--fixup={target}"))
        .run(cancel)
        .map(drop)
}

#[derive(Debug, Clone, Copy)]
enum Sequencer {
    CherryPick,
    Revert,
}

impl Sequencer {
    fn verb(self) -> &'static str {
        match self {
            Sequencer::CherryPick => "cherry-pick",
            Sequencer::Revert => "revert",
        }
    }

    /// The pseudo-ref that exists while a step is stopped.
    fn head(self) -> &'static str {
        match self {
            Sequencer::CherryPick => "CHERRY_PICK_HEAD",
            Sequencer::Revert => "REVERT_HEAD",
        }
    }
}

fn sequence(
    git: &GitCli,
    workdir: &Path,
    kind: Sequencer,
    commits: &[ObjectId],
    mainline: Option<u32>,
    cancel: &CancelToken,
) -> Result<PickOutcome, GitError> {
    let mut outcome = PickOutcome::default();
    if commits.is_empty() {
        return Ok(outcome);
    }
    let mut command = git.command(workdir).arg(kind.verb());
    if let Sequencer::Revert = kind {
        command = command.arg("--no-edit");
    }
    if let Some(parent) = mainline {
        command = command.arg("-m").arg(parent.to_string());
    }
    let mut result = command
        .args(commits.iter().map(ObjectId::to_string))
        .run(cancel)
        .map(drop);
    // A step that comes out empty stops the sequence with "nothing to commit"; git's
    // advice is to skip it, and the rest of the sequence goes on from there.
    loop {
        match result {
            Ok(()) => return Ok(outcome),
            Err(err)
                if came_out_empty(&err)
                    && outcome.skipped < commits.len()
                    && step_stopped(git, workdir, kind, cancel)? =>
            {
                outcome.skipped += 1;
                result = git
                    .command(workdir)
                    .args([kind.verb(), "--skip"])
                    .run(cancel)
                    .map(drop);
            }
            Err(err) => return Err(err),
        }
    }
}

fn came_out_empty(err: &GitError) -> bool {
    err.kind == GitErrorKind::NothingToCommit || err.stderr.contains("is now empty")
}

fn step_stopped(
    git: &GitCli,
    workdir: &Path,
    kind: Sequencer,
    cancel: &CancelToken,
) -> Result<bool, GitError> {
    let output = git
        .command(workdir)
        .args(["rev-parse", "--quiet", "--verify", kind.head()])
        .allow_exit_codes(&[1])
        .read_only()
        .run(cancel)?;
    Ok(output.status == 0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetMode {
    /// Only the branch moves: the index and files keep their content, so the difference
    /// shows as staged.
    Soft,
    /// The index follows the branch; the files keep their content (unstaged).
    Mixed,
    /// The index and files follow the branch. What that throws away is saved first.
    Hard,
}

/// What a reset saved of what it replaced; all `None` for a soft reset.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResetBackup {
    /// Mixed: the index it replaced, as a tree (`git write-tree`).
    pub index: Option<ObjectId>,
    /// Hard: the staged and unstaged changes to tracked files, as a `git stash create`
    /// commit (on no ref, so it lasts until garbage collection prunes it). `None` when
    /// there were none.
    pub changes: Option<ObjectId>,
    /// Hard: where untracked files that the target's files would have overwritten went.
    pub trash_dir: Option<PathBuf>,
}

/// Moves the checked-out branch (or detached HEAD) to `target`. Mixed and hard resets
/// refuse while conflicts are unresolved ([`GitErrorKind::UnresolvedConflicts`]): the
/// index can't be saved then, and aborting the operation is the way out.
pub fn reset(
    git: &GitCli,
    workdir: &Path,
    target: &str,
    mode: ResetMode,
    trash_root: &Path,
    cancel: &CancelToken,
) -> Result<ResetBackup, GitError> {
    match mode {
        ResetMode::Soft => {
            run_reset(git, workdir, "--soft", target, cancel)?;
            Ok(ResetBackup::default())
        }
        ResetMode::Mixed => {
            let index = index_tree(git, workdir, cancel)?;
            run_reset(git, workdir, "--mixed", target, cancel)?;
            Ok(ResetBackup {
                index: Some(index),
                ..ResetBackup::default()
            })
        }
        ResetMode::Hard => hard_reset(git, workdir, target, trash_root, cancel),
    }
}

/// [`reset`], plus the entry that undoes it (`label` names it). `None` when there is
/// nothing to undo: the branch stayed, and nothing was staged or changed.
pub fn recorded_reset(
    git: &GitCli,
    workdir: &Path,
    target: &str,
    mode: ResetMode,
    trash_root: &Path,
    label: String,
    cancel: &CancelToken,
) -> Result<(ResetBackup, Option<UndoEntry>), GitError> {
    let names = [undo::checked_out_ref(git, workdir, cancel)?];
    let before = undo::ref_values(git, workdir, &names, cancel)?[0].1;
    let backup = reset(git, workdir, target, mode, trash_root, cancel)?;
    // The reset is done: even if cancelled now, the branch must be read.
    let done = CancelToken::new();
    let after = undo::ref_values(git, workdir, &names, &done)?[0].1;
    let worktree = match (mode, &backup) {
        (ResetMode::Soft, _) => Worktree::Soft,
        (
            _,
            ResetBackup {
                changes: Some(changes),
                ..
            },
        ) => Worktree::RestoreChanges(*changes),
        (
            _,
            ResetBackup {
                index: Some(tree), ..
            },
        ) => Worktree::RestoreIndex(*tree),
        _ => Worktree::Follow,
    };
    let restores = backup.trash_dir.is_some()
        || match worktree {
            Worktree::RestoreChanges(_) => true,
            // Unstaging everything (a mixed reset to HEAD) changed something only if the
            // index differed from HEAD's tree.
            Worktree::RestoreIndex(tree) => Some(tree) != head_tree(git, workdir, &done)?,
            _ => false,
        };
    // The branch stays in the entry even when it didn't move: undo checks that it's
    // still there before it restores the index or the files.
    let entry = (before != after || restores).then(|| UndoEntry {
        label,
        changes: vec![RefChange {
            name: names[0].clone(),
            before,
            after,
        }],
        worktree,
        branch_config: Vec::new(),
        trash: backup.trash_dir.clone(),
    });
    Ok((backup, entry))
}

/// A hard reset that saves the local changes and moves untracked files in the way to a
/// new folder under `trash_root` first.
pub(crate) fn hard_reset(
    git: &GitCli,
    workdir: &Path,
    target: &str,
    trash_root: &Path,
    cancel: &CancelToken,
) -> Result<ResetBackup, GitError> {
    refuse_conflicts(git, workdir, cancel)?;
    let output = git
        .command(workdir)
        .args(["stash", "create", "gitweft: saved before a hard reset"])
        .run(cancel)?;
    let text = output.stdout_str();
    let changes = match text.trim() {
        "" => None,
        id => Some(parse_id(id, "git stash create")?),
    };
    let trash_dir = trash_untracked_in_the_way(git, workdir, target, trash_root, cancel)?;
    run_reset(git, workdir, "--hard", target, cancel)?;
    Ok(ResetBackup {
        index: None,
        changes,
        trash_dir,
    })
}

/// `git reset -q <mode> <target> --` (the `--` keeps a target that is also a path a
/// revision).
pub(crate) fn run_reset(
    git: &GitCli,
    workdir: &Path,
    mode: &str,
    target: &str,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    git.command(workdir)
        .args(["reset", "-q", mode, target, "--"])
        .run(cancel)?;
    Ok(())
}

/// The index as a tree (written to the object database).
pub(crate) fn index_tree(
    git: &GitCli,
    workdir: &Path,
    cancel: &CancelToken,
) -> Result<ObjectId, GitError> {
    refuse_conflicts(git, workdir, cancel)?;
    let output = git.command(workdir).arg("write-tree").run(cancel)?;
    parse_id(output.stdout_str().trim(), "git write-tree")
}

fn head_tree(
    git: &GitCli,
    workdir: &Path,
    cancel: &CancelToken,
) -> Result<Option<ObjectId>, GitError> {
    let output = git
        .command(workdir)
        .args(["rev-parse", "--quiet", "--verify", "HEAD^{tree}"])
        .allow_exit_codes(&[1])
        .read_only()
        .run(cancel)?;
    Ok(ObjectId::from_hex(output.stdout_str().trim().as_bytes()).ok())
}

fn refuse_conflicts(git: &GitCli, workdir: &Path, cancel: &CancelToken) -> Result<(), GitError> {
    let output = git
        .command(workdir)
        .args(["ls-files", "--unmerged", "-z"])
        .read_only()
        .run(cancel)?;
    if output.stdout.is_empty() {
        return Ok(());
    }
    Err(GitError::new(
        GitErrorKind::UnresolvedConflicts,
        "git reset",
        "There are unresolved conflicts, so the index can't be saved. Abort the operation in \
         progress instead. Nothing was changed.",
    ))
}

/// Moves untracked (or ignored) files that `git reset --hard <target>` would overwrite
/// or delete to a new folder under `trash_root`: files at paths the target has but the
/// index doesn't, folders where the target has a file, and files where it needs a
/// folder. Returns that folder, or `None` when nothing was in the way.
fn trash_untracked_in_the_way(
    git: &GitCli,
    workdir: &Path,
    target: &str,
    trash_root: &Path,
    cancel: &CancelToken,
) -> Result<Option<PathBuf>, GitError> {
    // From the target's tree to the index, a path only the target has is "deleted".
    let output = git
        .command(workdir)
        .args([
            "diff",
            "--cached",
            "--name-only",
            "-z",
            "--no-renames",
            "--diff-filter=D",
            target,
            "--",
        ])
        .read_only()
        .run(cancel)?;
    let mut in_the_way: Vec<PathBuf> = output
        .stdout
        .split(|&byte| byte == 0)
        .filter(|raw| !raw.is_empty())
        .filter_map(|raw| blocking_path(workdir, &path_from_bytes(raw)))
        .collect();
    if in_the_way.is_empty() {
        return Ok(None);
    }
    in_the_way.sort();
    in_the_way.dedup();
    // A folder that goes to the trash takes everything in it along.
    let mut kept: Vec<PathBuf> = Vec::new();
    for path in in_the_way {
        if !kept.iter().any(|folder| path.starts_with(folder)) {
            kept.push(path);
        }
    }

    let io_error = |e: std::io::Error| GitError::new(GitErrorKind::Other, "trash", e.to_string());
    let time = trash::now_ms();
    let dir = trash::create_entry_dir(trash_root, TrashAction::Reset, time).map_err(io_error)?;
    let mut record = TrashRecord::new(workdir, TrashAction::Reset, time);
    let files = dir.join(trash::FILES_DIR);
    let mut moved = Ok(());
    for relative in &kept {
        if let Err(e) = trash::move_path(&workdir.join(relative), &files.join(relative)) {
            moved = Err(io_error(e));
            break;
        }
        record.files.push(TrashFile {
            path: git_path(relative).into(),
            kind: TrashFileKind::Untracked,
        });
    }
    // Whatever was moved is recorded, even when a later move failed.
    let written = record.write(&dir).map_err(io_error);
    moved.and(written).map(|()| Some(dir))
}

/// What stands where the target puts `relative`: anything at that path, or a
/// non-folder where the target needs a folder.
fn blocking_path(workdir: &Path, relative: &Path) -> Option<PathBuf> {
    let mut ancestors: Vec<&Path> = relative
        .ancestors()
        .filter(|path| !path.as_os_str().is_empty())
        .collect();
    ancestors.reverse();
    ancestors.into_iter().find_map(|path| {
        let meta = std::fs::symlink_metadata(workdir.join(path)).ok()?;
        (path == relative || !meta.is_dir()).then(|| path.to_owned())
    })
}

fn parse_id(text: &str, command: &str) -> Result<ObjectId, GitError> {
    ObjectId::from_hex(text.as_bytes())
        .map_err(|e| GitError::new(GitErrorKind::Other, command, e.to_string()))
}
