//! Classification of git failures into kinds the UI can act on.
//!
//! Git runs with `LC_MESSAGES=C`, so the English messages matched here are stable.

/// What went wrong, in terms the UI can offer a remedy for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitErrorKind {
    GitMissing,
    GitTooOld,
    NotARepo,
    DubiousOwnership,
    /// A `*.lock` file exists, e.g. from a crashed git process.
    LockHeld {
        path: String,
    },
    /// Local changes would be overwritten by checkout/merge.
    WouldOverwrite,
    /// `git branch -d` refused: the branch has commits that aren't merged anywhere.
    NotFullyMerged,
    /// A merge, rebase, cherry-pick or revert can't continue: conflicts are unresolved.
    UnresolvedConflicts,
    NonFastForward,
    /// `git pull` found commits on both sides and no configured way to combine them
    /// (or `--ff-only` could not fast-forward).
    DivergentBranches,
    RemoteRejected,
    AuthFailed,
    HostKeyUnknown,
    Network,
    HookFailed,
    Conflict,
    NothingToCommit,
    /// git doesn't know who you are: no `user.name`/`user.email` and nothing it may guess
    /// them from (a commit, merge or rebase needs both).
    IdentityUnknown,
    /// Undo or redo refused: a ref it would move has moved since (see `undo`).
    RefMoved,
    /// `git submodule deinit` refused: the submodule's work tree has local changes
    /// (modified files, or merely untracked ones). Forcing discards them.
    SubmoduleModified,
    /// The LFS server has no file-locking API (a `file://` remote). Set by the LFS lock
    /// calls in `lfs`, not by [`classify`]: git-lfs says so only in a hint, and lists
    /// the locks of such a remote as none, successfully.
    LfsLockingUnsupported,
    /// An interactive rebase's commits include `count` merges, which its plans can't make
    /// again (set by `rebase`, not by [`classify`]). `after`: the newest merge that only
    /// straight commits follow, where a plan could start instead; `None` when HEAD itself
    /// is a merge.
    MergesInRange {
        count: usize,
        after: Option<gix::ObjectId>,
    },
    /// The commit an interactive rebase would start after isn't in HEAD's history (set by
    /// `rebase`).
    NotInHistory,
    /// No commits follow the one an interactive rebase would start after (set by `rebase`).
    NothingToRebase,
    /// A partial clone lacks objects a read needed, and read-only runs may not fetch them
    /// on their own (`GIT_NO_LAZY_FETCH`): `partial::fetch_missing` gets them on request.
    MissingObjects,
    Cancelled,
    Other,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{}", self.summary())]
pub struct GitError {
    pub kind: GitErrorKind,
    /// Command line as shown in the activity log (`git push origin main`).
    pub command: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
}

impl GitError {
    pub fn new(kind: GitErrorKind, command: impl Into<String>, stderr: impl Into<String>) -> Self {
        Self {
            kind,
            command: command.into(),
            stderr: stderr.into(),
            exit_code: None,
        }
    }

    /// The most informative single line: git's `fatal:`/`error:` message if any, with what
    /// it lists on the lines below (see [`with_listed`]). A hook that failed speaks for
    /// itself: its last line, not git's "failed to push some refs".
    pub fn summary(&self) -> String {
        let lines = || self.stderr.lines().map(str::trim).filter(|l| !l.is_empty());
        let from_git = |l: &&str| {
            ["error:", "fatal:", "hint:"]
                .iter()
                .any(|p| l.starts_with(p))
        };
        if self.kind == GitErrorKind::HookFailed
            && let Some(line) = lines().rfind(|l| !from_git(l))
        {
            return line.to_owned();
        }
        let raw: Vec<&str> = self.stderr.lines().collect();
        if let Some(at) = raw.iter().position(|l| {
            let l = l.trim();
            l.starts_with("fatal:") || l.starts_with("error:")
        }) {
            return with_listed(raw[at].trim(), &raw[at + 1..]);
        }
        lines()
            .next_back()
            .map(|l| l.to_owned())
            .unwrap_or_else(|| match self.exit_code {
                Some(code) => format!("`{}` failed with exit code {code}", self.command),
                None => format!("`{}` failed", self.command),
            })
    }
}

/// How many of a message's listed items [`with_listed`] names before it counts the rest.
const LISTED_NAMED: usize = 3;

