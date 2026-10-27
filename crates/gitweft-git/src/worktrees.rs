//! Linked worktrees: list, add, remove, prune, lock and unlock, through the git CLI.
//!
//! (Not to be confused with [`crate::read::worktree`], which reads the files of a
//! working tree.) Paths are kept as `PathBuf` and read from `-z` output as bytes, so
//! non-UTF-8 names survive; relative paths are resolved against `workdir`, like git does.
//! Every path goes after `--`, so a folder named `-x` can't be read as an option.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use gix::ObjectId;
use gix::bstr::BString;

use crate::cancel::CancelToken;
use crate::cli::{GitCli, GitError, GitErrorKind};

/// A worktree of the repository, as `git worktree list --porcelain -z` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    /// `None` for a bare main worktree (or an unborn branch).
    pub head: Option<ObjectId>,
    /// Full ref name of its branch (`refs/heads/x`), as git names it (it needn't be UTF-8,
    /// #63); `None` when detached.
    pub branch: Option<BString>,
    pub bare: bool,
    /// The main worktree (the first one listed).
    pub is_main: bool,
    /// Locked, with its reason (possibly empty).
    pub locked: Option<String>,
    /// Its folder is gone; `git worktree prune` would remove it. The reason git gives.
    pub prunable: Option<String>,
}

/// The repository's worktrees, the main one first.
pub fn list_worktrees(
    git: &GitCli,
    workdir: &Path,
    cancel: &CancelToken,
) -> Result<Vec<WorktreeInfo>, GitError> {
    let output = git
        .command(workdir)
        .args(["worktree", "list", "--porcelain", "-z"])
        .read_only()
        .run(cancel)?;
    Ok(parse_worktree_list(&output.stdout))
}

/// Parses `git worktree list --porcelain -z`: one NUL-terminated field per attribute
/// (`worktree <path>`, `HEAD <id>`, `branch <ref>`, `bare`, `detached`,
/// `locked[ <reason>]`, `prunable[ <reason>]`), records ended by an empty field.
fn parse_worktree_list(bytes: &[u8]) -> Vec<WorktreeInfo> {
    let text = |value: &[u8]| String::from_utf8_lossy(value).into_owned();
    let mut worktrees = Vec::new();
    let mut current: Option<WorktreeInfo> = None;
    for field in bytes.split(|&b| b == 0) {
        if field.is_empty() {
            worktrees.extend(current.take());
            continue;
        }
        let (key, value) = match field.iter().position(|&b| b == b' ') {
            Some(space) => (&field[..space], Some(&field[space + 1..])),
            None => (field, None),
        };
        if key == b"worktree" {
            worktrees.extend(current.take());
            current = Some(WorktreeInfo {
                path: PathBuf::from(OsStr::from_bytes(value.unwrap_or_default())),
                head: None,
                branch: None,
                bare: false,
                is_main: worktrees.is_empty(),
                locked: None,
                prunable: None,
            });
            continue;
        }
        let Some(worktree) = current.as_mut() else {
            continue;
        };
        match key {
            // An unborn branch reports the null id.
            b"HEAD" => {
                worktree.head = value
                    .and_then(|hex| ObjectId::from_hex(hex).ok())
                    .filter(|id| !id.is_null());
            }
            b"branch" => worktree.branch = value.map(BString::from),
            b"bare" => worktree.bare = true,
            b"locked" => worktree.locked = Some(value.map(text).unwrap_or_default()),
            b"prunable" => worktree.prunable = Some(value.map(text).unwrap_or_default()),
            // `detached`, and anything a newer git adds.
            _ => {}
        }
    }
    worktrees.extend(current);
    worktrees
}

/// What a new worktree checks out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeTarget {
    /// A new branch `name`, starting at `start` (a revision; `HEAD` when `None`).
    NewBranch { name: String, start: Option<String> },
    /// An existing local branch (short name). Git refuses if it's checked out elsewhere.
    Branch(String),
    /// A detached HEAD at a revision.
    Detached(String),
}

