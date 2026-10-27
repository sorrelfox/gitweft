//! Merging, and the operation a repository is in the middle of.
//!
//! * [`merge_preview`] answers "would this conflict?" without touching the working tree
//!   or index (`git merge-tree --write-tree`), for the confirmation before a merge.
//! * [`merge`] runs `git merge` (fast-forward, merge commit, fast-forward only, squash).
//!   Conflicts leave the merge in progress.
//! * [`operation_state`] reports a merge, rebase, cherry-pick or revert in progress, even
//!   one started in a terminal, and [`run_operation_action`] continues, skips or aborts it.
//! * [`prepared_message`] is the commit message git prepared when a merge, cherry-pick,
//!   revert or rebase stopped at a conflict (`MERGE_MSG`), to prefill the commit message,
//!   and [`prepared_conflicts`] the files it lists as conflicted at that point. The
//!   message comes without git's comment lines: git strips them only from a message it
//!   opened an editor for, not from one given with `--file` as [`crate::ops::commit`] does.

use std::path::Path;

use gix::bstr::BString;

use crate::cancel::CancelToken;
use crate::cli::{GitCli, GitError, GitErrorKind, GitVersion};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MergeMode {
    /// Fast-forward when possible, otherwise a merge commit (git's default, `merge.ff`).
    #[default]
    Default,
    /// Always a merge commit (`--no-ff`).
    NoFastForward,
    FastForwardOnly,
    /// All changes staged as one future commit; no merge is recorded (`--squash`).
    Squash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeOutcome {
    UpToDate,
    FastForwarded,
    Merged,
    /// `--squash`: the changes are staged; committing them is up to the user.
    Squashed,
}

/// Merges `revision` into the current branch. A conflict stops with the merge in
/// progress ([`GitErrorKind::Conflict`]): resolve, then continue or abort it.
pub fn merge(
    git: &GitCli,
    workdir: &Path,
    revision: impl AsRef<std::ffi::OsStr>,
    mode: MergeMode,
    cancel: &CancelToken,
) -> Result<MergeOutcome, GitError> {
    let mut command = git.command(workdir).args(["merge", "--no-edit"]);
    command = match mode {
        MergeMode::Default => command,
        MergeMode::NoFastForward => command.arg("--no-ff"),
        MergeMode::FastForwardOnly => command.arg("--ff-only"),
        MergeMode::Squash => command.arg("--squash"),
    };
    let output = command.arg(revision).run(cancel)?;
    let text = output.stdout_str();
    Ok(if text.contains("Already up to date") {
        MergeOutcome::UpToDate
    } else if mode == MergeMode::Squash {
        MergeOutcome::Squashed
    } else if text.contains("Fast-forward") {
        MergeOutcome::FastForwarded
    } else {
        MergeOutcome::Merged
    })
}

/// The name to hand `git merge` for the branch `full_name` (`refs/heads/x`,
/// `refs/remotes/origin/x`): the short name when it can only mean that branch, so git's
/// message reads "Merge branch 'x'" rather than naming the full ref. Git resolves a short
/// name as a tag before a local branch, and a local branch before a remote-tracking one
/// (gitrevisions), so a name taken there keeps the full, unambiguous form.
pub fn merge_revision(full_name: &str, tags: &[&str], local_branches: &[&str]) -> String {
    let short = if let Some(name) = full_name.strip_prefix("refs/heads/") {
        (!tags.contains(&name)).then_some(name)
    } else if let Some(name) = full_name.strip_prefix("refs/remotes/") {
        (!tags.contains(&name) && !local_branches.contains(&name)).then_some(name)
    } else {
        None
    };
    short.unwrap_or(full_name).to_owned()
}

/// What merging `theirs` into `ours` would do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergePreview {
    /// Paths that would conflict, as bytes (a name need be neither UTF-8 nor free of tabs,
    /// quotes or newlines); empty for a clean merge.
    pub conflicts: Vec<BString>,
}

impl MergePreview {
    pub fn is_clean(&self) -> bool {
        self.conflicts.is_empty()
    }
}

/// Computes the merge in memory (`git merge-tree --write-tree`): nothing in the working
/// tree, index or refs changes (merged objects may be written to the object database).
pub fn merge_preview(
    git: &GitCli,
    workdir: &Path,
    ours: &str,
    theirs: impl AsRef<std::ffi::OsStr>,
    cancel: &CancelToken,
) -> Result<MergePreview, GitError> {
    // `theirs` as bytes: a branch's name needn't be UTF-8.
    let output = git
        .command(workdir)
        .args([
            "merge-tree",
            "--write-tree",
            "--name-only",
            "--no-messages",
            "-z",
            ours,
        ])
        .arg(theirs)
        .read_only()
        .allow_exit_codes(&[1])
        .run(cancel)?;
    // NUL-terminated (`-z`), so names come as they are, not C-quoted: the merged tree
    // first, then with conflicts (exit status 1) each conflicted path, then an empty entry.
    let mut conflicts: Vec<BString> = if output.status == 1 {
        output
            .stdout
            .split(|byte| *byte == 0)
            .skip(1)
            .take_while(|path| !path.is_empty())
            .map(BString::from)
            .collect()
    } else {
        Vec::new()
    };
    conflicts.dedup();
    Ok(MergePreview { conflicts })
}

