//! Tags: create (lightweight or annotated), delete, and publish or remove on a remote.

use std::path::Path;

use crate::cancel::CancelToken;
use crate::cli::{GitCli, GitError, GitOutput, StderrEvent};

/// Whether `name` can be a tag name (`git check-ref-format refs/tags/<name>`).
pub fn is_valid_tag_name(git: &GitCli, workdir: &Path, name: &str, cancel: &CancelToken) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && git
            .command(workdir)
            .args(["check-ref-format"])
            .arg(format!("refs/tags/{name}"))
            .read_only()
            .run(cancel)
            .is_ok()
}

/// Tags `target` (any revision). With a message the tag is annotated (and signed if
/// `tag.gpgSign` says so), otherwise lightweight.
pub fn create_tag(
    git: &GitCli,
    workdir: &Path,
    name: &str,
    target: &str,
    message: Option<&str>,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    let command = match message.filter(|m| !m.trim().is_empty()) {
        Some(message) => git
            .command(workdir)
            .args(["tag", "--annotate", "--file=-", name, target])
            .stdin(message.as_bytes().to_vec()),
        None => git.command(workdir).args(["tag", name, target]),
    };
    command.run(cancel)?;
    Ok(())
}

pub fn delete_tag(
    git: &GitCli,
    workdir: &Path,
    name: &str,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    git.command(workdir)
        .args(["tag", "--delete", name])
        .run(cancel)?;
    Ok(())
}

/// Publishes one tag.
pub fn push_tag(
    git: &GitCli,
    workdir: &Path,
    remote: &str,
    name: &str,
    cancel: &CancelToken,
    on_event: impl FnMut(StderrEvent),
) -> Result<GitOutput, GitError> {
    git.command(workdir)
        .args(["push", "--progress", remote])
        .arg(format!("refs/tags/{name}"))
        .run_with_events(cancel, on_event)
}

/// Deletes a tag on a remote (the local tag stays).
pub fn delete_remote_tag(
    git: &GitCli,
    workdir: &Path,
    remote: &str,
    name: &str,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    git.command(workdir)
        .args(["push", remote, "--delete"])
        .arg(format!("refs/tags/{name}"))
        .run(cancel)?;
    Ok(())
}
