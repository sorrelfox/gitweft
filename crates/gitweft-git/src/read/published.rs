//! Which commits an operation would take off the current branch although a remote
//! already has them, for the warning before rewriting published history.

use std::collections::HashSet;
use std::path::Path;

use crate::cancel::CancelToken;
use crate::cli::{GitCli, GitError};

/// Commits that leave the branch although a remote-tracking branch has them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Published {
    pub count: usize,
    /// The remote-tracking branches holding the newest of them (`origin/main`), sorted.
    pub remote_branches: Vec<String>,
}

impl Published {
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

/// The commits reachable from `tip` but not from `keep` (the branch before and after a
/// rewrite; `None` counts everything `tip` reaches) that some remote-tracking branch
/// reaches too. Both are revisions of our own making (ids, `HEAD`), never user text.
pub fn published_commits(
    git: &GitCli,
    workdir: &Path,
    tip: impl AsRef<std::ffi::OsStr>,
    keep: Option<&std::ffi::OsStr>,
    cancel: &CancelToken,
) -> Result<Published, GitError> {
    // Names as bytes: a branch's name needn't be UTF-8 (#18).
    let excluded = keep.map(|keep| {
        let mut arg = std::ffi::OsString::from("^");
        arg.push(keep);
        arg
    });
    let rev_list = |extra: &[&str]| -> Result<String, GitError> {
        let output = git
            .command(workdir)
            .arg("rev-list")
            .args(extra)
            .arg(tip.as_ref())
            .args(excluded.as_deref())
            .arg("--")
            .read_only()
            .run(cancel)?;
        Ok(output.stdout_str().into_owned())
    };
    // Counts first: a branch's whole history is never held as ids (#25).
    let count = |extra: &[&str]| -> Result<usize, GitError> {
        let mut args = vec!["--count"];
        args.extend_from_slice(extra);
        Ok(rev_list(&args)?.trim().parse().unwrap_or(0))
    };
    let dropped = count(&[])?;
    if dropped == 0 {
        return Ok(Published::default());
    }
    // `--not --remotes` goes after the revisions it applies to.
    let unpublished_count = {
        let output = git
            .command(workdir)
            .args(["rev-list", "--count"])
            .arg(tip.as_ref())
            .args(excluded.as_deref())
            .args(["--not", "--remotes", "--"])
            .read_only()
            .run(cancel)?;
        output.stdout_str().trim().parse::<usize>().unwrap_or(0)
    };
    let published = dropped.saturating_sub(unpublished_count);
    if published == 0 {
        return Ok(Published::default());
    }
    // The newest published commit: among the first unpublished+1 dropped ones at least one
    // is published, so only those and the unpublished ids are read.
    let unpublished: HashSet<String> = {
        let output = git
            .command(workdir)
            .arg("rev-list")
            .arg(tip.as_ref())
            .args(excluded.as_deref())
            .args(["--not", "--remotes", "--"])
            .read_only()
            .run(cancel)?;
        output.stdout_str().lines().map(str::to_owned).collect()
    };
    let first = rev_list(&[&format!("--max-count={}", unpublished_count + 1)])?;
    let Some(newest) = first
        .lines()
        .find(|id| !unpublished.contains(*id))
        .map(str::to_owned)
    else {
        return Ok(Published::default());
    };
    let output = git
        .command(workdir)
        .args([
            "for-each-ref",
            "--format=%(refname:lstrip=2)",
            "--contains",
            newest.as_str(),
            "refs/remotes/",
        ])
        .read_only()
        .run(cancel)?;
    // `origin/HEAD` only points at another remote branch.
    let mut remote_branches: Vec<String> = output
        .stdout_str()
        .lines()
        .filter(|name| !name.is_empty() && !name.ends_with("/HEAD"))
        .map(str::to_owned)
        .collect();
    remote_branches.sort();
    Ok(Published {
        count: published,
        remote_branches,
    })
}
