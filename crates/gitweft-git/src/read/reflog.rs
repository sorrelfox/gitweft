//! A ref's reflog (where it pointed, newest first), and which of those commits only the
//! reflog still remembers: commits lost to a reset, an amend or a rebase.

use std::collections::HashSet;
use std::path::Path;

use gix::ObjectId;

use super::ReadError;
use crate::cancel::CancelToken;
use crate::cli::GitCli;

/// Ends each entry in the log output (the fields are NUL-separated).
const RECORD: u8 = 0x1e;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflogEntry {
    /// Position in the reflog, as in `<ref>@{index}`.
    pub index: usize,
    /// Where the ref pointed after this change.
    pub id: ObjectId,
    /// When the ref moved, in seconds since the epoch.
    pub time: i64,
    /// What moved it, as git recorded it: "commit: Fix typo", "reset: moving to HEAD~1".
    pub action: String,
    pub author: String,
    /// The commit's summary line.
    pub summary: String,
}

/// The newest `limit` entries of `refname`'s reflog (`HEAD`, `main`, `refs/heads/main`).
pub fn reflog(
    git: &GitCli,
    cwd: &Path,
    refname: &str,
    limit: usize,
    cancel: &CancelToken,
) -> Result<Vec<ReflogEntry>, ReadError> {
    let output = git
        .command(cwd)
        .args([
            "log",
            "--walk-reflogs",
            "--date=unix",
            "--no-show-signature",
            "--format=%H%x00%gd%x00%gs%x00%an%x00%s%x1e",
        ])
        .arg(format!("--max-count={limit}"))
        .arg(refname)
        .arg("--")
        .read_only()
        .run(cancel)?;
    parse_reflog(&output.stdout)
}

fn parse_reflog(stdout: &[u8]) -> Result<Vec<ReflogEntry>, ReadError> {
    let lossy = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
    let mut entries = Vec::new();
    for record in stdout.split(|b| *b == RECORD) {
        let record = record.strip_prefix(b"\n").unwrap_or(record);
        if record.is_empty() {
            continue;
        }
        let mut fields = record.splitn(5, |b| *b == 0);
        let mut next = || fields.next().ok_or("unexpected `git log -g` output");
        let id = ObjectId::from_hex(next()?)?;
        // With `--date=unix`, the selector carries the time: `HEAD@{1767225600}`.
        let selector = std::str::from_utf8(next()?)?;
        let time = selector
            .rsplit_once('{')
            .and_then(|(_, rest)| rest.strip_suffix('}'))
            .ok_or("unexpected reflog selector")?
            .parse()?;
        let action = lossy(next()?);
        let author = lossy(next()?);
        let summary = lossy(next()?);
        entries.push(ReflogEntry {
            index: entries.len(),
            id,
            time,
            action,
            author,
            summary,
        });
    }
    Ok(entries)
}

/// The commits among `ids` that no ref reaches: not a branch, tag, remote branch, HEAD
/// or the latest stash. Only a reflog still remembers them.
pub fn unreachable_from_refs(
    git: &GitCli,
    cwd: &Path,
    ids: &[ObjectId],
    cancel: &CancelToken,
) -> Result<HashSet<ObjectId>, ReadError> {
    if ids.is_empty() {
        return Ok(HashSet::new());
    }
    let stdin: String = ids.iter().map(|id| format!("{id}\n")).collect();
    // `--not --all` comes before `--stdin`, so the ids read from stdin stay positive:
    // git lists what they reach and no ref does.
    let output = git
        .command(cwd)
        .args(["rev-list", "--not", "--all", "--stdin"])
        .stdin(stdin)
        .read_only()
        .run(cancel)?;
    let listed = output
        .stdout
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(ObjectId::from_hex)
        .collect::<Result<HashSet<_>, _>>()?;
    Ok(ids
        .iter()
        .copied()
        .filter(|id| listed.contains(id))
        .collect())
}
