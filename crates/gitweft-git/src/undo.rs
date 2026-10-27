//! Undo and redo for local operations.
//!
//! An operation records the refs it moved, before and after ([`record`]). Undo puts
//! them back only if every one of them still points where the operation left it, so it
//! never discards work that happened since. The refs move in one atomic
//! `git update-ref --stdin` transaction, and each update there carries its expected old
//! value as well (git's compare-and-swap). [`undo`] returns the entry that reverses what
//! it just did, so redo is `undo` of that entry.
//!
//! The checked-out branch (or a detached HEAD) is special, because its files must
//! follow ([`Worktree`]). Undoing a commit uses `git reset --soft`: the undone commit's
//! changes come back staged ("uncommit"). Undoing a merge or pull uses `git reset --keep`,
//! which refuses to overwrite local changes instead of discarding them. Undoing a reset
//! puts back what it saved: the old index, or the local changes a hard reset threw away.
//!
//! A deleted (or created) branch's `branch.<name>.*` config (its upstream) travels with
//! the entry ([`BranchConfig`]): it's written back when the branch comes back and removed
//! when it goes. Untracked files a hard reset moved to the trash are named by the entry
//! ([`UndoEntry::trash`]); the caller restores them after the undo (`gitweft_core`).
//!
//! Not covered: renames and checkouts move the symbolic HEAD (and branch config), which
//! a compare-and-swap on ref values can't express; pushes and fetches change other
//! repositories and are never undoable.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use gix::ObjectId;

use crate::cancel::CancelToken;
use crate::cli::{GitCli, GitError, GitErrorKind};
use crate::rewrite;

/// Stands for "whatever is checked out" in the ref names given to [`record`]: the
/// branch HEAD points at, or HEAD itself when detached.
pub const CHECKED_OUT: &str = "HEAD";

/// How one ref moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefChange {
    /// Full name, e.g. `refs/heads/main`; `HEAD` for a detached HEAD.
    pub name: String,
    /// `None`: the ref didn't exist before.
    pub before: Option<ObjectId>,
    /// `None`: the operation deleted it.
    pub after: Option<ObjectId>,
}

/// How [`undo`] moves the index and files along with the checked-out branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Worktree {
    /// They stay as they are (`reset --soft`): an undone commit's changes come back staged.
    Soft,
    /// They follow the branch, unless that would overwrite local changes (`reset --keep`).
    Follow,
    /// The index follows the branch and the files stay (`reset --mixed`); the index it
    /// replaces is saved for the way back. Redo of a mixed reset.
    Mixed,
    /// A hard reset that saves the local changes first (see
    /// [`rewrite::reset`]). Redo of a hard reset that threw changes away.
    Discard,
    /// The branch moves with `reset --soft`, then the index becomes this tree again.
    /// Refused while something is staged. Undo of a mixed reset.
    RestoreIndex(ObjectId),
    /// The files follow the branch (`reset --keep`), then these saved changes (a
    /// `git stash create` commit) are applied again, staged parts staged. Refused while
    /// there are local changes. Undo of a hard reset.
    RestoreChanges(ObjectId),
}

/// A local branch's `branch.<name>.*` config (upstream, merge and rebase settings), kept
/// by an entry that creates or deletes the branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchConfig {
    /// Full name, e.g. `refs/heads/topic`: the entry's change of this ref says whether
    /// the branch exists after an undo, and so whether the config is written or removed.
    pub branch: String,
    /// Full keys (`branch.topic.remote`) and values, in the order git lists them.
    pub entries: Vec<(String, String)>,
}

/// One undoable operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoEntry {
    /// What the operation was, for menus and notifications (e.g. `commit “Fix typo”`).
    pub label: String,
    pub changes: Vec<RefChange>,
    pub worktree: Worktree,
    pub branch_config: Vec<BranchConfig>,
    /// The trash entry (a folder under the trash root) holding untracked files the undone
    /// hard reset moved out of its way; they go back once the undo is done.
    pub trash: Option<PathBuf>,
}

