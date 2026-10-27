//! Local branch operations through the git CLI: create, rename, delete, switch.
//!
//! Git's own safety checks stay in charge: deleting unmerged work needs `force`, and a
//! switch that would overwrite local changes is refused without touching anything.

use std::ffi::OsStr;
use std::path::Path;

use crate::cancel::CancelToken;
use crate::cli::{GitCli, GitError};

/// Whether `name` is acceptable as a new branch name (`git check-ref-format --branch`).
pub fn is_valid_branch_name(
    git: &GitCli,
    workdir: &Path,
    name: &str,
    cancel: &CancelToken,
) -> bool {
    // `--branch` also expands `@{-N}` to the branch checked out N switches ago (and
    // succeeds); only a name that comes back unchanged is a name of its own. (A plain
    // `@` is a valid branch name to git, so it is to us.)
    !name.is_empty()
        && !name.starts_with('-')
        && git
            .command(workdir)
            .args(["check-ref-format", "--branch", name])
            .read_only()
            .run(cancel)
            .is_ok_and(|output| output.stdout_str().trim_end() == name)
}

/// Creates `name` at `start` (any revision; HEAD when `None`), optionally switching to
/// it. Starting from a remote-tracking branch sets it as upstream (git's default).
pub fn create_branch(
    git: &GitCli,
    workdir: &Path,
    name: &str,
    start: Option<&str>,
    checkout: bool,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    let mut command = if checkout {
        git.command(workdir).args(["switch", "--create", name])
    } else {
        git.command(workdir).args(["branch", name])
    };
    if let Some(start) = start {
        command = command.arg(start);
    }
    command.run(cancel)?;
    Ok(())
}

/// Renames `old` (as git names it: bytes, since a branch's name needn't be UTF-8) to
/// `new` (typed, so text).
pub fn rename_branch(
    git: &GitCli,
    workdir: &Path,
    old: impl AsRef<OsStr>,
    new: &str,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    git.command(workdir)
        .args(["branch", "--move"])
        .arg(old)
        .arg(new)
        .run(cancel)?;
    Ok(())
}

/// Deletes a local branch (its name as bytes). Without `force`, git refuses when the
/// branch has commits that are in neither its upstream nor HEAD
/// ([`crate::GitErrorKind::NotFullyMerged`]).
pub fn delete_branch(
    git: &GitCli,
    workdir: &Path,
    name: impl AsRef<OsStr>,
    force: bool,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    let flag = if force { "-D" } else { "-d" };
    git.command(workdir)
        .args(["branch", flag])
        .arg(name)
        .run(cancel)?;
    Ok(())
}

/// The `branch.<name>.*` keys and values of the repository's own config (`--local`: where
/// `git branch` writes them and `git branch -d` removes them), in git's order. A value
/// that isn't UTF-8 comes as [`config_value_text`] keeps it (an earlier version left it
/// out, and undo wrote the config back without it); a key without a value reads as `true`,
/// as git does.
pub fn branch_config(
    git: &GitCli,
    workdir: &Path,
    name: &str,
    cancel: &CancelToken,
) -> Result<Vec<(String, String)>, GitError> {
    let output = git
        .command(workdir)
        .args(["config", "--local", "--null", "--list"])
        .read_only()
        .run(cancel)?;
    let prefix = format!("branch.{name}.");
    Ok(output
        .stdout
        .split(|&byte| byte == 0)
        .filter_map(|item| {
            let (key, value) = match item.iter().position(|&byte| byte == b'\n') {
                Some(newline) => (&item[..newline], &item[newline + 1..]),
                None => (item, &b"true"[..]),
            };
            let key = std::str::from_utf8(key).ok()?;
            // `branch.a.remote` is branch a's, `branch.a.b.remote` branch a.b's.
            let variable = key.strip_prefix(&prefix)?;
            (!variable.is_empty() && !variable.contains('.'))
                .then(|| (key.to_owned(), config_value_text(value)))
        })
        .collect())
}