/// Creates a worktree at `path` (which must not exist, or be an empty folder).
pub fn add_worktree(
    git: &GitCli,
    workdir: &Path,
    path: &Path,
    target: &WorktreeTarget,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    let mut command = git.command(workdir).args(["worktree", "add"]);
    command = match target {
        WorktreeTarget::NewBranch { name, start } => {
            refuse_option_like(name, "branch name")?;
            if let Some(start) = start {
                refuse_option_like(start, "revision")?;
            }
            command
                .args(["-b", name])
                .arg("--")
                .arg(path)
                .args(start.as_deref())
        }
        WorktreeTarget::Branch(name) => {
            refuse_option_like(name, "branch name")?;
            command.arg("--").arg(path).arg(name)
        }
        WorktreeTarget::Detached(revision) => {
            refuse_option_like(revision, "revision")?;
            command.args(["--detach", "--"]).arg(path).arg(revision)
        }
    };
    command.run(cancel)?;
    Ok(())
}

/// Removes a linked worktree: its folder and its administrative files. Its branch stays.
/// `force` also removes a worktree with local changes (or untracked files); it doesn't
/// override a lock.
pub fn remove_worktree(
    git: &GitCli,
    workdir: &Path,
    path: &Path,
    force: bool,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    // git's cleanliness check is a `git status` that honours `status.showUntrackedFiles`,
    // so with it set to `no`, untracked files would be deleted without `--force`.
    let mut command = git.command(workdir).args([
        "-c",
        "status.showUntrackedFiles=normal",
        "worktree",
        "remove",
    ]);
    if force {
        command = command.arg("--force");
    }
    command.arg("--").arg(path).run(cancel)?;
    Ok(())
}

/// Forgets worktrees whose folders are gone (the `prunable` ones; locked ones stay).
pub fn prune_worktrees(git: &GitCli, workdir: &Path, cancel: &CancelToken) -> Result<(), GitError> {
    git.command(workdir)
        .args(["worktree", "prune"])
        .run(cancel)?;
    Ok(())
}

/// Locks a worktree, so it's neither pruned nor removed (e.g. on a removable drive).
/// Git refuses one that is already locked.
pub fn lock_worktree(
    git: &GitCli,
    workdir: &Path,
    path: &Path,
    reason: Option<&str>,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    let mut command = git.command(workdir).args(["worktree", "lock"]);
    if let Some(reason) = reason.filter(|r| !r.is_empty()) {
        // One argument, so a reason starting with `-` stays a reason.
        command = command.arg(format!("--reason={reason}"));
    }
    command.arg("--").arg(path).run(cancel)?;
    Ok(())
}

/// Unlocks a worktree. Git refuses one that isn't locked.
pub fn unlock_worktree(
    git: &GitCli,
    workdir: &Path,
    path: &Path,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    git.command(workdir)
        .args(["worktree", "unlock", "--"])
        .arg(path)
        .run(cancel)?;
    Ok(())
}

/// Moves a linked worktree to the folder `to` (which must not exist yet); git updates its
/// records. Git refuses the main worktree and a locked one.
pub fn move_worktree(
    git: &GitCli,
    workdir: &Path,
    from: &Path,
    to: &Path,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    git.command(workdir)
        .args(["worktree", "move", "--"])
        .arg(from)
        .arg(to)
        .run(cancel)?;
    Ok(())
}

/// Whether the worktree at `path` has local changes, untracked files included (what
/// `remove` without force refuses). Submodule changes count too; ignored files don't.
pub fn worktree_has_changes(
    git: &GitCli,
    path: &Path,
    cancel: &CancelToken,
) -> Result<bool, GitError> {
    let output = git
        .command(path)
        .args([
            "status",
            "--porcelain",
            "--untracked-files=normal",
            "--ignore-submodules=none",
        ])
        .read_only()
        .run(cancel)?;
    Ok(!output.stdout.is_empty())
}

