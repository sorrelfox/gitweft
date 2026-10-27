//! Details of a single commit, loaded when the user selects it.
//!
//! Uses the git CLI: two short read-only calls give the exact data `git show` would,
//! including rename detection, and cost a few milliseconds per selection.

use std::collections::HashMap;
use std::path::Path;

use gix::ObjectId;

use super::ReadError;
use crate::cancel::CancelToken;
use crate::cli::GitCli;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub name: String,
    pub email: String,
    /// Seconds since the Unix epoch.
    pub time: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitDetails {
    pub id: ObjectId,
    pub parents: Vec<ObjectId>,
    pub author: Signature,
    pub committer: Signature,
    /// Full message, including the summary line.
    pub message: String,
    /// Files changed relative to the first parent (everything, for a root commit).
    pub changes: Vec<FileChange>,
    /// `None` for an unsigned commit.
    pub signature: Option<CommitSignature>,
}

/// How a commit's signature checks out, as git reports it (`%G?`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureStatus {
    /// `G`: good, from a trusted key (for SSH: one listed in `gpg.ssh.allowedSignersFile`).
    Good,
    /// `U`: good, but the key isn't trusted (for SSH: not in the allowed signers).
    Untrusted,
    /// `B`: the signature doesn't match the commit.
    Bad,
    /// `X`: the signature has expired.
    ExpiredSignature,
    /// `Y`: made with a key that has expired since.
    ExpiredKey,
    /// `R`: made with a revoked key.
    RevokedKey,
    /// `E`, or no answer: the key is missing, or SSH verification isn't set up.
    CannotCheck,
}

