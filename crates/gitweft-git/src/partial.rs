//! Partial clones (`--filter=blob:none` and the like): what a read found missing is fetched
//! only when the user asks. Read-only runs set `GIT_NO_LAZY_FETCH` (see the runner),
//! so git never goes to the promisor remote by itself; a read that needed a missing object
//! fails with [`GitErrorKind::MissingObjects`](crate::GitErrorKind::MissingObjects), and
//! [`fetch_missing`] downloads just those objects, the way git's own lazy fetch would.

use std::path::Path;

use gitweft_platform::bytes::os_str_from_bytes;
use gix::bstr::{BStr, BString};

use crate::cancel::CancelToken;
use crate::cli::{GitCli, GitError};

/// Whether `err` is a read that stopped at objects missing from a partial clone.
pub fn is_missing_objects(err: &(dyn std::error::Error + 'static)) -> bool {
    err.downcast_ref::<GitError>()
        .is_some_and(|err| err.kind == crate::GitErrorKind::MissingObjects)
}

/// The remote a partial clone fetches missing objects from: the first
/// `remote.<name>.promisor = true`, else `extensions.partialClone` (older clones).
pub fn promisor_remote(
    git: &GitCli,
    cwd: &Path,
    cancel: &CancelToken,
) -> Result<Option<String>, GitError> {
    let output = git
        .command(cwd)
        .args(["config", "-z", "--get-regexp", r"^remote\..*\.promisor$"])
        .allow_exit_codes(&[1])
        .read_only()
        .run(cancel)?;
    for record in output.stdout.split(|&b| b == 0) {
        let text = String::from_utf8_lossy(record);
        let Some((key, value)) = text.split_once('\n') else {
            continue;
        };
        let promisor = matches!(value.trim(), "true" | "yes" | "on" | "1");
        let name = key
            .strip_prefix("remote.")
            .and_then(|rest| rest.strip_suffix(".promisor"));
        if let (true, Some(name)) = (promisor, name) {
            return Ok(Some(name.to_owned()));
        }
    }
    let older = git
        .command(cwd)
        .args(["config", "--get", "extensions.partialClone"])
        .allow_exit_codes(&[1])
        .read_only()
        .run(cancel)?;
    let name = older.stdout_str().trim().to_owned();
    Ok((!name.is_empty()).then_some(name))
}

/// The blobs of `files` (`(commit, path)`) that aren't in the repository. Trees are there
/// in a blobless clone, so finding a blob's id needs nothing fetched (`ls-tree`); a path
/// that isn't in its commit is left out.
pub fn missing_blobs(
    git: &GitCli,
    cwd: &Path,
    files: &[(String, BString)],
    cancel: &CancelToken,
) -> Result<Vec<String>, GitError> {
    let mut ids = Vec::new();
    for (commit, path) in files {
        // `<mode> blob <id>\t<path>\0`, or nothing for a path that isn't there.
        let listed = git
            .command(cwd)
            .args(["ls-tree", "-z", "--full-tree", "--end-of-options"])
            .arg(commit)
            .arg("--")
            .arg(os_str_from_bytes(path))
            .env("GIT_LITERAL_PATHSPECS", "1")
            .read_only()
            .run(cancel)?;
        for entry in listed.stdout.split(|&b| b == 0) {
            let text = String::from_utf8_lossy(entry);
            let mut fields = text.split_whitespace();
            if let (Some(_), Some("blob"), Some(id)) = (fields.next(), fields.next(), fields.next())
                && !ids.iter().any(|seen| seen == id)
            {
                ids.push(id.to_owned());
            }
        }
    }
    missing_of(git, cwd, &ids, cancel)
}

/// Those of `ids` that aren't in the repository ("<id> missing" from `cat-file
/// --batch-check`; the run doesn't fetch them: read-only).
fn missing_of(
    git: &GitCli,
    cwd: &Path,
    ids: &[String],
    cancel: &CancelToken,
) -> Result<Vec<String>, GitError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut input = ids.join("\n");
    input.push('\n');
    let checked = git
        .command(cwd)
        .args(["cat-file", "--batch-check"])
        .stdin(input)
        .read_only()
        .run(cancel)?;
    Ok(checked
        .stdout_str()
        .lines()
        .filter_map(|line| line.strip_suffix(" missing"))
        .map(str::to_owned)
        .collect())
}

