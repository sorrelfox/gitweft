//! Dangling commits: the tips of lost history that no ref and no reflog reaches any
//! more (a reset or amend whose reflog entries expired, a dropped stash). Their
//! ancestors are reached through them, so each tip stands for a whole lost line.
//!
//! The set is `git fsck --dangling` *with* reflogs counted as reachable (no
//! `--no-reflogs`): a commit only a reflog still has shows on the reflog page, and
//! appears here once the reflog entry expires. `--unreachable` would list every lost
//! ancestor as well (plus their trees and blobs), not just the tips.

use std::collections::HashMap;
use std::path::Path;

use gix::ObjectId;

use super::{ReadError, Signature};
use crate::cancel::CancelToken;
use crate::cli::{GitCli, GitError, GitErrorKind};

/// Ends each commit in the log output (the fields are NUL-separated).
const RECORD: u8 = 0x1e;
const LOG_FORMAT: &str = "--format=%H%x00%P%x00%an%x00%ae%x00%at%x00%ct%x00%s%x1e";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DanglingCommit {
    pub id: ObjectId,
    pub author: Signature,
    /// Committer time, in seconds since the epoch (what the list is sorted by).
    pub time: i64,
    pub summary: String,
    /// A stash entry that was dropped or cleared (see [`is_stash`] for the rule).
    pub stash: bool,
}

/// The newest `limit` dangling commits (by committer date), and how many there are in
/// all. Reads only: never `--lost-found`, and no optional locks. fsck reads every object,
/// which takes a while on big repositories; `cancel` stops it.
///
/// fsck exits non-zero when it finds corruption, but still lists what it could: those
/// are used (after dropping ids that can't be read), and it only fails when it listed
/// nothing.
pub fn dangling_commits(
    git: &GitCli,
    cwd: &Path,
    limit: usize,
    cancel: &CancelToken,
) -> Result<(Vec<DanglingCommit>, usize), ReadError> {
    // fsck's exit code is a bit set of the kinds of errors it found (1 to 0x1ff, cut to
    // 8 bits); 128 is a fatal error (not a repository), where it listed nothing.
    let corruption: Vec<i32> = (1..128).collect();
    let output = git
        .command(cwd)
        .args(["fsck", "--dangling", "--connectivity-only", "--no-progress"])
        .allow_exit_codes(&corruption)
        .read_only()
        .run(cancel)?;
    let mut ids = parse_fsck(&output.stdout);
    if output.status != 0 {
        if ids.is_empty() {
            return Err(Box::new(GitError {
                kind: GitErrorKind::Other,
                command: "git fsck".to_owned(),
                stderr: output.stderr,
                exit_code: Some(output.status),
            }));
        }
        ids = readable_commits(git, cwd, &ids, cancel)?;
    }
    let total = ids.len();
    if ids.is_empty() || limit == 0 {
        return Ok((Vec::new(), total));
    }

    // `--no-walk` lists just these commits, newest first by committer date.
    let commits = log_commits(git, cwd, &ids, Some(limit), cancel)?;
    let parents_to_check: Vec<ObjectId> = commits
        .iter()
        .filter(|c| looks_like_stash(c))
        .flat_map(|c| c.parents[1..].iter().copied())
        .collect();
    // Unreadable parents only mean "not recognisably a stash".
    let parents: HashMap<ObjectId, Logged> = if parents_to_check.is_empty() {
        HashMap::new()
    } else {
        log_commits(git, cwd, &parents_to_check, None, cancel)
            .map(|list| list.into_iter().map(|c| (c.id, c)).collect())
            .unwrap_or_default()
    };
    let list = commits
        .iter()
        .map(|commit| DanglingCommit {
            id: commit.id,
            author: commit.author.clone(),
            time: commit.time,
            summary: commit.summary.clone(),
            stash: is_stash(commit, |id| parents.get(&id)),
        })
        .collect();
    Ok((list, total))
}

/// `dangling commit <id>` lines (fsck also reports blobs, trees and tags).
fn parse_fsck(stdout: &[u8]) -> Vec<ObjectId> {
    stdout
        .split(|b| *b == b'\n')
        .filter_map(|line| line.strip_prefix(b"dangling commit "))
        .filter_map(|hex| ObjectId::from_hex(hex.trim_ascii()).ok())
        .collect()
}

