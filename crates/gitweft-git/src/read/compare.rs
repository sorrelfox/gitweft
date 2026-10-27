//! How two refs differ: the commits each has that the other lacks, and the files the
//! compared ref changed since the two diverged (what a pull request would show).

use std::path::Path;

use gix::ObjectId;

use super::ReadError;
use super::details::{FileChange, Signature, parse_raw};
use crate::cancel::CancelToken;
use crate::cli::{GitCli, GitError, GitErrorKind};

/// Ends each commit in the log output (the fields are NUL-separated).
const RECORD: u8 = 0x1e;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComparedCommit {
    pub id: ObjectId,
    pub author: Signature,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comparison {
    pub base: ObjectId,
    pub head: ObjectId,
    /// `None` for unrelated histories.
    pub merge_base: Option<ObjectId>,
    /// Commits in `head` but not in `base`, newest first, at most `limit` of them.
    pub ahead: Vec<ComparedCommit>,
    /// Commits in `base` but not in `head`, newest first, at most `limit` of them.
    pub behind: Vec<ComparedCommit>,
    /// The full counts (the lists may be capped).
    pub ahead_count: usize,
    pub behind_count: usize,
    /// Changes from the merge base to `head` (from `base` when the histories are unrelated).
    pub files: Vec<FileChange>,
    /// For each of `files`, its blob in `head` (for a deletion, the one it removed): what
    /// a review's "viewed" mark is tied to.
    pub blobs: Vec<ObjectId>,
}

/// Compares `head` with `base` (any revisions: branch names, tags, ids).
pub fn compare(
    git: &GitCli,
    cwd: &Path,
    base: &str,
    head: &str,
    limit: usize,
    cancel: &CancelToken,
) -> Result<Comparison, ReadError> {
    let run = |args: &[&str]| git.command(cwd).args(args).read_only().run(cancel);
    // `--end-of-options`: a ref named like an option is still a ref.
    let resolve = |rev: &str| -> Result<ObjectId, ReadError> {
        let spec = format!("{rev}^{{commit}}");
        let output = run(&["rev-parse", "--verify", "--end-of-options", &spec])?;
        Ok(ObjectId::from_hex(output.stdout_str().trim().as_bytes())?)
    };
    let (base, head) = (resolve(base)?, resolve(head)?);
    let (base_hex, head_hex) = (base.to_string(), head.to_string());

    let merge_base = merge_base(git, cwd, base, head, cancel)?;

    let counts = run(&[
        "rev-list",
        "--left-right",
        "--count",
        &format!("{base_hex}...{head_hex}"),
    ])?;
    let counts = counts.stdout_str();
    let mut counts = counts.split_whitespace().map(str::parse::<usize>);
    let behind_count = counts.next().ok_or("unexpected `git rev-list` output")??;
    let ahead_count = counts.next().ok_or("unexpected `git rev-list` output")??;

    let max_count = format!("--max-count={limit}");
    let commits = |range: String| -> Result<Vec<ComparedCommit>, ReadError> {
        let output = run(&[
            "log",
            "--no-show-signature",
            "--format=%H%x00%an%x00%ae%x00%at%x00%s%x1e",
            &max_count,
            &range,
            "--",
        ])?;
        parse_commits(&output.stdout)
    };
    let ahead = commits(format!("{base_hex}..{head_hex}"))?;
    let behind = commits(format!("{head_hex}..{base_hex}"))?;

    let from = merge_base.unwrap_or(base);
    let (files, blobs) = changed_files(git, cwd, &from, &head, cancel)?;

    Ok(Comparison {
        base,
        head,
        merge_base,
        ahead,
        behind,
        ahead_count,
        behind_count,
        files,
        blobs,
    })
}

/// The best common ancestor of `a` and `b` (`git merge-base`); `None` when the histories
/// are unrelated.
pub fn merge_base(
    git: &GitCli,
    cwd: &Path,
    a: ObjectId,
    b: ObjectId,
    cancel: &CancelToken,
) -> Result<Option<ObjectId>, ReadError> {
    // Exit code 1 means there is no merge base: unrelated histories.
    let found = git
        .command(cwd)
        .arg("merge-base")
        .arg(a.to_string())
        .arg(b.to_string())
        .allow_exit_codes(&[1])
        .read_only()
        .run(cancel)?;
    Ok(match found.status {
        0 => Some(ObjectId::from_hex(found.stdout_str().trim().as_bytes())?),
        _ => None,
    })
}

/// The files `to` changed since `from`, each with its blob in `to` (for a deletion, the
/// one it removed).
pub fn changed_files(
    git: &GitCli,
    cwd: &Path,
    from: &ObjectId,
    to: &ObjectId,
    cancel: &CancelToken,
) -> Result<(Vec<FileChange>, Vec<ObjectId>), ReadError> {
    // `--raw` also gives each file's blob ids; `--no-abbrev` keeps them whole.
    let diff = git
        .command(cwd)
        .args(["diff", "--raw", "--no-abbrev", "-z", "-M", "--no-ext-diff"])
        .arg(from.to_string())
        .arg(to.to_string())
        .arg("--")
        .read_only()
        .run(cancel)
        .map_err(|err| super::diff::explain_refused_diff(git, cwd, err, cancel))?;
    Ok(parse_raw(&diff.stdout).into_iter().unzip())
}

/// Whether `ancestor` is in `descendant`'s history (a commit is its own ancestor).
/// An unknown commit is an error.
pub fn is_ancestor(
    git: &GitCli,
    cwd: &Path,
    ancestor: &ObjectId,
    descendant: &ObjectId,
    cancel: &CancelToken,
) -> Result<bool, ReadError> {
    // Exit code 1 means "not an ancestor"; anything else but 0 is a failure.
    let output = git
        .command(cwd)
        .args(["merge-base", "--is-ancestor"])
        .arg(ancestor.to_string())
        .arg(descendant.to_string())
        .allow_exit_codes(&[1])
        .read_only()
        .run(cancel)?;
    Ok(output.status == 0)
}

/// How many commits `to` has that `from` doesn't (`git rev-list --count from..to`).
pub fn count_commits(
    git: &GitCli,
    cwd: &Path,
    from: &ObjectId,
    to: &ObjectId,
    cancel: &CancelToken,
) -> Result<usize, ReadError> {
    let output = git
        .command(cwd)
        .args(["rev-list", "--count"])
        .arg(format!("{from}..{to}"))
        .arg("--")
        .read_only()
        .run(cancel)?;
    Ok(output.stdout_str().trim().parse()?)
}

/// How many merge commits `to` has that `from` doesn't
/// (`git rev-list --count --merges from..to`), with the arguments meaning what they do in
/// [`count_commits`]. Revisions are passed after `--end-of-options`.
pub fn count_merges(
    git: &GitCli,
    cwd: &Path,
    from: &str,
    to: &str,
    cancel: &CancelToken,
) -> Result<usize, GitError> {
    let output = git
        .command(cwd)
        .args(["rev-list", "--count", "--merges", "--end-of-options"])
        .arg(format!("{from}..{to}"))
        .arg("--")
        .read_only()
        .run(cancel)?;
    let stdout = output.stdout_str();
    let count = stdout.trim();
    count.parse().map_err(|_| {
        GitError::new(
            GitErrorKind::Other,
            "git rev-list --count --merges",
            format!("unexpected output: {count}"),
        )
    })
}

fn parse_commits(stdout: &[u8]) -> Result<Vec<ComparedCommit>, ReadError> {
    let lossy = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
    let mut commits = Vec::new();
    for record in stdout.split(|b| *b == RECORD) {
        let record = record.strip_prefix(b"\n").unwrap_or(record);
        if record.is_empty() {
            continue;
        }
        let mut fields = record.splitn(5, |b| *b == 0);
        let mut next = || fields.next().ok_or("unexpected `git log` output");
        let id = ObjectId::from_hex(next()?)?;
        let name = lossy(next()?);
        let email = lossy(next()?);
        let time = std::str::from_utf8(next()?)?.trim().parse()?;
        let summary = lossy(next()?);
        commits.push(ComparedCommit {
            id,
            author: Signature { name, email, time },
            summary,
        });
    }
    Ok(commits)
}