impl UndoEntry {
    /// `None` when the operation moved nothing (e.g. a merge that was already up to date).
    pub fn new(label: String, changes: Vec<RefChange>, worktree: Worktree) -> Option<Self> {
        (!changes.is_empty()).then_some(Self {
            label,
            changes,
            worktree,
            branch_config: Vec::new(),
            trash: None,
        })
    }

    /// The same refs moving the other way, with `worktree` for the files. The branch
    /// config goes along; a trash entry doesn't (it's restored by the undo).
    fn reversed(&self, worktree: Worktree) -> UndoEntry {
        UndoEntry {
            label: self.label.clone(),
            changes: self
                .changes
                .iter()
                .map(|change| RefChange {
                    name: change.name.clone(),
                    before: change.after,
                    after: change.before,
                })
                .collect(),
            worktree,
            branch_config: self.branch_config.clone(),
            trash: None,
        }
    }
}

/// A ref's full name as the undo log keeps it: itself when it's UTF-8, else encoded as a
/// config value that isn't (a NUL, then its bytes in hex), so the log stays text and git
/// gets the name byte for byte.
pub fn ref_name_text(bytes: &[u8]) -> String {
    crate::branch::config_value_text(bytes)
}

/// The bytes of a name [`ref_name_text`] gave.
pub fn ref_name_bytes(text: &str) -> Vec<u8> {
    crate::branch::config_value_bytes(text)
}

/// The full name of the checked-out branch (also when it has no commits yet), or
/// `HEAD` when HEAD is detached.
pub fn checked_out_ref(
    git: &GitCli,
    workdir: &Path,
    cancel: &CancelToken,
) -> Result<String, GitError> {
    let output = git
        .command(workdir)
        .args(["symbolic-ref", "--quiet", "HEAD"])
        .allow_exit_codes(&[1])
        .read_only()
        .run(cancel)?;
    let listed = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
    let name = ref_name_text(listed);
    Ok(if name.is_empty() {
        CHECKED_OUT.to_owned()
    } else {
        name
    })
}

/// Where each of `names` points now; `None` for refs that don't exist.
pub fn ref_values(
    git: &GitCli,
    workdir: &Path,
    names: &[String],
    cancel: &CancelToken,
) -> Result<Vec<(String, Option<ObjectId>)>, GitError> {
    names
        .iter()
        .map(|name| {
            // `--verify` takes exactly one name, and `--end-of-options` keeps a name
            // from being read as an option.
            let output = git
                .command(workdir)
                .args(["rev-parse", "--quiet", "--verify", "--end-of-options"])
                .arg(
                    <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(&ref_name_bytes(
                        name,
                    )),
                )
                .allow_exit_codes(&[1])
                .read_only()
                .run(cancel)?;
            let id = ObjectId::from_hex(output.stdout_str().trim().as_bytes()).ok();
            Ok((name.clone(), id))
        })
        .collect()
}

/// Runs `op` and returns what it moved among `names` (full ref names, or
/// [`CHECKED_OUT`]). Nothing is recorded when `op` fails.
pub fn record<T>(
    git: &GitCli,
    workdir: &Path,
    names: &[&str],
    cancel: &CancelToken,
    op: impl FnOnce() -> Result<T, GitError>,
) -> Result<(T, Vec<RefChange>), GitError> {
    let mut resolved = Vec::with_capacity(names.len());
    for name in names {
        let name = if *name == CHECKED_OUT {
            checked_out_ref(git, workdir, cancel)?
        } else {
            (*name).to_owned()
        };
        if !resolved.contains(&name) {
            resolved.push(name);
        }
    }
    let before = ref_values(git, workdir, &resolved, cancel)?;
    let value = op()?;
    // The operation is done: even if cancelled now, the refs must be read.
    let after = ref_values(git, workdir, &resolved, &CancelToken::new())?;
    Ok((value, ref_changes(&before, &after)))
}

