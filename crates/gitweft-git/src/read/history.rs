//! The commits that changed one file, newest first, following renames (`git log --follow`).

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use gix::ObjectId;
use gix::bstr::BString;

use super::ReadError;
use super::details::{ChangeKind, Signature};
use crate::cancel::CancelToken;
use crate::cli::GitCli;

/// Starts each commit in the log output. The fields are NUL-separated; the byte could only
/// clash with a summary containing it.
const RECORD: u8 = 0x1e;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRevision {
    pub id: ObjectId,
    pub parents: Vec<ObjectId>,
    pub author: Signature,
    pub summary: String,
    /// What the commit did to the file (`Other` for a merge, which lists no change).
    pub kind: ChangeKind,
    /// The file's path in this commit.
    pub path: BString,
    /// The path before a rename or copy made by this commit.
    pub old_path: Option<BString>,
}

/// The history of `path` (relative to the repository root) from `rev`, or from HEAD when
/// `None`, following renames. `limit` caps the number of commits.
pub fn file_history(
    git: &GitCli,
    cwd: &Path,
    rev: Option<ObjectId>,
    path: &[u8],
    limit: Option<usize>,
    cancel: &CancelToken,
) -> Result<Vec<FileRevision>, ReadError> {
    let mut command = git.command(cwd).args([
        "log",
        "--follow",
        "-z",
        "-M",
        "--name-status",
        "--no-show-signature",
        "--format=%x1e%H%x00%P%x00%an%x00%ae%x00%at%x00%s",
    ]);
    if let Some(limit) = limit {
        command = command.arg(format!("--max-count={limit}"));
    }
    if let Some(rev) = rev {
        command = command.arg(rev.to_string());
    }
    let output = command
        .arg("--")
        .arg(OsStr::from_bytes(path))
        // The path is a file name, not a pattern (`*.txt` is a file called that).
        .env("GIT_LITERAL_PATHSPECS", "1")
        .read_only()
        .run(cancel)?;
    parse_history(&output.stdout, path)
}

fn parse_history(stdout: &[u8], path: &[u8]) -> Result<Vec<FileRevision>, ReadError> {
    let lossy = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
    // The file's path in the commit being read; older commits may have it under an old name.
    let mut current = BString::from(path);
    let mut revisions = Vec::new();
    for record in stdout.split(|b| *b == RECORD).filter(|r| !r.is_empty()) {
        let mut fields = record.splitn(7, |b| *b == 0);
        let mut next = || fields.next().ok_or("unexpected `git log` output");
        let id = ObjectId::from_hex(next()?)?;
        let parents = std::str::from_utf8(next()?)?
            .split_whitespace()
            .map(|p| ObjectId::from_hex(p.as_bytes()))
            .collect::<Result<Vec<_>, _>>()?;
        let name = lossy(next()?);
        let email = lossy(next()?);
        let time = std::str::from_utf8(next()?)?.trim().parse()?;
        let summary = lossy(next()?);

        // `STATUS\0PATH\0`, or `R100\0OLD\0NEW\0`, after the newline that ends the format.
        let mut tokens = fields
            .next()
            .unwrap_or_default()
            .split(|b| *b == 0)
            .map(|t| t.strip_prefix(b"\n").unwrap_or(t))
            .filter(|t| !t.is_empty());
        let change = tokens.next().map(|status| {
            let kind = ChangeKind::from_status(status[0]);
            match kind {
                ChangeKind::Renamed | ChangeKind::Copied => (kind, tokens.next(), tokens.next()),
                _ => (kind, None, tokens.next()),
            }
        });
        let (kind, path, old_path) = match change {
            Some((kind, old, Some(new))) => (kind, BString::from(new), old.map(BString::from)),
            _ => (ChangeKind::Other, current.clone(), None),
        };
        current = old_path.clone().unwrap_or_else(|| path.clone());
        revisions.push(FileRevision {
            id,
            parents,
            author: Signature { name, email, time },
            summary,
            kind,
            path,
            old_path,
        });
    }
    Ok(revisions)
}