/// `line` with what it introduces, when it ends in a colon: git lists the files a checkout
/// or merge would overwrite on the indented lines `after` it, and the line alone names none
/// ("…would be overwritten by merge:"). The first few join the line, the rest are counted:
/// "…by merge: a.txt, b.txt, c.txt (+2)".
fn with_listed(line: &str, after: &[&str]) -> String {
    if !line.ends_with(':') {
        return line.to_owned();
    }
    let listed: Vec<&str> = after
        .iter()
        .take_while(|l| l.starts_with([' ', '\t']))
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    if listed.is_empty() {
        return line.to_owned();
    }
    let named = listed.len().min(LISTED_NAMED);
    let mut text = format!("{line} {}", listed[..named].join(", "));
    if listed.len() > named {
        text.push_str(&format!(" (+{})", listed.len() - named));
    }
    text
}

/// Maps git's output of a failed command to an error kind.
pub fn classify(stderr: &str, stdout: &str) -> GitErrorKind {
    let has = |needle: &str| stderr.contains(needle);

    // A read-only run in a partial clone, with lazy fetching off; `git blame` says it
    // its own way ("Cannot read blob <id> for path <path>").
    if has("lazy fetching disabled")
        || (has("could not fetch") && has("from promisor remote"))
        || has("Cannot read blob")
    {
        return GitErrorKind::MissingObjects;
    }
    if has("detected dubious ownership") {
        return GitErrorKind::DubiousOwnership;
    }
    if has("not a git repository") {
        return GitErrorKind::NotARepo;
    }
    if let Some(path) = lock_path(stderr) {
        return GitErrorKind::LockHeld { path };
    }
    if has("would be overwritten by") {
        return GitErrorKind::WouldOverwrite;
    }
    // `git submodule deinit` without `-f`.
    if has("contains local modifications; use '-f' to discard them") {
        return GitErrorKind::SubmoduleModified;
    }
    if has("is not fully merged") {
        return GitErrorKind::NotFullyMerged;
    }
    if has("Host key verification failed") {
        return GitErrorKind::HostKeyUnknown;
    }
    if has("Authentication failed")
        || has("Permission denied (publickey")
        || has("could not read Username")
        || has("could not read Password")
        || has("terminal prompts disabled")
    {
        return GitErrorKind::AuthFailed;
    }
    if has("[remote rejected]") || has("hook declined") {
        return GitErrorKind::RemoteRejected;
    }
    if has("divergent branches") || has("Not possible to fast-forward") {
        return GitErrorKind::DivergentBranches;
    }
    // A force-with-lease refusal is a non-fast-forward too: the remote moved on.
    if has("(non-fast-forward)")
        || has("(fetch first)")
        || has("(stale info)")
        || has("(remote ref updated since checkout)")
        || has("Updates were rejected because")
    {
        return GitErrorKind::NonFastForward;
    }
    if has("Could not resolve host")
        || has("Connection refused")
        || has("Connection timed out")
        || has("Network is unreachable")
        || has("Could not read from remote repository")
        || has("unable to access '")
        // git-lfs is written in Go, whose network errors are lowercase:
        // `dial tcp: lookup lfs.example: no such host`, `connect: connection refused`.
        || has("no such host")
        || has("connection refused")
        || has("dial tcp")
    {
        return GitErrorKind::Network;
    }
    // The first two head git's advice block, the rest are the `fatal:` line that ends it
    // (or stands alone, for an empty name given through the environment).
    if has("Author identity unknown")
        || has("Committer identity unknown")
        || has("Please tell me who you are")
        || has("empty ident name")
        || has("unable to auto-detect email address")
    {
        return GitErrorKind::IdentityUnknown;
    }
    // Continuing before every conflict is resolved (and staged).
    if has("you have unmerged files")
        || has("unresolved conflict")
        || has("You must edit all merge conflicts")
    {
        return GitErrorKind::UnresolvedConflicts;
    }
    // Merge, stash apply and cherry-pick report conflicts on stdout, rebase on stderr.
    let reports_conflict = |text: &str| {
        text.contains("CONFLICT (")
            || text.contains("Automatic merge failed")
            || text.contains("could not apply")
    };
    if reports_conflict(stderr) || reports_conflict(stdout) {
        return GitErrorKind::Conflict;
    }
    // The last: changes exist, but none is staged.
    if stdout.contains("nothing to commit")
        || stdout.contains("nothing added to commit")
        || stdout.contains("no changes added to commit")
    {
        return GitErrorKind::NothingToCommit;
    }
    GitErrorKind::Other
}