/// A config value as text for the undo log: itself when it's UTF-8, else a NUL (which no
/// config value holds) and its bytes in hex, so undo writes it back byte for byte.
pub fn config_value_text(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => std::iter::once("\0".to_owned())
            .chain(bytes.iter().map(|byte| format!("{byte:02x}")))
            .collect(),
    }
}

/// The bytes of a value [`config_value_text`] gave.
pub fn config_value_bytes(text: &str) -> Vec<u8> {
    match text.strip_prefix('\0') {
        Some(hex) => hex
            .as_bytes()
            .chunks(2)
            .filter_map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
            .collect(),
        None => text.as_bytes().to_vec(),
    }
}

/// Replaces the branch's `branch.<name>.*` config with `entries` (none: removes it), as
/// [`branch_config`] gives them. The new values are written before the keys they don't
/// have go, so a failure halfway leaves the old config, partly updated, rather than none
/// (an earlier version removed the section first, then added each key).
pub fn set_branch_config(
    git: &GitCli,
    workdir: &Path,
    name: &str,
    entries: &[(String, String)],
    cancel: &CancelToken,
) -> Result<(), GitError> {
    use std::os::unix::ffi::OsStrExt as _;
    if entries.is_empty() {
        // Exit 128: there was no such section.
        git.command(workdir)
            .args(["config", "--local", "--remove-section"])
            .arg(format!("branch.{name}"))
            .allow_exit_codes(&[128])
            .run(cancel)?;
        return Ok(());
    }
    let current = branch_config(git, workdir, name, cancel)?;
    if current == entries {
        return Ok(());
    }
    // A key's first value replaces all it had; its other values are added after it.
    let mut written: Vec<&str> = Vec::new();
    for (key, value) in entries {
        let first = !written.contains(&key.as_str());
        git.command(workdir)
            .args(["config", "--local"])
            .arg(if first { "--replace-all" } else { "--add" })
            .arg(key)
            .arg(std::ffi::OsStr::from_bytes(&config_value_bytes(value)))
            .run(cancel)?;
        if first {
            written.push(key);
        }
    }
    // Then the keys `entries` doesn't have. Exit 5: gone already.
    for (key, _) in &current {
        if !written.contains(&key.as_str()) {
            git.command(workdir)
                .args(["config", "--local", "--unset-all", key])
                .allow_exit_codes(&[5])
                .run(cancel)?;
            written.push(key);
        }
    }
    Ok(())
}

/// Switches to a local branch (its name as bytes: git refuses a lossy one). Local
/// changes that don't conflict come along; ones the switch would overwrite make git
/// refuse ([`crate::GitErrorKind::WouldOverwrite`]).
pub fn switch_branch(
    git: &GitCli,
    workdir: &Path,
    name: impl AsRef<OsStr>,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    git.command(workdir).arg("switch").arg(name).run(cancel)?;
    Ok(())
}

/// Checks out a remote-tracking branch (`origin/feature`) as a local branch that tracks
/// it (`feature`), or switches to that local branch if it already exists. The branch is
/// named by its bytes, which needn't be UTF-8. Returns the local branch name as text
/// to show.
pub fn switch_to_remote_branch(
    git: &GitCli,
    workdir: &Path,
    remote: &str,
    remote_branch: impl AsRef<OsStr>,
    local_exists: bool,
    cancel: &CancelToken,
) -> Result<String, GitError> {
    let local = remote_branch.as_ref();
    if local_exists {
        switch_branch(git, workdir, local, cancel)?;
    } else {
        let mut upstream = std::ffi::OsString::from(format!("{remote}/"));
        upstream.push(local);
        git.command(workdir)
            .args(["switch", "--create"])
            .arg(local)
            .arg("--track")
            .arg(upstream)
            .run(cancel)?;
    }
    Ok(local.to_string_lossy().into_owned())
}

/// Checks out a commit without a branch (detached HEAD).
pub fn switch_detached(
    git: &GitCli,
    workdir: &Path,
    commit: &str,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    git.command(workdir)
        .args(["switch", "--detach", commit])
        .run(cancel)?;
    Ok(())
}