/// `git worktree add` reads a branch or revision starting with `-` as an option (or, after
/// `--`, fails confusingly): refuse it up front.
fn refuse_option_like(name: &str, what: &str) -> Result<(), GitError> {
    if name.starts_with('-') {
        return Err(GitError::new(
            GitErrorKind::Other,
            "git worktree add",
            format!("fatal: '{name}' is not a valid {what}"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "1c272101c5c26442cf2d04b6fbc29030cf072307";

    fn id() -> Option<ObjectId> {
        Some(ObjectId::from_hex(ID.as_bytes()).unwrap())
    }

    fn worktree(path: &[u8]) -> WorktreeInfo {
        WorktreeInfo {
            path: PathBuf::from(OsStr::from_bytes(path)),
            head: id(),
            branch: None,
            bare: false,
            is_main: false,
            locked: None,
            prunable: None,
        }
    }

    #[test]
    fn parses_every_kind_of_record() {
        let mut bytes = Vec::new();
        for record in [
            vec![
                "worktree /r".to_owned(),
                format!("HEAD {ID}"),
                "branch refs/heads/main".into(),
            ],
            vec![
                "worktree /w/sp ace/a\nb".into(),
                format!("HEAD {ID}"),
                "detached".into(),
            ],
            vec![
                "worktree /w/locked".into(),
                format!("HEAD {ID}"),
                "branch refs/heads/feat".into(),
                "locked on usb".into(),
            ],
            vec![
                "worktree /w/plain-lock".into(),
                format!("HEAD {ID}"),
                "detached".into(),
                "locked".into(),
            ],
            vec![
                "worktree /w/gone".into(),
                format!("HEAD {ID}"),
                "detached".into(),
                "prunable gitdir file points to non-existent location".into(),
            ],
            vec![
                "worktree /w/bare-prunable".into(),
                format!("HEAD {ID}"),
                "detached".into(),
                "prunable".into(),
            ],
            vec![
                "worktree /w/unborn".into(),
                format!("HEAD {}", "0".repeat(40)),
                "branch refs/heads/new".into(),
            ],
        ] {
            for field in record {
                bytes.extend_from_slice(field.as_bytes());
                bytes.push(0);
            }
            bytes.push(0);
        }
        // A non-UTF-8 folder name.
        bytes.extend_from_slice(b"worktree /w/caf\xe9\0");
        bytes.extend_from_slice(format!("HEAD {ID}\0detached\0\0").as_bytes());

        let expected = vec![
            WorktreeInfo {
                is_main: true,
                branch: Some("refs/heads/main".into()),
                ..worktree(b"/r")
            },
            worktree(b"/w/sp ace/a\nb"),
            WorktreeInfo {
                branch: Some("refs/heads/feat".into()),
                locked: Some("on usb".into()),
                ..worktree(b"/w/locked")
            },
            WorktreeInfo {
                locked: Some(String::new()),
                ..worktree(b"/w/plain-lock")
            },
            WorktreeInfo {
                prunable: Some("gitdir file points to non-existent location".into()),
                ..worktree(b"/w/gone")
            },
            WorktreeInfo {
                prunable: Some(String::new()),
                ..worktree(b"/w/bare-prunable")
            },
            WorktreeInfo {
                head: None,
                branch: Some("refs/heads/new".into()),
                ..worktree(b"/w/unborn")
            },
            worktree(b"/w/caf\xe9"),
        ];
        assert_eq!(parse_worktree_list(&bytes), expected);
    }

    #[test]
    fn a_bare_main_worktree_has_no_head() {
        let parsed = parse_worktree_list(b"worktree /repo.git\0bare\0\0");
        assert_eq!(
            parsed,
            [WorktreeInfo {
                head: None,
                bare: true,
                is_main: true,
                ..worktree(b"/repo.git")
            }]
        );
        assert!(parse_worktree_list(b"").is_empty());
    }

    #[test]
    fn a_missing_final_terminator_still_ends_the_record() {
        let parsed = parse_worktree_list(format!("worktree /r\0HEAD {ID}\0detached").as_bytes());
        assert_eq!(
            parsed,
            [WorktreeInfo {
                is_main: true,
                ..worktree(b"/r")
            }]
        );
    }
}