/// Extracts the path from `Unable to create '/repo/.git/index.lock': File exists.`
fn lock_path(stderr: &str) -> Option<String> {
    let start = stderr.find("Unable to create '")? + "Unable to create '".len();
    let rest = &stderr[start..];
    let end = rest.find("': File exists")?;
    Some(rest[..end].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_known_failures() {
        let cases: &[(&str, GitErrorKind)] = &[
            (
                "fatal: not a git repository (or any of the parent directories): .git",
                GitErrorKind::NotARepo,
            ),
            (
                "fatal: detected dubious ownership in repository at '/mnt/usb/repo'",
                GitErrorKind::DubiousOwnership,
            ),
            (
                "fatal: Unable to create '/r/.git/index.lock': File exists.\n\nAnother git process seems to be running",
                GitErrorKind::LockHeld {
                    path: "/r/.git/index.lock".into(),
                },
            ),
            (
                "error: Your local changes to the following files would be overwritten by checkout:\n\ta.txt",
                GitErrorKind::WouldOverwrite,
            ),
            (
                " ! [rejected]        main -> main (fetch first)\nerror: failed to push some refs",
                GitErrorKind::NonFastForward,
            ),
            (
                " ! [remote rejected] main -> main (pre-receive hook declined)",
                GitErrorKind::RemoteRejected,
            ),
            (
                "git@host: Permission denied (publickey).\nfatal: Could not read from remote repository.",
                GitErrorKind::AuthFailed,
            ),
            (
                "Host key verification failed.\nfatal: Could not read from remote repository.",
                GitErrorKind::HostKeyUnknown,
            ),
            (
                "fatal: unable to access 'https://git.example/x.git/': Could not resolve host: git.example",
                GitErrorKind::Network,
            ),
            (
                "Fetching reference refs/heads/main\n\
                 batch response: Post \"https://lfs.example.invalid/x/objects/batch\": \
                 dial tcp: lookup lfs.example.invalid: no such host\n\
                 error: failed to fetch some objects from 'https://lfs.example.invalid/x'",
                GitErrorKind::Network,
            ),
            (
                "CONFLICT (content): Merge conflict in a.txt\nAutomatic merge failed; fix conflicts",
                GitErrorKind::Conflict,
            ),
            (
                "hint: You have divergent branches and need to specify how to reconcile them.\n\
                 fatal: Need to specify how to reconcile divergent branches.",
                GitErrorKind::DivergentBranches,
            ),
            (
                "fatal: Not possible to fast-forward, aborting.",
                GitErrorKind::DivergentBranches,
            ),
            (
                " ! [rejected]        main -> main (stale info)",
                GitErrorKind::NonFastForward,
            ),
            (
                "error: the branch 'wip' is not fully merged\n\
                 hint: If you are sure you want to delete it, run 'git branch -D wip'",
                GitErrorKind::NotFullyMerged,
            ),
            (
                "error: Committing is not possible because you have unmerged files.\n\
                 fatal: Exiting because of an unresolved conflict.",
                GitErrorKind::UnresolvedConflicts,
            ),
            // git 2.55, `git commit` with no identity anywhere and an unqualified hostname.
            (
                "Author identity unknown\n\n\
                 *** Please tell me who you are.\n\n\
                 Run\n\n  \
                 git config --global user.email \"you@example.com\"\n  \
                 git config --global user.name \"Your Name\"\n\n\
                 to set your account's default identity.\n\
                 Omit --global to set the identity only in this repository.\n\n\
                 fatal: unable to auto-detect email address (got 'user@workstation.(none)')",
                GitErrorKind::IdentityUnknown,
            ),
            // The same with `user.useConfigOnly=true`.
            (
                "Author identity unknown\n\n\
                 *** Please tell me who you are.\n\n\
                 fatal: no email was given and auto-detection is disabled",
                GitErrorKind::IdentityUnknown,
            ),
            (
                "Committer identity unknown\n\n\
                 *** Please tell me who you are.\n\n\
                 fatal: no name was given and auto-detection is disabled",
                GitErrorKind::IdentityUnknown,
            ),
            (
                "fatal: empty ident name (for <a@example.invalid>) not allowed",
                GitErrorKind::IdentityUnknown,
            ),
            (
                "fatal: unable to auto-detect email address (got 'user@workstation.(none)')",
                GitErrorKind::IdentityUnknown,
            ),
            // git 2.55, `git submodule deinit` with a modified or untracked file inside.
            (
                "error: the following file has local modifications:\n    lib\n\
                 (use --cached to keep the file, or -f to force removal)\n\
                 fatal: Submodule work tree 'lib' contains local modifications; use '-f' to \
                 discard them",
                GitErrorKind::SubmoduleModified,
            ),
            ("fatal: something odd", GitErrorKind::Other),
        ];
        for (stderr, expected) in cases {
            assert_eq!(&classify(stderr, ""), expected, "stderr: {stderr}");
        }
        assert_eq!(
            classify(
                "",
                "On branch main\nnothing to commit, working tree clean\n"
            ),
            GitErrorKind::NothingToCommit
        );
        assert_eq!(
            classify(
                "",
                "Changes not staged for commit:\n\tmodified:   a.txt\n\nno changes added to \
                 commit (use \"git add\" and/or \"git commit -a\")\n"
            ),
            GitErrorKind::NothingToCommit
        );
        // `git merge` and `git stash pop` print their conflicts on stdout.
        assert_eq!(
            classify(
                "",
                "Auto-merging a.txt\nCONFLICT (content): Merge conflict in a.txt\n"
            ),
            GitErrorKind::Conflict
        );
    }

    #[test]
    fn summary_prefers_fatal_line() {
        let err = GitError::new(
            GitErrorKind::Other,
            "git push",
            "To origin\n ! [rejected] main\nerror: failed to push some refs to 'origin'\nhint: ...\n",
        );
        assert_eq!(err.summary(), "error: failed to push some refs to 'origin'");
        let err = GitError {
            exit_code: Some(128),
            ..GitError::new(GitErrorKind::Other, "git x", "")
        };
        assert_eq!(err.summary(), "`git x` failed with exit code 128");
    }

    #[test]
    fn a_summary_names_what_its_line_lists() {
        // git 2.56's `git stash apply` over a local change to the same file.
        let err = GitError::new(
            GitErrorKind::WouldOverwrite,
            "git stash apply",
            "error: Your local changes to the following files would be overwritten by merge:\n\
             \tnotes.txt\n\
             Please commit your changes or stash them before you merge.\n\
             Aborting\n",
        );
        assert_eq!(
            err.summary(),
            "error: Your local changes to the following files would be overwritten by merge: \
             notes.txt"
        );
        // Past three files, the rest are counted.
        let many = GitError::new(
            GitErrorKind::WouldOverwrite,
            "git checkout main",
            "error: The following untracked working tree files would be overwritten by checkout:\n\
             \ta.txt\n\tb.txt\n\tc.txt\n\td.txt\n\te.txt\n\
             Please move or remove them before you switch branches.\n\
             Aborting\n",
        );
        assert_eq!(
            many.summary(),
            "error: The following untracked working tree files would be overwritten by checkout: \
             a.txt, b.txt, c.txt (+2)"
        );
        // A colon with nothing indented after it stays as it is.
        let bare = GitError::new(
            GitErrorKind::Other,
            "git x",
            "fatal: unable to read:\nsomething else\n",
        );
        assert_eq!(bare.summary(), "fatal: unable to read:");
    }

    #[test]
    fn a_failed_hooks_summary_is_its_own_last_line() {
        // git 2.55's `git push` when the pre-push hook exits 1.
        let err = GitError::new(
            GitErrorKind::HookFailed,
            "git push",
            "checking…\nno pushing on Fridays\nerror: failed to push some refs to '../r.git'\n",
        );
        assert_eq!(err.summary(), "no pushing on Fridays");
        // A hook that printed nothing: git's line after all.
        let silent = GitError::new(
            GitErrorKind::HookFailed,
            "git push",
            "error: failed to push some refs to '../r.git'\n",
        );
        assert_eq!(
            silent.summary(),
            "error: failed to push some refs to '../r.git'"
        );
    }
}