/// [`record`] for one local branch (`refs/heads/…`) that the operation creates or deletes:
/// also returns its `branch.<name>.*` config, as it was before the operation (a delete
/// removes it) or else as the operation left it (a branch created with an upstream).
/// `None` when it has none.
pub fn record_branch<T>(
    git: &GitCli,
    workdir: &Path,
    branch: &str,
    cancel: &CancelToken,
    op: impl FnOnce() -> Result<T, GitError>,
) -> Result<(T, Vec<RefChange>, Option<BranchConfig>), GitError> {
    // `branch` may be encoded (`ref_name_text`); only a name that's text has config kept,
    // since its keys are read as text.
    let full = ref_name_bytes(branch);
    let short = std::str::from_utf8(full.strip_prefix(b"refs/heads/").unwrap_or(&full))
        .ok()
        .map(str::to_owned);
    let config = |cancel: &CancelToken| match &short {
        Some(short) => crate::branch::branch_config(git, workdir, short, cancel),
        None => Ok(Vec::new()),
    };
    let before = config(cancel)?;
    let (value, changes) = record(git, workdir, &[branch], cancel, op)?;
    let entries = if before.is_empty() {
        config(&CancelToken::new())?
    } else {
        before
    };
    let config = (!entries.is_empty()).then(|| BranchConfig {
        branch: branch.to_owned(),
        entries,
    });
    Ok((value, changes, config))
}

/// The refs that differ between two readings of the same names.
pub fn ref_changes(
    before: &[(String, Option<ObjectId>)],
    after: &[(String, Option<ObjectId>)],
) -> Vec<RefChange> {
    before
        .iter()
        .filter_map(|(name, old)| {
            let new = after
                .iter()
                .find(|(other, _)| other == name)
                .and_then(|(_, id)| *id);
            (*old != new).then(|| RefChange {
                name: name.clone(),
                before: *old,
                after: new,
            })
        })
        .collect()
}