/// The ids among `ids` that are commits git can read (`cat-file --batch-check` says
/// `missing` for the others).
fn readable_commits(
    git: &GitCli,
    cwd: &Path,
    ids: &[ObjectId],
    cancel: &CancelToken,
) -> Result<Vec<ObjectId>, ReadError> {
    let output = git
        .command(cwd)
        .args(["cat-file", "--batch-check=%(objectname) %(objecttype)"])
        .stdin(stdin_ids(ids))
        .read_only()
        .run(cancel)?;
    Ok(output
        .stdout
        .split(|b| *b == b'\n')
        .filter_map(|line| line.strip_suffix(b" commit"))
        .filter_map(|hex| ObjectId::from_hex(hex).ok())
        .collect())
}

fn stdin_ids(ids: &[ObjectId]) -> String {
    ids.iter().map(|id| format!("{id}\n")).collect()
}

struct Logged {
    id: ObjectId,
    parents: Vec<ObjectId>,
    author: Signature,
    time: i64,
    summary: String,
}

/// `git log --no-walk` over `ids`: newest first when `limit` is given, otherwise in the
/// given order.
fn log_commits(
    git: &GitCli,
    cwd: &Path,
    ids: &[ObjectId],
    limit: Option<usize>,
    cancel: &CancelToken,
) -> Result<Vec<Logged>, ReadError> {
    let mut command = git
        .command(cwd)
        .args(["log", "--no-show-signature", LOG_FORMAT]);
    // `--max-count` must come before `--no-walk`: parsing `--max-count` turns walking
    // back on (so that `git show -3` walks), and a later `--no-walk` turns it off again.
    command = match limit {
        Some(limit) => command.arg(format!("--max-count={limit}")).arg("--no-walk"),
        None => command.arg("--no-walk=unsorted"),
    };
    let output = command
        .arg("--stdin")
        .stdin(stdin_ids(ids))
        .read_only()
        .run(cancel)?;
    parse_log(&output.stdout)
}

fn parse_log(stdout: &[u8]) -> Result<Vec<Logged>, ReadError> {
    let lossy = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
    let mut commits = Vec::new();
    for record in stdout.split(|b| *b == RECORD) {
        let record = record.strip_prefix(b"\n").unwrap_or(record);
        if record.is_empty() {
            continue;
        }
        let mut fields = record.splitn(7, |b| *b == 0);
        let mut next = || fields.next().ok_or("unexpected `git log` output");
        let id = ObjectId::from_hex(next()?)?;
        let parents = next()?
            .split(|b| *b == b' ')
            .filter(|hex| !hex.is_empty())
            .map(ObjectId::from_hex)
            .collect::<Result<Vec<_>, _>>()?;
        let name = lossy(next()?);
        let email = lossy(next()?);
        let author_time = std::str::from_utf8(next()?)?.parse()?;
        let time = std::str::from_utf8(next()?)?.parse()?;
        let summary = lossy(next()?);
        commits.push(Logged {
            id,
            parents,
            author: Signature {
                name,
                email,
                time: author_time,
            },
            time,
            summary,
        });
    }
    Ok(commits)
}

/// The stash commit's own shape: 2 or 3 parents, and the message `git stash` writes
/// (`WIP on <branch>: <sha> <subject>`, or `On <branch>: <message>` for one given a
/// message, `git stash create` included).
fn looks_like_stash(commit: &Logged) -> bool {
    (2..=3).contains(&commit.parents.len())
        && (commit.summary.starts_with("WIP on ") || commit.summary.starts_with("On "))
        && commit.summary.contains(": ")
}

/// A stash, as `git stash` builds it: [`looks_like_stash`], and its second parent is the
/// index commit (`index on <branch>: …`, on the same first parent), and a third parent
/// is the untracked-files commit (`untracked files on <branch>: …`, with no parent). A
/// merge whose message merely starts with "On " fails the parent checks.
fn is_stash<'a>(commit: &Logged, parent: impl Fn(ObjectId) -> Option<&'a Logged>) -> bool {
    if !looks_like_stash(commit) {
        return false;
    }
    let index = parent(commit.parents[1]).is_some_and(|index| {
        index.summary.starts_with("index on ") && index.parents.first() == commit.parents.first()
    });
    let untracked = commit.parents.get(2).is_none_or(|&id| {
        parent(id).is_some_and(|untracked| {
            untracked.summary.starts_with("untracked files on ") && untracked.parents.is_empty()
        })
    });
    index && untracked
}
