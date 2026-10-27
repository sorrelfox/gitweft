//! Working-tree status via `git status --porcelain=v2 -z --branch`.
//!
//! The CLI is used (not gix) so the result is exactly git's: fsmonitor, sparse checkouts,
//! submodules and `status.*` configuration all apply. Paths are kept as raw bytes so files
//! with non-UTF-8 names can still be staged.

use std::path::Path;

use gix::ObjectId;
use gix::bstr::{BString, ByteSlice};

use super::ReadError;
use super::details::ChangeKind;
use crate::cancel::CancelToken;
use crate::cli::GitCli;

/// The branch header of `git status`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BranchStatus {
    /// Checked-out branch; `None` when HEAD is detached.
    pub head: Option<String>,
    /// HEAD commit; `None` on an unborn branch.
    pub oid: Option<ObjectId>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Tracked,
    Untracked,
    /// Unmerged path during a merge/rebase/cherry-pick; see [`StatusEntry::code`].
    Conflicted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    pub path: BString,
    /// Source of a rename/copy.
    pub orig_path: Option<BString>,
    /// Change between HEAD and the index (staged).
    pub index: Option<ChangeKind>,
    /// Change between the index and the working tree (unstaged).
    pub worktree: Option<ChangeKind>,
    pub kind: EntryKind,
    /// Git's two-letter `XY` code (e.g. `b"UU"` for a both-modified conflict).
    pub code: [u8; 2],
    pub submodule: bool,
}

impl StatusEntry {
    pub fn display_path(&self) -> String {
        self.path.to_str_lossy().into_owned()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub branch: BranchStatus,
    pub entries: Vec<StatusEntry>,
}

impl Status {
    pub fn is_clean(&self) -> bool {
        self.entries.is_empty()
    }

    /// Entries with staged changes.
    pub fn staged(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries
            .iter()
            .filter(|e| e.kind == EntryKind::Tracked && e.index.is_some())
    }

    /// Entries with unstaged changes, untracked files and conflicts.
    pub fn unstaged(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries
            .iter()
            .filter(|e| e.kind != EntryKind::Tracked || e.worktree.is_some())
    }