/// Moves every ref of `entry` back to its `before`, provided each is still at its
/// `after`; otherwise nothing changes and the error is [`GitErrorKind::RefMoved`].
/// Returns the entry that reverses this (redo). A hard reset on the way (redoing one)
/// moves untracked files in its way to a new folder under `trash_root`.
pub fn undo(
    git: &GitCli,
    workdir: &Path,
    entry: &UndoEntry,
    trash_root: &Path,
    cancel: &CancelToken,
) -> Result<UndoEntry, GitError> {
    let checked_out = checked_out_ref(git, workdir, cancel)?;
    let names: Vec<String> = entry.changes.iter().map(|c| c.name.clone()).collect();
    let now = ref_values(git, workdir, &names, cancel)?;
    for change in &entry.changes {
        let current = now
            .iter()
            .find(|(name, _)| *name == change.name)
            .and_then(|(_, id)| *id);
        // A recorded detached HEAD must still be detached: otherwise HEAD now names a
        // branch, and moving "HEAD" would move that branch.
        let head_attached = change.name == CHECKED_OUT && checked_out != CHECKED_OUT;
        if current != change.after || head_attached {
            return Err(refused(
                entry,
                GitErrorKind::RefMoved,
                format!(
                    "{} has moved since, so this could lose work. Nothing was changed.",
                    short_name(&change.name)
                ),
            ));
        }
    }

    let head_change = entry
        .changes
        .iter()
        .find(|change| change.name == checked_out);
    if let Some(change) = head_change
        && change.before.is_none()
    {
        return Err(refused(
            entry,
            GitErrorKind::Other,
            format!(
                "that would delete {}, which is checked out.",
                short_name(&change.name)
            ),
        ));
    }
    // Restoring the index or files needs them as the operation left them.
    match entry.worktree {
        Worktree::RestoreIndex(_) if has_staged_changes(git, workdir, cancel)? => {
            return Err(refused(
                entry,
                GitErrorKind::WouldOverwrite,
                "changes were staged since. Commit or unstage them first. Nothing was changed."
                    .to_owned(),
            ));
        }
        Worktree::RestoreChanges(_) if has_local_changes(git, workdir, cancel)? => {
            return Err(refused(
                entry,
                GitErrorKind::WouldOverwrite,
                "there are local changes. Commit or stash them first. Nothing was changed."
                    .to_owned(),
            ));
        }
        _ => {}
    }

    // The other refs first: one atomic transaction, each update checked against its
    // expected old value again.
    let transaction = transaction(&entry.changes, &checked_out);
    if !transaction.is_empty() {
        git.command(workdir)
            .args(["update-ref", "--stdin", "-m"])
            .arg(format!("gitweft: undo {}", entry.label))
            .stdin(transaction)
            .run(cancel)?;
    }
    // Without the checked-out branch in the entry, Soft and Follow leave the files alone;
    // the other modes work on the index and files at HEAD.
    let target = head_change
        .and_then(|change| change.before)
        .map_or_else(|| CHECKED_OUT.to_owned(), |id| id.to_string());
    let moves_head = head_change.is_some();
    let mut moved_to_trash = None;
    let reverse = match entry.worktree {
        Worktree::Soft => {
            if moves_head {
                rewrite::run_reset(git, workdir, "--soft", &target, cancel)?;
            }
            Worktree::Soft
        }
        Worktree::Follow => {
            if moves_head {
                rewrite::run_reset(git, workdir, "--keep", &target, cancel)?;
            }
            // Undoing a hard reset that only moved untracked files: they come back from
            // the trash, so its redo must move them out of the way again.
            if entry.trash.is_some() {
                Worktree::Discard
            } else {
                Worktree::Follow
            }
        }
        Worktree::Mixed => {
            let index = rewrite::index_tree(git, workdir, cancel)?;
            rewrite::run_reset(git, workdir, "--mixed", &target, cancel)?;
            Worktree::RestoreIndex(index)
        }
        Worktree::Discard => {
            let backup = rewrite::hard_reset(git, workdir, &target, trash_root, cancel)?;
            moved_to_trash = backup.trash_dir;
            backup
                .changes
                .map_or(Worktree::Follow, Worktree::RestoreChanges)
        }
        Worktree::RestoreIndex(tree) => {
            rewrite::run_reset(git, workdir, "--soft", &target, cancel)?;
            git.command(workdir)
                .arg("read-tree")
                .arg(tree.to_string())
                .run(cancel)?;
            Worktree::Mixed
        }
        Worktree::RestoreChanges(changes) => {
            rewrite::run_reset(git, workdir, "--keep", &target, cancel)?;
            git.command(workdir)
                .args(["stash", "apply", "--index", "--quiet"])
                .arg(changes.to_string())
                .run(cancel)
                .map_err(|mut err| {
                    let _ = write!(
                        err.stderr,
                        "\nThe saved changes are in commit {changes}: \
                         `git stash apply --index {changes}` applies them."
                    );
                    err
                })?;
            Worktree::Discard
        }
    };
    // The refs have moved, so a config write that fails can't fail the undo any more:
    // the branch is back (or gone) either way, only without (or with) its upstream.
    for config in &entry.branch_config {
        let Some(change) = entry.changes.iter().find(|c| c.name == config.branch) else {
            continue;
        };
        let short = short_name(&config.branch);
        let entries = if change.before.is_some() {
            &config.entries[..]
        } else {
            &[]
        };
        let _ = crate::branch::set_branch_config(git, workdir, &short, entries, cancel);
    }
    let mut reversed = entry.reversed(reverse);
    reversed.trash = moved_to_trash;
    Ok(reversed)
}

fn has_staged_changes(
    git: &GitCli,
    workdir: &Path,
    cancel: &CancelToken,
) -> Result<bool, GitError> {
    let output = git
        .command(workdir)
        .args(["diff", "--cached", "--quiet"])
        .allow_exit_codes(&[1])
        .read_only()
        .run(cancel)?;
    Ok(output.status == 1)
}

/// Staged or unstaged changes to tracked files (untracked files don't count).
fn has_local_changes(git: &GitCli, workdir: &Path, cancel: &CancelToken) -> Result<bool, GitError> {
    let output = git
        .command(workdir)
        .args(["status", "--porcelain", "-z", "--untracked-files=no"])
        .read_only()
        .run(cancel)?;
    Ok(!output.stdout.is_empty())
}

