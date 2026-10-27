//! Git notes: text attached to a commit under a notes ref, outside its history. Review
//! sessions write their Markdown summary to [`REVIEW_NOTES_REF`] on the reviewed head.
//!
//! `notes_ref` is taken the way `git notes --ref` takes it: a name that doesn't start with
//! `refs/notes/` gets that prefix (`review` → `refs/notes/review`, `notes/x` →
//! `refs/notes/x`, even `refs/heads/main` → `refs/notes/refs/heads/main`), so a note never
//! lands on a branch or a tag. `--ref` also overrides `GIT_NOTES_REF` and `core.notesRef`.

use std::path::Path;

use gix::ObjectId;

use crate::cancel::CancelToken;
use crate::cli::{GitCli, GitCommand, GitError, classify};

/// Where review summaries go.
pub const REVIEW_NOTES_REF: &str = "refs/notes/review";

/// Attaches `text` to `commit` under `notes_ref`, replacing a note it already has there.
///
/// The text is stored exactly as given. `git notes add -m/-F` would clean it up like a
/// commit message (trailing whitespace goes, runs of blank lines collapse: Markdown's
/// two-space line breaks and spacing), and `--no-stripspace` is recent. So the text is
/// written as a blob first (`git hash-object -w`) and the note reuses that blob (`-C`),
/// which every git copies verbatim. An empty text removes the note, as git does.
pub fn add_note(
    git: &GitCli,
    workdir: &Path,
    notes_ref: &str,
    commit: ObjectId,
    text: &str,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    if text.is_empty() {
        return remove_note(git, workdir, notes_ref, commit, cancel);
    }
    let blob = git
        .command(workdir)
        .args(["hash-object", "-w", "--no-filters", "--stdin"])
        .stdin(text.as_bytes().to_vec())
        .run(cancel)?;
    let blob = blob.stdout_str().trim().to_owned();
    notes(git, workdir, notes_ref)
        .args(["add", "-f", "-C", &blob])
        .arg(commit.to_string())
        .run(cancel)?;
    Ok(())
}

/// The note `commit` has under `notes_ref`, exactly as stored (decoded lossily: notes are
/// text). `None` when it has none there, or the notes ref doesn't exist.
pub fn read_note(
    git: &GitCli,
    workdir: &Path,
    notes_ref: &str,
    commit: ObjectId,
    cancel: &CancelToken,
) -> Result<Option<String>, GitError> {
    let command = notes(git, workdir, notes_ref)
        .arg("show")
        .arg(commit.to_string())
        .read_only()
        .allow_exit_codes(&[1]);
    let display = command.display();
    let output = command.run(cancel)?;
    if output.status == 0 {
        return Ok(Some(output.stdout_str().into_owned()));
    }
    // `error: no note found for object <id>.`
    if output.stderr.contains("no note found") {
        return Ok(None);
    }
    Err(GitError {
        kind: classify(&output.stderr, ""),
        command: display,
        stderr: output.stderr,
        exit_code: Some(output.status),
    })
}

/// Removes `commit`'s note under `notes_ref`; nothing happens when it has none.
pub fn remove_note(
    git: &GitCli,
    workdir: &Path,
    notes_ref: &str,
    commit: ObjectId,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    notes(git, workdir, notes_ref)
        .args(["remove", "--ignore-missing"])
        .arg(commit.to_string())
        .run(cancel)?;
    Ok(())
}

/// `git notes --ref=<notes_ref>`: the attached form, so a name starting with `-` is a name.
fn notes<'a>(git: &'a GitCli, workdir: &Path, notes_ref: &str) -> GitCommand<'a> {
    git.command(workdir)
        .arg("notes")
        .arg(format!("--ref={notes_ref}"))
}