impl SignatureStatus {
    fn from_letter(letter: &str) -> Self {
        match letter {
            "G" => Self::Good,
            "U" => Self::Untrusted,
            "B" => Self::Bad,
            "X" => Self::ExpiredSignature,
            "Y" => Self::ExpiredKey,
            "R" => Self::RevokedKey,
            _ => Self::CannotCheck,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitSignature {
    pub status: SignatureStatus,
    /// Who signed, as the signing tool reports it (`%GS`; the principal for SSH).
    pub signer: String,
    /// The signing key (`%GK`), e.g. `SHA256:…` for SSH.
    pub key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Other,
}

impl ChangeKind {
    /// From the status letter of `--name-status` output (`R` of `R092`).
    pub fn from_status(letter: u8) -> Self {
        match letter {
            b'A' => Self::Added,
            b'M' => Self::Modified,
            b'D' => Self::Deleted,
            b'R' => Self::Renamed,
            b'C' => Self::Copied,
            b'T' => Self::TypeChanged,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub kind: ChangeKind,
    /// As git has it: a path needn't be UTF-8 (#18). Shown with `to_str_lossy`.
    pub path: gix::bstr::BString,
    /// Source path of a rename or copy.
    pub old_path: Option<gix::bstr::BString>,
    /// Lines added and removed; `None` for a binary file, or where nothing counted them
    /// (stashes, comparisons).
    pub lines: Option<LineCount>,
}

/// Lines a change added and removed (`git diff --numstat`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LineCount {
    pub added: u32,
    pub removed: u32,
}

impl CommitDetails {
    /// Lines added and removed over the files that were counted; `None` when none were.
    pub fn line_totals(&self) -> Option<LineCount> {
        self.changes
            .iter()
            .filter_map(|change| change.lines)
            .reduce(|a, b| LineCount {
                added: a.added + b.added,
                removed: a.removed + b.removed,
            })
    }
}

pub fn commit_details(
    git: &GitCli,
    git_dir: &Path,
    id: ObjectId,
    cancel: &CancelToken,
) -> Result<CommitDetails, ReadError> {
    let git_dir = std::path::absolute(git_dir)?;
    let run = |args: &[&str]| {
        git.command(&git_dir)
            .env("GIT_DIR", &git_dir)
            .args(args)
            .read_only()
            .run(cancel)
    };

    let hex = id.to_string();
    let show = run(&[
        "show",
        "-s",
        "--no-show-signature",
        "--format=%P%x00%an%x00%ae%x00%at%x00%cn%x00%ce%x00%ct%x00%G?%x00%GS%x00%GK%x00%B",
        &hex,
    ])?;
    let text = show.stdout_str();
    let mut fields = text.splitn(11, '\0');
    let mut next = || fields.next().ok_or("unexpected `git show` output");
    let parents = next()?
        .split_whitespace()
        .map(|p| ObjectId::from_hex(p.as_bytes()))
        .collect::<Result<Vec<_>, _>>()?;
    let mut signature = || -> Result<Signature, ReadError> {
        Ok(Signature {
            name: next()?.to_owned(),
            email: next()?.to_owned(),
            time: next()?.trim().parse()?,
        })
    };
    let author = signature()?;
    let committer = signature()?;
    let (status, signer, key) = (
        next()?.trim().to_owned(),
        next()?.to_owned(),
        next()?.to_owned(),
    );
    let message = next()?.trim_end().to_owned();

    // `%G?` answers N both for an unsigned commit and for a signature git can't check
    // (SSH without allowed signers), so whether it is signed comes from the commit itself.
    let raw = run(&["cat-file", "commit", &hex])?;
    let signature = has_signature(&raw.stdout).then(|| CommitSignature {
        status: SignatureStatus::from_letter(&status),
        signer,
        key,
    });

    // The changes, then their line counts: `--numstat` has no status letters.
    let diff_tree = |options: &[&str]| -> Result<Vec<u8>, ReadError> {
        let mut args = vec!["diff-tree", "-r", "-z"];
        args.extend_from_slice(options);
        args.push("--no-ext-diff");
        let first = parents.first().map(ToString::to_string);
        match &first {
            Some(first) => args.extend([first.as_str(), hex.as_str()]),
            None => args.extend(["--no-commit-id", "--root", hex.as_str()]),
        }
        Ok(run(&args)?.stdout)
    };
    // The changes and their line counts in one run (`--raw` then `--numstat`, #25). In a
    // partial clone the versions' contents may be missing, and reads don't fetch them
    // (#82): then renames are found by identical content only (no `-M` on contents), and
    // the files are listed without line counts.
    let missing = |err: &ReadError| crate::partial::is_missing_objects(err.as_ref());
    let changes = match diff_tree(&["-M", "--raw", "--numstat"]) {
        Ok(both) => {
            let (names, counts) = split_raw_numstat(&both);
            let mut changes = parse_name_status(&names);
            add_line_counts(&mut changes, &parse_numstat(counts));
            changes
        }
        Err(err) if missing(&err) => {
            let names = diff_tree(&["--find-renames=100%", "--name-status"]).or_else(|err| {
                if missing(&err) {
                    diff_tree(&["--name-status"])
                } else {
                    Err(err)
                }
            })?;
            parse_name_status(&names)
        }
        Err(err) => return Err(err),
    };

    Ok(CommitDetails {
        id,
        parents,
        author,
        committer,
        message,
        changes,
        signature,
    })
}

/// The raw commit object has a `gpgsig` (or `gpgsig-sha256`) header: it is signed with
/// OpenPGP, SSH or X.509.
/// Whether each of `ids` is signed and how its signature checks out, for the graph's row
/// marks: `None` for an unsigned commit. Whether one is signed comes from the commit itself
/// (as for [`commit_details`]); only the signed ones are verified (`%G?`, one `git log`
/// for all of them), so an unsigned history costs no gpg or ssh runs.
pub fn signature_marks(
    git: &GitCli,
    git_dir: &Path,
    repo: &gix::Repository,
    ids: &[ObjectId],
    cancel: &CancelToken,
) -> Result<Vec<(ObjectId, Option<SignatureStatus>)>, ReadError> {
    let mut marks = Vec::with_capacity(ids.len());
    let mut signed = Vec::new();
    for &id in ids {
        if has_signature(&repo.find_object(id)?.data) {
            signed.push(id);
        } else {
            marks.push((id, None));
        }
    }
    if signed.is_empty() {
        return Ok(marks);
    }
    let git_dir = std::path::absolute(git_dir)?;
    let output = git
        .command(&git_dir)
        .env("GIT_DIR", &git_dir)
        .args([
            "log",
            "--no-walk=unsorted",
            "--no-show-signature",
            "--format=%H%x00%G?",
        ])
        .args(signed.iter().map(ObjectId::to_string))
        .read_only()
        .run(cancel)?;
    let text = output.stdout_str();
    let checked: std::collections::HashMap<ObjectId, SignatureStatus> = text
        .lines()
        .filter_map(|line| {
            let (hex, letter) = line.split_once('\0')?;
            let id = ObjectId::from_hex(hex.as_bytes()).ok()?;
            Some((id, SignatureStatus::from_letter(letter.trim())))
        })
        .collect();
    marks.extend(signed.into_iter().map(|id| {
        let status = checked.get(&id).copied();
        (id, Some(status.unwrap_or(SignatureStatus::CannotCheck)))
    }));
    Ok(marks)
}

fn has_signature(raw_commit: &[u8]) -> bool {
    raw_commit
        .split(|b| *b == b'\n')
        .take_while(|line| !line.is_empty())
        .any(|line| line.starts_with(b"gpgsig ") || line.starts_with(b"gpgsig-sha256 "))
}

/// Parses `--name-status -z`: `STATUS\0PATH\0` or, for renames/copies, `R100\0OLD\0NEW\0`.
pub(super) fn parse_name_status(stdout: &[u8]) -> Vec<FileChange> {
    let mut tokens = stdout.split(|b| *b == 0).map(gix::bstr::BString::from);
    let mut changes = Vec::new();
    while let Some(status) = tokens.next() {
        let Some(&letter) = status.first() else {
            continue;
        };
        let kind = ChangeKind::from_status(letter);
        let (old_path, path) = if matches!(kind, ChangeKind::Renamed | ChangeKind::Copied) {
            (tokens.next(), tokens.next())
        } else {
            (None, tokens.next())
        };
        let Some(path) = path else { break };
        changes.push(FileChange {
            kind,
            path,
            old_path,
            lines: None,
        });
    }
    changes
}

/// `diff-tree -z --raw --numstat` output split into the `--name-status -z` form of its raw
/// records (`STATUS\0path\0[new path\0]`) and the numstat records that follow them.
pub(super) fn split_raw_numstat(stdout: &[u8]) -> (Vec<u8>, &[u8]) {
    let mut names = Vec::new();
    let mut at = 0;
    let next = |from: usize| -> Option<(&[u8], usize)> {
        let rest = stdout.get(from..)?;
        let end = rest.iter().position(|&b| b == 0)?;
        Some((&rest[..end], from + end + 1))
    };
    // `:<mode> <mode> <id> <id> <status>`, then one path, or two for a rename or copy.
    while let Some((record, after)) = next(at).filter(|(record, _)| record.starts_with(b":")) {
        let status = record.rsplit(|&b| b == b' ').next().unwrap_or_default();
        let paths = if matches!(status.first(), Some(b'R' | b'C')) {
            2
        } else {
            1
        };
        names.extend_from_slice(status);
        names.push(0);
        at = after;
        for _ in 0..paths {
            let Some((path, after)) = next(at) else {
                return (names, &[]);
            };
            names.extend_from_slice(path);
            names.push(0);
            at = after;
        }
    }
    (names, &stdout[at.min(stdout.len())..])
}

/// Parses `--numstat -z`: `ADDED\tREMOVED\tPATH\0`, or for renames and copies
/// `ADDED\tREMOVED\t\0OLD\0NEW\0`; a binary file counts `-\t-`. Gives each (new) path
/// with its count.
pub(super) fn parse_numstat(stdout: &[u8]) -> Vec<(gix::bstr::BString, Option<LineCount>)> {
    let lossy = gix::bstr::BString::from;
    let number = |field: &[u8]| std::str::from_utf8(field).ok()?.parse::<u32>().ok();
    let mut tokens = stdout.split(|b| *b == 0);
    let mut counts = Vec::new();
    while let Some(token) = tokens.next() {
        let mut fields = token.splitn(3, |b| *b == b'\t');
        let (Some(added), Some(removed), Some(path)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let path = if path.is_empty() {
            tokens.next(); // the old path
            match tokens.next() {
                Some(new) => lossy(new),
                None => break,
            }
        } else {
            lossy(path)
        };
        let lines = number(added)
            .zip(number(removed))
            .map(|(added, removed)| LineCount { added, removed });
        counts.push((path, lines));
    }
    counts
}

/// Gives each change the count `--numstat` found for its path.
fn add_line_counts(changes: &mut [FileChange], counts: &[(gix::bstr::BString, Option<LineCount>)]) {
    let by_path: HashMap<&[u8], Option<LineCount>> = counts
        .iter()
        .map(|(path, lines)| (path.as_slice(), *lines))
        .collect();
    for change in changes {
        change.lines = by_path.get(change.path.as_slice()).copied().flatten();
    }
}

/// Parses `--raw -z --no-abbrev`: `:MODE MODE OLD NEW STATUS\0PATH\0`, and for renames
/// and copies `…R086\0OLD PATH\0NEW PATH\0`. Each change comes with its blob on the new
/// side (the old side's for a deletion, whose new side is the null id).
pub(super) fn parse_raw(stdout: &[u8]) -> Vec<(FileChange, ObjectId)> {
    let lossy = gix::bstr::BString::from;
    let mut tokens = stdout.split(|b| *b == 0);
    let mut changes = Vec::new();
    while let Some(meta) = tokens.next() {
        let Some(meta) = meta.strip_prefix(b":") else {
            continue;
        };
        let fields: Vec<&[u8]> = meta.split(|b| *b == b' ').collect();
        let [_, _, old, new, status] = fields[..] else {
            break;
        };
        let (Ok(old), Ok(new)) = (ObjectId::from_hex(old), ObjectId::from_hex(new)) else {
            break;
        };
        let Some(&letter) = status.first() else {
            break;
        };
        let kind = ChangeKind::from_status(letter);
        let (old_path, path) = if matches!(kind, ChangeKind::Renamed | ChangeKind::Copied) {
            (tokens.next().map(lossy), tokens.next().map(lossy))
        } else {
            (None, tokens.next().map(lossy))
        };
        let Some(path) = path else { break };
        let blob = if new.is_null() { old } else { new };
        changes.push((
            FileChange {
                kind,
                path,
                old_path,
                lines: None,
            },
            blob,
        ));
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_raw_with_blobs() {
        let old = "1".repeat(40);
        let new = "2".repeat(40);
        let null = "0".repeat(40);
        let raw = format!(
            ":100644 100644 {old} {new} M\0src/a.rs\0\
             :100644 100644 {old} {new} R092\0old name.txt\0new name.txt\0\
             :000000 100644 {null} {new} A\0b\0\
             :100644 000000 {old} {null} D\0c\0"
        );
        let changes = parse_raw(raw.as_bytes());
        let id = |hex: &str| ObjectId::from_hex(hex.as_bytes()).unwrap();
        assert_eq!(changes.len(), 4);
        assert_eq!(changes[0].0.path, "src/a.rs");
        assert_eq!(changes[0].1, id(&new));
        assert_eq!(changes[1].0.kind, ChangeKind::Renamed);
        assert_eq!(
            changes[1]
                .0
                .old_path
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
            Some("old name.txt")
        );
        assert_eq!(changes[1].0.path, "new name.txt");
        assert_eq!(changes[2].0.kind, ChangeKind::Added);
        // A deletion is known by the content it removed.
        assert_eq!(changes[3].0.kind, ChangeKind::Deleted);
        assert_eq!(changes[3].1, id(&old));
    }

    #[test]
    fn signature_header_is_found_only_in_the_header() {
        let signed = b"tree 1\nauthor A\ngpgsig -----BEGIN SSH SIGNATURE-----\n x\n\nmessage\n";
        assert!(has_signature(signed));
        let sha256 = b"tree 1\ngpgsig-sha256 -----BEGIN PGP SIGNATURE-----\n\nmsg\n";
        assert!(has_signature(sha256));
        // A message that merely mentions it is no signature.
        let unsigned = b"tree 1\nauthor A\n\ngpgsig in the message\n";
        assert!(!has_signature(unsigned));
    }

    #[test]
    fn raw_and_numstat_in_one_run_split_apart() {
        // git 2.55's `diff-tree -r -z -M --raw --numstat` for a modify, a delete and a
        // rename.
        let raw: &[u8] = b":100644 100644 587b 975f M\0b.txt\0:100644 000000 badc 0000 D\0c.bin\0\
            :100644 100644 9405 0fdf R083\0a.txt\0moved.txt\0\
            1\t1\tb.txt\0-\t-\tc.bin\x001\t0\t\0a.txt\0moved.txt\0";
        let (names, counts) = super::split_raw_numstat(raw);
        assert_eq!(
            names,
            b"M\0b.txt\0D\0c.bin\0R083\0a.txt\0moved.txt\0".to_vec()
        );
        assert!(counts.starts_with(b"1\t1\tb.txt\0"));
        assert_eq!(super::parse_numstat(counts).len(), 3);
        assert_eq!(super::split_raw_numstat(b""), (Vec::new(), &b""[..]));
    }

    #[test]
    fn parses_numstat_with_renames_and_binaries() {
        // `\x00`, not `\0`: a digit after `\0` would read as an octal escape.
        let raw = b"3\t1\tsrc/a.rs\x00-\t-\timage.png\x000\t0\t\x00old name.txt\x00new name.txt\x0012\t0\tb\x00";
        let counts: Vec<(String, Option<LineCount>)> = parse_numstat(raw)
            .into_iter()
            .map(|(path, lines)| (path.to_string(), lines))
            .collect();
        let count = |added, removed| Some(LineCount { added, removed });
        assert_eq!(
            counts,
            [
                ("src/a.rs".to_owned(), count(3, 1)),
                ("image.png".to_owned(), None),
                ("new name.txt".to_owned(), count(0, 0)),
                ("b".to_owned(), count(12, 0)),
            ]
        );
    }

    #[test]
    fn parses_name_status() {
        let raw = b"M\0src/a.rs\0R092\0old name.txt\0new name.txt\0A\0b\0D\0c\0";
        let changes = parse_name_status(raw);
        assert_eq!(changes.len(), 4);
        assert_eq!(changes[0].kind, ChangeKind::Modified);
        assert_eq!(changes[1].kind, ChangeKind::Renamed);
        assert_eq!(
            changes[1]
                .old_path
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
            Some("old name.txt")
        );
        assert_eq!(changes[1].path, "new name.txt");
        assert_eq!(changes[3].kind, ChangeKind::Deleted);
    }
}