/// An operation the repository is in the middle of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Merge,
    Rebase,
    CherryPick,
    Revert,
    /// `git am` applying patches.
    ApplyMailbox,
    Bisect,
}

impl Operation {
    /// Whether the current step can be skipped (`--skip`).
    pub fn can_skip(self) -> bool {
        matches!(
            self,
            Operation::Rebase | Operation::CherryPick | Operation::Revert | Operation::ApplyMailbox
        )
    }

    /// Whether it can be continued (bisect only ends with a reset).
    pub fn can_continue(self) -> bool {
        self != Operation::Bisect
    }
}

/// The operation in progress, whoever started it.
pub fn operation_state(repo: &gix::Repository) -> Option<Operation> {
    use gix::state::InProgress;
    Some(match repo.state()? {
        InProgress::Merge => Operation::Merge,
        InProgress::Rebase | InProgress::RebaseInteractive | InProgress::ApplyMailboxRebase => {
            Operation::Rebase
        }
        InProgress::CherryPick | InProgress::CherryPickSequence => Operation::CherryPick,
        InProgress::Revert | InProgress::RevertSequence => Operation::Revert,
        InProgress::ApplyMailbox => Operation::ApplyMailbox,
        InProgress::Bisect => Operation::Bisect,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationAction {
    /// Carry on once conflicts are resolved and staged (the prepared message is kept).
    Continue,
    Skip,
    /// Go back to where the operation started.
    Abort,
}

/// Continues, skips or aborts the operation in progress. Continuing with unresolved
/// conflicts fails with [`GitErrorKind::UnresolvedConflicts`]; nothing changes then.
pub fn run_operation_action(
    git: &GitCli,
    workdir: &Path,
    operation: Operation,
    action: OperationAction,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    let unsupported = || {
        GitError::new(
            GitErrorKind::Other,
            "git",
            format!("{action:?} is not possible for {operation:?}"),
        )
    };
    let command = match (operation, action) {
        (Operation::Bisect, OperationAction::Abort) => {
            git.command(workdir).args(["bisect", "reset"])
        }
        (Operation::Bisect, _) => return Err(unsupported()),
        (_, OperationAction::Skip) if !operation.can_skip() => return Err(unsupported()),
        _ => {
            let verb = match operation {
                Operation::Merge => "merge",
                Operation::Rebase => "rebase",
                Operation::CherryPick => "cherry-pick",
                Operation::Revert => "revert",
                Operation::ApplyMailbox => "am",
                Operation::Bisect => unreachable!("handled above"),
            };
            let flag = match action {
                OperationAction::Continue => "--continue",
                OperationAction::Skip => "--skip",
                OperationAction::Abort => "--abort",
            };
            git.command(workdir).args([verb, flag])
        }
    };
    command.run(cancel)?;
    Ok(())
}

/// The commit message git prepared for the operation in progress (`<git dir>/MERGE_MSG`),
/// with its comment lines removed (see [`comment_prefix`]); `None` when there's no such
/// file or nothing but comments and blank lines is left.
///
/// Git writes it when a merge, cherry-pick, revert or rebase stops at a conflict: "Merge
/// branch 'x'" or the picked commit's message, then the conflicted paths as comments. A
/// squash merge's own text is in `SQUASH_MSG`: its `MERGE_MSG` holds only the comments.
/// A linked worktree has its own, in its own git dir. `git` is the version that wrote it.
pub fn prepared_message(repo: &gix::Repository, git: GitVersion) -> Option<String> {
    let bytes = read_prepared(repo)?;
    let prefix = comment_prefix(repo, git);
    let message = strip_comments(&String::from_utf8_lossy(&bytes), &prefix);
    (!message.is_empty()).then_some(message)
}

/// The files git listed under "Conflicts:" in the prepared message (`<git dir>/MERGE_MSG`)
/// when the operation stopped: every file that conflicted at this step, including those
/// resolved since. Empty when there's no such file or no such list.
///
/// In index order (by bytes), as git wrote them: raw, not quoted. A newline in a name
/// breaks git's format (it comments out the rest of the name): that name is cut there,
/// and the list ends with it.
pub fn prepared_conflicts(repo: &gix::Repository, git: GitVersion) -> Vec<BString> {
    read_prepared(repo)
        .map(|bytes| conflicts_block(&bytes, &comment_prefix(repo, git)))
        .unwrap_or_default()
}

fn read_prepared(repo: &gix::Repository) -> Option<Vec<u8>> {
    std::fs::read(repo.git_dir().join("MERGE_MSG")).ok()
}

/// The paths of the `<prefix> Conflicts:` block in a prepared message: the
/// `<prefix>\t<path>` lines right after that line, up to the first other one. Git appends
/// its block last, so one further up is part of the message itself and doesn't count.
fn conflicts_block(text: &[u8], prefix: &str) -> Vec<BString> {
    let mut paths = Vec::new();
    let mut in_block = false;
    for line in text.split(|&byte| byte == b'\n') {
        let comment = line.strip_prefix(prefix.as_bytes());
        if comment == Some(b" Conflicts:".as_slice()) {
            paths.clear();
            in_block = true;
            continue;
        }
        match comment.and_then(|comment| comment.strip_prefix(b"\t")) {
            Some(path) if in_block => paths.push(BString::from(path)),
            _ => in_block = false,
        }
    }
    paths
}

/// The first git with `core.commentString` and longer comment prefixes.
const COMMENT_STRINGS: GitVersion = GitVersion::new(2, 45, 0);

/// The prefix git starts comment lines with in commit messages: `core.commentChar` or
/// `core.commentString`, whichever is set last (they are one setting under two names, so
/// neither outranks the other), else `#`. `auto` (git picks a character not used in the
/// message) and empty values also give `#`: merges, cherry-picks and the like write `#`
/// with `auto`, and git refuses to run with an empty value.
///
/// Git before 2.45 (`git`) knows only `core.commentChar`, and only one character: it
/// ignores `core.commentString` and refuses to run with a longer `core.commentChar`.
pub fn comment_prefix(repo: &gix::Repository, git: GitVersion) -> String {
    let strings = git >= COMMENT_STRINGS;
    let config = repo.config_snapshot();
    config
        .plumbing()
        .sections_by_name("core")
        .into_iter()
        .flatten()
        // `[core "x"]` sections hold other settings.
        .filter(|section| section.header().subsection_name().is_none())
        // In the order git reads them: files by precedence, each from the top.
        .flat_map(|section| section.body())
        .filter(|(name, _)| {
            name.eq_ignore_ascii_case("commentChar")
                || strings && name.eq_ignore_ascii_case("commentString")
        })
        .last()
        .map(|(_, value)| value)
        // Git refuses to run with an empty value or a newline in it.
        .filter(|value| {
            !value.is_empty() && !value.eq_ignore_ascii_case(b"auto") && !value.contains(&b'\n')
        })
        .filter(|value| strings || value.len() == 1)
        .map_or_else(
            || "#".to_owned(),
            |value| String::from_utf8_lossy(&value).into_owned(),
        )
}

/// `text` without the lines that start with `prefix`, without trailing whitespace on each
/// line, and without leading and trailing blank lines. Runs of blank lines in between
/// collapse into one (as `git commit --cleanup=strip` does).
///
/// Only a line that starts with `prefix` itself is a comment (an indented one is text),
/// and a dropped comment doesn't count as a blank line. An empty `prefix` marks no
/// comments, like `--cleanup=whitespace`.
pub fn strip_comments(text: &str, prefix: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_before = false;
    for line in text.lines() {
        if !prefix.is_empty() && line.starts_with(prefix) {
            continue;
        }
        // Git's whitespace is only these (and the newline).
        let line = line.trim_end_matches([' ', '\t', '\r']);
        if line.is_empty() {
            blank_before = !out.is_empty();
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blank_before { "\n\n" } else { "\n" });
        }
        out.push_str(line);
        blank_before = false;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{conflicts_block, merge_revision, strip_comments};

    #[test]
    fn short_names_when_unambiguous() {
        assert_eq!(
            merge_revision("refs/heads/feature/x", &[], &[]),
            "feature/x"
        );
        assert_eq!(
            merge_revision("refs/remotes/origin/main", &["v1"], &["main"]),
            "origin/main"
        );
    }

    #[test]
    fn full_names_when_another_ref_would_win() {
        // A tag beats a local branch of the same name.
        assert_eq!(
            merge_revision("refs/heads/v1", &["v1"], &["v1"]),
            "refs/heads/v1"
        );
        // A local branch called "origin/x" beats the remote-tracking branch.
        assert_eq!(
            merge_revision("refs/remotes/origin/x", &[], &["origin/x"]),
            "refs/remotes/origin/x"
        );
        // Anything that isn't a branch is left alone.
        assert_eq!(merge_revision("refs/tags/v1", &[], &[]), "refs/tags/v1");
    }

    #[test]
    fn comment_lines_are_dropped() {
        // What git writes when a merge stops at conflicts.
        let prepared = "Merge branch 'feature'\n\n# Conflicts:\n#\ta.txt\n#\tsrc/b.rs\n";
        assert_eq!(strip_comments(prepared, "#"), "Merge branch 'feature'");
        // A squash merge's: nothing but comments.
        assert_eq!(strip_comments("\n# Conflicts:\n#\ta.txt\n", "#"), "");
        assert_eq!(strip_comments("", "#"), "");
    }

    #[test]
    fn any_prefix_marks_comments() {
        let text = "Subject\n\n; Conflicts:\n;\ta.txt\n# not a comment now\n";
        assert_eq!(strip_comments(text, ";"), "Subject\n\n# not a comment now");
        let text = "Subject\n// gone\n//\tgone too\n/ kept\n";
        assert_eq!(strip_comments(text, "//"), "Subject\n/ kept");
    }

    #[test]
    fn only_a_prefix_at_the_start_of_a_line_marks_a_comment() {
        let text = "Fix #12\n  # indented\n\t#tabbed\nsee C#";
        assert_eq!(strip_comments(text, "#"), text);
        assert_eq!(
            strip_comments("# kept \n\n", ""),
            "# kept",
            "no prefix, no comments"
        );
    }

    #[test]
    fn whitespace_is_cleaned_up_as_git_does() {
        // Trailing whitespace (a CRLF's CR included), leading and trailing blank lines.
        let text = "\n \nSubject \t\r\n\r\nBody  \n\n\n";
        assert_eq!(strip_comments(text, "#"), "Subject\n\nBody");
        // Runs of blank lines become one, also around a dropped comment...
        assert_eq!(
            strip_comments("a\n\n\n \n\t\nb\n\n# c\n\nc", "#"),
            "a\n\nb\n\nc"
        );
        // ...which isn't a blank line itself.
        assert_eq!(strip_comments("a\n# c\nb", "#"), "a\nb");
    }

    #[test]
    fn the_conflicts_block_lists_paths_as_git_wrote_them() {
        let text = b"Merge branch 'feature'\n\n# Conflicts:\n#\ta.txt\n#\tsrc/b c.rs\n";
        assert_eq!(conflicts_block(text, "#"), ["a.txt", "src/b c.rs"]);
        assert_eq!(
            conflicts_block(b"Pick\n\n; Conflicts:\n;\ta.txt\n", ";"),
            ["a.txt"]
        );
        // Git puts a space after any prefix before "Conflicts:", even after "// ".
        assert_eq!(
            conflicts_block(b"x\n\n// Conflicts:\n//\ta.txt\n", "//"),
            ["a.txt"]
        );
        assert_eq!(
            conflicts_block(b"x\n\n//  Conflicts:\n// \ta.txt\n", "// "),
            ["a.txt"]
        );
        // Names are bytes, unquoted.
        let text = b"x\n\n# Conflicts:\n#\tcaf\xe9.txt\n#\ttab\there\n";
        assert_eq!(
            conflicts_block(text, "#"),
            [&b"caf\xe9.txt"[..], &b"tab\there"[..]]
        );
    }

    #[test]
    fn no_conflicts_block_no_paths() {
        assert!(conflicts_block(b"Merge branch 'feature'\n", "#").is_empty());
        assert!(conflicts_block(b"", "#").is_empty());
        // Under another prefix it's text.
        assert!(conflicts_block(b"x\n\n# Conflicts:\n#\ta.txt\n", ";").is_empty());
    }

    #[test]
    fn the_conflicts_block_ends_at_the_first_other_line() {
        let text = b"x\n\n# Conflicts:\n#\ta.txt\n#\n# a note\n#\tnot a path\n";
        assert_eq!(conflicts_block(text, "#"), ["a.txt"]);
        // With `commit.cleanup=scissors`, git's cut line comes first.
        let text = b"x\n\n# ------------------------ >8 ------------------------\n\
            # Do not modify or remove the line above.\n# Everything below it will be ignored.\n\
            #\n# Conflicts:\n#\ta.txt\n";
        assert_eq!(conflicts_block(text, "#"), ["a.txt"]);
        // Git's block comes last: one kept in the picked commit's message doesn't count.
        let text = b"Pick\n\n# Conflicts:\n#\told.txt\n\n# Conflicts:\n#\ta.txt\n";
        assert_eq!(conflicts_block(text, "#"), ["a.txt"]);
        // A newline in a name: git comments out the rest of it, which ends the list.
        let text = b"x\n\n# Conflicts:\n#\ta.txt\n#\tnew\n# line.txt\n#\tz.txt\n";
        assert_eq!(conflicts_block(text, "#"), ["a.txt", "new"]);
    }
}
