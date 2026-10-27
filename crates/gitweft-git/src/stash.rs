//! Stashes: list, push, apply, pop, drop, branch and contents, through the git CLI.

use std::path::Path;

use gix::ObjectId;
use gix::bstr::BString;

use crate::cancel::CancelToken;
use crate::cli::{GitCli, GitError, GitErrorKind};
use crate::read::{FileChange, ReadError, changed_files};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashEntry {
    /// Position in the stash list: `stash@{index}`.
    pub index: usize,
    pub id: ObjectId,
    /// git's subject, e.g. `On main: half-done login form` or `WIP on main: 1a2b3c4 msg`.
    pub message: String,
    /// Seconds since the Unix epoch.
    pub time: i64,
}

impl StashEntry {
    pub fn revision(&self) -> String {
        format!("stash@{{{}}}", self.index)
    }
}

/// The stash list, newest first (`stash@{0}` first).
pub fn list_stashes(
    git: &GitCli,
    workdir: &Path,
    cancel: &CancelToken,
) -> Result<Vec<StashEntry>, GitError> {
    let output = git
        .command(workdir)
        .args(["stash", "list", "--format=%H%x00%ct%x00%gs"])
        .read_only()
        .run(cancel)?;
    Ok(output
        .stdout_str()
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let mut fields = line.splitn(3, '\0');
            let id = ObjectId::from_hex(fields.next()?.as_bytes()).ok()?;
            let time = fields.next()?.parse().ok()?;
            let message = fields.next().unwrap_or_default().to_owned();
            Some(StashEntry {
                index,
                id,
                message,
                time,
            })
        })
        .collect())
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StashOptions {
    pub message: Option<String>,
    /// Stash untracked files too (they are removed from the working tree).
    pub include_untracked: bool,
    /// Leave staged changes in the index (they are stashed as well).
    pub keep_index: bool,
    /// Stash only these paths (their staged and unstaged changes, and with
    /// `include_untracked` untracked ones); everything when empty. Taken literally, not
    /// as globs.
    pub paths: Vec<BString>,
}

/// Stashes local changes. `Ok(false)` when there was nothing to stash.
pub fn stash_push(
    git: &GitCli,
    workdir: &Path,
    options: &StashOptions,
    cancel: &CancelToken,
) -> Result<bool, GitError> {
    let mut command = git.command(workdir).args(["stash", "push"]);
    if let Some(message) = options.message.as_deref().filter(|m| !m.trim().is_empty()) {
        command = command.args(["--message", message]);
    }
    if options.include_untracked {
        command = command.arg("--include-untracked");
    }
    if options.keep_index {
        command = command.arg("--keep-index");
    }
    if !options.paths.is_empty() {
        // Paths go NUL-separated on stdin: no argv limits, non-UTF-8 names survive, and
        // a path starting with `-` can't be read as an option.
        let mut stdin = Vec::new();
        for path in &options.paths {
            stdin.extend_from_slice(path);
            stdin.push(0);
        }
        command = command
            .args(["--pathspec-from-file=-", "--pathspec-file-nul"])
            .env("GIT_LITERAL_PATHSPECS", "1")
            .stdin(stdin);
    }
    let output = command.run(cancel)?;
    Ok(!output.stdout_str().contains("No local changes to save"))
}

/// Applies a stash; with `pop`, drops it afterwards, unless applying it conflicted
/// ([`GitErrorKind::Conflict`]), in which case git keeps it. The changes come back
/// unstaged (new files staged). Restoring the index too (`--index`) fails outright on
/// conflicts instead of leaving markers to resolve, so it's not the default.
pub fn stash_apply(
    git: &GitCli,
    workdir: &Path,
    entry: &StashEntry,
    pop: bool,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    verify_position(git, workdir, entry, cancel)?;
    let verb = if pop { "pop" } else { "apply" };
    git.command(workdir)
        .args(["stash", verb])
        .arg(entry.revision())
        .run(cancel)?;
    Ok(())
}

/// Applies a stash commit that is no longer in the stash list (a dropped stash, found
/// among the dangling commits): `git stash apply <id>`, which refuses a commit that isn't
/// stash-like. The changes come back as with [`stash_apply`].
pub fn stash_apply_commit(
    git: &GitCli,
    workdir: &Path,
    id: ObjectId,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    git.command(workdir)
        .args(["stash", "apply"])
        .arg(id.to_string())
        .run(cancel)?;
    Ok(())
}