    pub fn conflicts(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries
            .iter()
            .filter(|e| e.kind == EntryKind::Conflicted)
    }
}

pub fn read_status(
    git: &GitCli,
    workdir: &Path,
    cancel: &CancelToken,
) -> Result<Status, ReadError> {
    let output = git
        .command(workdir)
        .args([
            "status",
            "--porcelain=v2",
            "-z",
            "--branch",
            "--untracked-files=all",
        ])
        .read_only()
        .run(cancel)?;
    parse_porcelain_v2(&output.stdout)
}

/// Parses `git status --porcelain=v2 -z --branch` output.
pub fn parse_porcelain_v2(stdout: &[u8]) -> Result<Status, ReadError> {
    let mut status = Status::default();
    let mut records = stdout.split(|b| *b == 0);
    while let Some(record) = records.next() {
        let Some(&tag) = record.first() else {
            continue;
        };
        match tag {
            b'#' => parse_header(record, &mut status.branch)?,
            b'1' => {
                let fields: Vec<&[u8]> = record.splitn(9, |b| *b == b' ').collect();
                let [_, xy, sub, _, _, _, _, _, path] = fields[..] else {
                    return Err(malformed(record));
                };
                status.entries.push(tracked(xy, sub, path, None)?);
            }
            b'2' => {
                let fields: Vec<&[u8]> = record.splitn(10, |b| *b == b' ').collect();
                let [_, xy, sub, _, _, _, _, _, _, path] = fields[..] else {
                    return Err(malformed(record));
                };
                // With -z the rename source follows as its own record.
                let orig = records.next().ok_or_else(|| malformed(record))?;
                status.entries.push(tracked(xy, sub, path, Some(orig))?);
            }
            b'u' => {
                let fields: Vec<&[u8]> = record.splitn(11, |b| *b == b' ').collect();
                let [_, xy, sub, .., path] = fields[..] else {
                    return Err(malformed(record));
                };
                status.entries.push(StatusEntry {
                    path: path.into(),
                    orig_path: None,
                    index: None,
                    worktree: None,
                    kind: EntryKind::Conflicted,
                    code: code(xy)?,
                    submodule: sub.first() == Some(&b'S'),
                });
            }
            b'?' => status.entries.push(StatusEntry {
                path: record.get(2..).unwrap_or_default().into(),
                orig_path: None,
                index: None,
                worktree: Some(ChangeKind::Added),
                kind: EntryKind::Untracked,
                code: *b"??",
                submodule: false,
            }),
            b'!' => {} // ignored files are not requested
            _ => return Err(malformed(record)),
        }
    }
    Ok(status)
}

fn parse_header(record: &[u8], branch: &mut BranchStatus) -> Result<(), ReadError> {
    let line = record.to_str_lossy();
    let mut words = line.split(' ').skip(1);
    match (words.next(), words.next()) {
        (Some("branch.oid"), Some(oid)) => {
            branch.oid = (oid != "(initial)")
                .then(|| ObjectId::from_hex(oid.as_bytes()))
                .transpose()?;
        }
        (Some("branch.head"), Some(head)) => {
            branch.head = (head != "(detached)").then(|| head.to_owned());
        }
        (Some("branch.upstream"), Some(upstream)) => branch.upstream = Some(upstream.to_owned()),
        (Some("branch.ab"), Some(ahead)) => {
            branch.ahead = ahead.trim_start_matches('+').parse()?;
            branch.behind = words
                .next()
                .unwrap_or("-0")
                .trim_start_matches('-')
                .parse()?;
        }
        _ => {} // e.g. `# stash <n>`
    }
    Ok(())
}

fn tracked(
    xy: &[u8],
    sub: &[u8],
    path: &[u8],
    orig: Option<&[u8]>,
) -> Result<StatusEntry, ReadError> {
    let code = code(xy)?;
    Ok(StatusEntry {
        path: path.into(),
        orig_path: orig.map(Into::into),
        index: change(code[0]),
        worktree: change(code[1]),
        kind: EntryKind::Tracked,
        code,
        submodule: sub.first() == Some(&b'S'),
    })
}

fn code(xy: &[u8]) -> Result<[u8; 2], ReadError> {
    xy.try_into().map_err(|_| "malformed XY status code".into())
}

fn change(letter: u8) -> Option<ChangeKind> {
    Some(match letter {
        b'.' => return None,
        b'A' => ChangeKind::Added,
        b'M' => ChangeKind::Modified,
        b'D' => ChangeKind::Deleted,
        b'R' => ChangeKind::Renamed,
        b'C' => ChangeKind::Copied,
        b'T' => ChangeKind::TypeChanged,
        _ => ChangeKind::Other,
    })
}

fn malformed(record: &[u8]) -> ReadError {
    format!("unexpected `git status` record: {}", record.to_str_lossy()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_record_kinds() {
        let raw = [
            "# branch.oid 0123456789012345678901234567890123456789",
            "# branch.head main",
            "# branch.upstream origin/main",
            "# branch.ab +2 -1",
            "1 .M N... 100644 100644 100644 aaaa aaaa src/lib.rs",
            "1 A. N... 000000 100644 100644 0000 bbbb new file.txt",
            "2 R. N... 100644 100644 100644 cccc cccc R100 renamed.txt",
            "old.txt",
            "u UU N... 100644 100644 100644 100644 1111 2222 3333 conflict.rs",
            "? untracked dir/x",
        ]
        .join("\0");
        let status = parse_porcelain_v2(raw.as_bytes()).unwrap();

        assert_eq!(status.branch.head.as_deref(), Some("main"));
        assert_eq!(status.branch.upstream.as_deref(), Some("origin/main"));
        assert_eq!((status.branch.ahead, status.branch.behind), (2, 1));
        assert_eq!(status.entries.len(), 5);

        let modified = &status.entries[0];
        assert_eq!(
            (modified.index, modified.worktree),
            (None, Some(ChangeKind::Modified))
        );
        assert_eq!(status.entries[1].path, "new file.txt");
        assert_eq!(status.entries[1].index, Some(ChangeKind::Added));
        let renamed = &status.entries[2];
        assert_eq!(renamed.index, Some(ChangeKind::Renamed));
        assert_eq!(renamed.orig_path.as_ref().unwrap(), "old.txt");
        assert_eq!(status.entries[3].kind, EntryKind::Conflicted);
        assert_eq!(&status.entries[3].code, b"UU");
        assert_eq!(status.entries[4].kind, EntryKind::Untracked);
        assert_eq!(status.entries[4].path, "untracked dir/x");

        assert_eq!(status.staged().count(), 2);
        assert_eq!(status.unstaged().count(), 3); // modified, conflict, untracked
    }

    #[test]
    fn unborn_and_detached_headers() {
        let status =
            parse_porcelain_v2(b"# branch.oid (initial)\0# branch.head (detached)\0").unwrap();
        assert_eq!(status.branch.oid, None);
        assert_eq!(status.branch.head, None);
        assert!(status.is_clean());
    }
}