/// `update-ref --stdin` lines that move every ref except `skip` back to `before`,
/// each guarded by its `after` value. Bytes: a ref's name needn't be UTF-8.
fn transaction(changes: &[RefChange], skip: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for change in changes.iter().filter(|change| change.name != skip) {
        let (verb, ids) = match (change.before, change.after) {
            (Some(before), Some(after)) if before == after => continue,
            (Some(before), Some(after)) => ("update", format!("{before} {after}")),
            (None, Some(after)) => ("delete", after.to_string()),
            (Some(before), None) => ("create", before.to_string()),
            (None, None) => continue,
        };
        out.extend_from_slice(verb.as_bytes());
        out.push(b' ');
        out.extend_from_slice(&ref_name_bytes(&change.name));
        out.push(b' ');
        out.extend_from_slice(ids.as_bytes());
        out.push(b'\n');
    }
    out
}

/// A ref's name without `refs/heads/` (tags, remotes), as text for messages.
fn short_name(name: &str) -> String {
    let bytes = ref_name_bytes(name);
    let text = String::from_utf8_lossy(&bytes);
    ["refs/heads/", "refs/tags/", "refs/remotes/"]
        .iter()
        .find_map(|prefix| text.strip_prefix(prefix))
        .unwrap_or(&text)
        .to_owned()
}

fn refused(entry: &UndoEntry, kind: GitErrorKind, reason: String) -> GitError {
    GitError::new(
        kind,
        format!("undo {}", entry.label),
        format!("Can't undo {}: {reason}", entry.label),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(byte: u8) -> ObjectId {
        ObjectId::from_bytes_or_panic(&[byte; 20])
    }

    #[test]
    fn changes_between_readings() {
        let before = vec![
            ("refs/heads/main".to_owned(), Some(id(1))),
            ("refs/heads/gone".to_owned(), Some(id(2))),
            ("refs/heads/new".to_owned(), None),
            ("refs/heads/same".to_owned(), Some(id(3))),
        ];
        let after = vec![
            ("refs/heads/main".to_owned(), Some(id(4))),
            ("refs/heads/gone".to_owned(), None),
            ("refs/heads/new".to_owned(), Some(id(5))),
            ("refs/heads/same".to_owned(), Some(id(3))),
        ];
        let changes = ref_changes(&before, &after);
        assert_eq!(changes.len(), 3);
        assert_eq!(changes[1].name, "refs/heads/gone");
        assert_eq!((changes[1].before, changes[1].after), (Some(id(2)), None));
        let reversed = UndoEntry::new("x".into(), changes, Worktree::Follow)
            .unwrap()
            .reversed(Worktree::Soft);
        assert_eq!(reversed.changes[0].before, Some(id(4)));
        assert_eq!(reversed.changes[0].after, Some(id(1)));
        assert_eq!(reversed.worktree, Worktree::Soft);
        assert!(UndoEntry::new("x".into(), Vec::new(), Worktree::Soft).is_none());
    }

    #[test]
    fn transactions_guard_every_update_and_skip_the_checked_out_branch() {
        let changes = vec![
            RefChange {
                name: "refs/heads/main".into(),
                before: Some(id(1)),
                after: Some(id(2)),
            },
            RefChange {
                name: "refs/heads/created".into(),
                before: None,
                after: Some(id(3)),
            },
            RefChange {
                name: "refs/tags/deleted".into(),
                before: Some(id(4)),
                after: None,
            },
            RefChange {
                name: "refs/heads/unmoved".into(),
                before: Some(id(5)),
                after: Some(id(5)),
            },
        ];
        assert_eq!(
            transaction(&changes, "refs/heads/main"),
            format!(
                "delete refs/heads/created {}\ncreate refs/tags/deleted {}\n",
                id(3),
                id(4)
            )
            .into_bytes()
        );
        assert!(
            transaction(&changes, CHECKED_OUT)
                .starts_with(format!("update refs/heads/main {} {}\n", id(1), id(2)).as_bytes())
        );

        // A name that isn't UTF-8, as the log keeps it, goes to git as its bytes.
        let latin1 = [RefChange {
            name: ref_name_text(b"refs/heads/caf\xe9"),
            before: None,
            after: Some(id(6)),
        }];
        let mut expected = b"delete refs/heads/caf\xe9 ".to_vec();
        expected.extend_from_slice(format!("{}\n", id(6)).as_bytes());
        assert_eq!(transaction(&latin1, CHECKED_OUT), expected);
    }
}