/// Deletes a stash. Its commit stays in the object database until garbage collection,
/// so `git stash apply <id>` can still bring it back.
pub fn stash_drop(
    git: &GitCli,
    workdir: &Path,
    entry: &StashEntry,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    verify_position(git, workdir, entry, cancel)?;
    git.command(workdir)
        .args(["stash", "drop"])
        .arg(entry.revision())
        .run(cancel)?;
    Ok(())
}

/// Creates branch `name` at the commit the stash was made on, checks it out, applies
/// the stash there with its index, and drops it (`git stash branch`). Refuses if the
/// stash list changed meanwhile.
///
/// git checks out the new branch first, so local changes that the checkout would
/// overwrite make it fail before anything is created; other local changes are carried
/// onto the branch, and if the stash then can't be applied on top of them the branch
/// stays checked out and the stash is kept.
pub fn stash_branch(
    git: &GitCli,
    workdir: &Path,
    entry: &StashEntry,
    name: &str,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    if name.starts_with('-') {
        // `git stash branch` has no `--end-of-options`; don't let a name become a flag.
        return Err(GitError::new(
            GitErrorKind::Other,
            "git stash branch",
            format!("fatal: '{name}' is not a valid branch name"),
        ));
    }
    verify_position(git, workdir, entry, cancel)?;
    git.command(workdir)
        .args(["stash", "branch", name])
        .arg(entry.revision())
        .run(cancel)?;
    Ok(())
}

/// What a stash holds, for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashContents {
    /// The commit it was made on (the stash's first parent).
    pub base: ObjectId,
    /// Tracked files changed from `base` to the stash (the working tree as stashed,
    /// staged parts included), each with its blob in the stash.
    pub tracked: Vec<FileChange>,
    /// The commit holding its untracked files (the third parent), when there is one.
    pub untracked_commit: Option<ObjectId>,
    /// Its untracked files, as additions.
    pub untracked: Vec<FileChange>,
}

/// Reads what a stash holds. Works on the stash's id, so it doesn't matter whether
/// the stash list moved.
pub fn stash_contents(
    git: &GitCli,
    workdir: &Path,
    entry: &StashEntry,
    cancel: &CancelToken,
) -> Result<StashContents, GitError> {
    // `--quiet` with exit code 1: the parent doesn't exist (no `^3` without `-u`).
    let parent = |n: u8| -> Result<Option<ObjectId>, GitError> {
        let output = git
            .command(workdir)
            .args(["rev-parse", "--verify", "--quiet"])
            .arg(format!("{}^{n}", entry.id))
            .allow_exit_codes(&[1])
            .read_only()
            .run(cancel)?;
        if output.status != 0 {
            return Ok(None);
        }
        let hex = output.stdout_str();
        ObjectId::from_hex(hex.trim().as_bytes())
            .map(Some)
            .map_err(|e| read_error(e.into()))
    };
    let base = parent(1)?.ok_or_else(|| {
        GitError::new(
            GitErrorKind::Other,
            "git rev-parse",
            format!("fatal: {} is not a stash: it has no parent", entry.id),
        )
    })?;
    let (tracked, _) = changed_files(git, workdir, &base, &entry.id, cancel).map_err(read_error)?;

    let untracked_commit = parent(3)?;
    let untracked = match untracked_commit {
        Some(commit) => {
            // The untracked files' commit has no parent: compare with the empty tree.
            let empty = commit.kind().empty_tree();
            changed_files(git, workdir, &empty, &commit, cancel)
                .map_err(read_error)?
                .0
        }
        None => Vec::new(),
    };
    Ok(StashContents {
        base,
        tracked,
        untracked_commit,
        untracked,
    })
}

/// The read helpers fail with a [`GitError`] (git failed) or a parse error.
fn read_error(err: ReadError) -> GitError {
    match err.downcast::<GitError>() {
        Ok(err) => *err,
        Err(err) => GitError::new(GitErrorKind::Other, "git stash", err.to_string()),
    }
}

/// `stash@{n}` shifts when stashes are added or dropped elsewhere (a terminal, another
/// tab): refuse to act on a position that no longer holds the stash the user picked.
fn verify_position(
    git: &GitCli,
    workdir: &Path,
    entry: &StashEntry,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    let output = git
        .command(workdir)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(entry.revision())
        .read_only()
        .run(cancel);
    let current = output
        .ok()
        .and_then(|out| ObjectId::from_hex(out.stdout_str().trim().as_bytes()).ok());
    if current == Some(entry.id) {
        Ok(())
    } else {
        Err(GitError::new(
            GitErrorKind::Other,
            "git stash",
            format!(
                "The stash list changed: {} is no longer the stash you selected. Reload and try again.",
                entry.revision()
            ),
        ))
    }
}