/// Every version of `path` along `rev`'s history (what blame reads): the blob ids
/// `git log --raw` names for it, which needs only commits and trees. Renames aren't
/// followed (blame doesn't either).
pub fn history_blobs(
    git: &GitCli,
    cwd: &Path,
    rev: &str,
    path: &BStr,
    cancel: &CancelToken,
) -> Result<Vec<String>, GitError> {
    let output = git
        .command(cwd)
        .args([
            "log",
            "--no-renames",
            "--format=",
            "--raw",
            "--no-abbrev",
            "-m",
            "--end-of-options",
        ])
        .arg(rev)
        .arg("--")
        .arg(os_str_from_bytes(path))
        .env("GIT_LITERAL_PATHSPECS", "1")
        .read_only()
        .run(cancel)?;
    let mut ids: Vec<String> = Vec::new();
    // `:<mode> <mode> <old id> <new id> <status>\t<path>`
    for line in output.stdout_str().lines() {
        let Some(fields) = line.strip_prefix(':') else {
            continue;
        };
        let mut fields = fields.split_whitespace().skip(2);
        for id in [fields.next(), fields.next()].into_iter().flatten() {
            if id.bytes().any(|b| b != b'0') && !ids.iter().any(|seen| seen == id) {
                ids.push(id.to_owned());
            }
        }
    }
    Ok(ids)
}

/// Downloads every version of `path` along `rev`'s history that the partial clone lacks
/// (for blame). Returns how many were missing.
pub fn fetch_history(
    git: &GitCli,
    cwd: &Path,
    rev: &str,
    path: &BStr,
    cancel: &CancelToken,
) -> Result<usize, GitError> {
    let ids = history_blobs(git, cwd, rev, path, cancel)?;
    let missing = missing_of(git, cwd, &ids, cancel)?;
    fetch_ids(git, cwd, &missing, cancel)?;
    Ok(missing.len())
}

/// Downloads the blobs of `files` that the partial clone lacks, from its promisor
/// remote, as git's lazy fetch does (`fetch --filter=blob:none --stdin`, no tags, no
/// FETCH_HEAD). A network operation, run only on the user's request. Returns how many
/// objects were missing (0: nothing to do, nothing run).
pub fn fetch_missing(
    git: &GitCli,
    cwd: &Path,
    files: &[(String, BString)],
    cancel: &CancelToken,
) -> Result<usize, GitError> {
    let missing = missing_blobs(git, cwd, files, cancel)?;
    fetch_ids(git, cwd, &missing, cancel)?;
    Ok(missing.len())
}

/// Fetches `ids` from the promisor remote, as git's own lazy fetch does (nothing to do for
/// none).
fn fetch_ids(
    git: &GitCli,
    cwd: &Path,
    ids: &[String],
    cancel: &CancelToken,
) -> Result<(), GitError> {
    if ids.is_empty() {
        return Ok(());
    }
    let Some(remote) = promisor_remote(git, cwd, cancel)? else {
        return Err(GitError::new(
            crate::GitErrorKind::Other,
            "git fetch",
            "This repository has no promisor remote to fetch missing objects from.",
        ));
    };
    let mut input = ids.join("\n");
    input.push('\n');
    git.command(cwd)
        .args([
            "-c",
            "fetch.negotiationAlgorithm=noop",
            "fetch",
            "--no-tags",
            "--no-write-fetch-head",
            "--recurse-submodules=no",
            "--filter=blob:none",
            "--stdin",
            "--end-of-options",
        ])
        .arg(&remote)
        .stdin(input)
        .run(cancel)?;
    Ok(())
}
