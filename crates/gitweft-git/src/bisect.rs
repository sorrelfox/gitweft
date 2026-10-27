//! `git bisect`: start, mark, reset, and where a bisect stands, through the git CLI.
//!
//! The state is derived from the repository, never from the text of the last command, so
//! it is right after a restart and when the user bisects in a terminal:
//! - a bisect is running while `BISECT_START` exists in the (per-worktree) git dir;
//! - the marks are the refs `refs/bisect/<bad>`, `refs/bisect/<good>-<id>` and
//!   `refs/bisect/skip-<id>`, with the terms from `BISECT_TERMS` (`bad`/`good` unless the
//!   bisect was started with `--term-new`/`--term-old`);
//! - what's left is `git rev-list --bisect-all --bisect-vars <bad> --not <good>…`, with the
//!   limits the bisect was started with (`--first-parent`, the pathspecs in `BISECT_NAMES`).
//!   Its counts are the ones `git bisect` prints. rev-list knows nothing about skipped
//!   commits, so they are set aside here: when every remaining candidate but the bad commit
//!   is skipped, only skipped commits are left; when there is no other candidate at all,
//!   the bad commit is the first bad one. Git keeps no other record of the result than
//!   `refs/bisect/bad`, and leaves HEAD where the last test was;
//! - when a good commit isn't an ancestor of the bad one, git first tests a merge base of
//!   the two (outside the range rev-list counts), until it has checked them all and wrote
//!   `BISECT_ANCESTORS_OK`.
//!
//! Pathspecs given to [`bisect_start`] are stored with `:(literal)` magic in
//! `BISECT_NAMES`, which every later git command re-reads (ours and a terminal's alike), so
//! a path named `*.txt` means that file only. `GIT_LITERAL_PATHSPECS` would not last: git
//! stores the pathspecs as given, and a `git bisect good` without it globs them. So the
//! commands that read `BISECT_NAMES` run with `GIT_LITERAL_PATHSPECS=0`, which the magic
//! needs.
//!
//! `git bisect` prints its progress on stdout and its errors on stderr; a failed checkout
//! of the next commit (local changes in the way) is a [`GitErrorKind::WouldOverwrite`].
//! Revisions starting with `-` are refused before git runs.
//!
//! [`bisect_run`] lets git test by itself, with a shell command (`git bisect run`).

use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::OsString;
use std::io::ErrorKind;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use gix::ObjectId;

use crate::cancel::CancelToken;
use crate::cli::{GitCli, GitCommand, GitError, GitErrorKind, StderrEvent, classify};

/// How a tested commit is marked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BisectMark {
    /// Before the change (`git bisect good`, or the bisect's own term for it).
    Good,
    /// After the change (`git bisect bad`, or the bisect's own term for it).
    Bad,
    /// Can't be tested (`git bisect skip`).
    Skip,
}

/// Where a bisect stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BisectState {
    /// The commit marked bad (`refs/bisect/bad`), once there is one.
    pub bad: Option<ObjectId>,
    /// Commits marked good (`refs/bisect/good-*`), in ref-name order.
    pub good: Vec<ObjectId>,
    /// Commits skipped (`refs/bisect/skip-*`), in ref-name order.
    pub skipped: Vec<ObjectId>,
    /// The paths the bisect is limited to (`git bisect start … -- <paths>`, from
    /// `BISECT_NAMES`): only commits touching them are tested. Empty for the whole history.
    /// Read back as git stored them, without the `:(literal)` magic [`bisect_start`] adds.
    pub paths: Vec<PathBuf>,
    /// What's next.
    pub progress: BisectProgress,
}

/// What's next in a bisect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BisectProgress {
    /// Started, but git still needs a bad and/or a good commit.
    NeedsMarks { needs_bad: bool, needs_good: bool },
    /// Testing the checked-out commit.
    Testing {
        /// The checked-out commit (`BISECT_HEAD` in a `--no-checkout` bisect): the one a
        /// mark without a revision applies to.
        commit: ObjectId,
        /// The commit git chose to test. It differs from `commit` when git couldn't check
        /// it out (local changes in the way) or the user checked out another one.
        suggested: ObjectId,
        /// Revisions left to test after this one, as `git bisect` prints them.
        revisions_left: usize,
        /// Roughly this many steps left.
        steps_left: usize,
    },
    /// Testing the checked-out commit, a merge base of the bad commit and a good one. Git
    /// tests it before bisecting because that good commit isn't an ancestor of the bad one
    /// (git: "Bisecting: a merge base must be tested"). The merge base is outside the range
    /// git counts, so there are no counts for it. Good, Bad and Skip apply to it as usual;
    /// marked bad, it means the change came between it and the good commit, and git stops
    /// (the state is then [`BisectProgress::Contradictory`]).
    MergeBase { commit: ObjectId },
    /// Done: this is the first bad commit.
    Found { first_bad: ObjectId },
    /// Only skipped commits are left: the first bad commit is one of these (newest first;
    /// the bad commit is among them).
    OnlySkipped { candidates: Vec<ObjectId> },
    /// No commit is left between the good and the bad ones: a good commit comes after the
    /// bad one (git: "Some good revs are not ancestors of the bad rev"), or, when bisecting
    /// some paths only, no commit in the range touches them. Mark another commit bad, or
    /// start over.
    Contradictory,
}

/// Where the bisect in `workdir` stands; `None` when none is running (no `BISECT_START` in
/// the git dir).
pub fn bisect_state(
    git: &GitCli,
    workdir: &Path,
    cancel: &CancelToken,
) -> Result<Option<BisectState>, GitError> {
    match running_bisect(git, workdir, cancel)? {
        Some(bisect) => bisect.state(git, workdir, cancel).map(Some),
        None => Ok(None),
    }
}

/// Starts bisecting, like `git bisect start [<bad> [<good>…]] -- <paths>…`: git checks out
/// the commit to test once it has a bad and a good one. Good commits without a bad one are
/// marked right after the start. A bisect already running is ended first (git does that).
///
/// With `paths`, only commits touching them are tested (git keeps them in
/// `BISECT_NAMES`). They are relative to `workdir` and taken literally: `*.txt` is the file
/// of that name, `-x` a file too. An empty path is refused before git runs.
///
/// With local changes git starts only if the checkout doesn't touch them; otherwise it
/// fails with [`GitErrorKind::WouldOverwrite`] and no bisect is left behind.
pub fn bisect_start(
    git: &GitCli,
    workdir: &Path,
    bad: Option<&str>,
    good: &[String],
    paths: &[PathBuf],
    cancel: &CancelToken,
) -> Result<BisectState, GitError> {
    for revision in bad.into_iter().chain(good.iter().map(String::as_str)) {
        refuse_option_like(revision)?;
    }
    if paths.iter().any(|path| path.as_os_str().is_empty()) {
        // With the magic it would mean the whole tree.
        return Err(GitError::new(
            GitErrorKind::Other,
            "git bisect",
            "fatal: empty string is not a valid pathspec",
        ));
    }
    let mut command = bisect_command(git, workdir).args(["bisect", "start"]);
    if let Some(bad) = bad {
        command = command.arg(bad).args(good);
    }
    // Without `--`, git would take a mistyped revision that happens to name a file for a
    // pathspec; with it, the revision is an error.
    command
        .arg("--")
        .args(paths.iter().map(|path| literal_pathspec(path)))
        .run(cancel)?;
    if bad.is_none() && !good.is_empty() {
        // A new bisect has the default terms.
        bisect_command(git, workdir)
            .args(["bisect", "good"])
            .args(good)
            .run(cancel)?;
    }
    bisect_state(git, workdir, cancel)?.ok_or_else(not_bisecting)
}

/// Marks `commit` (the checked-out one when `None`), and lets git check out the next one to
/// test. When only skipped commits are left, that is the returned state, not an error.
///
/// If local changes are in the way of the next checkout, this fails with
/// [`GitErrorKind::WouldOverwrite`], but the mark is kept: the state then shows the
/// checked-out commit and, as `suggested`, the one git wanted.
pub fn bisect_mark(
    git: &GitCli,
    workdir: &Path,
    mark: BisectMark,
    commit: Option<&str>,
    cancel: &CancelToken,
) -> Result<BisectState, GitError> {
    if let Some(commit) = commit {
        refuse_option_like(commit)?;
    }
    // Git refuses too, but leaves a `BISECT_TERMS` file behind.
    let bisect = running_bisect(git, workdir, cancel)?.ok_or_else(not_bisecting)?;
    let verb = match mark {
        BisectMark::Good => bisect.terms.good.as_str(),
        BisectMark::Bad => bisect.terms.bad.as_str(),
        BisectMark::Skip => "skip",
    };
    // Failures are handled here: some messages are on stdout only (`<id> was both 'good'
    // and 'bad'`; with 3, `The merge base <id> is bad.`), and the runner keeps just stderr.
    let output = bisect_command(git, workdir)
        .args(["bisect", verb])
        .args(commit)
        .allow_exit_codes(&[1, 2, 3])
        .run(cancel)?;
    // Git exits with 2 when only skipped commits are left: an answer, not a failure.
    let answered = |state: &BisectState| {
        output.status == 0
            || output.status == 2 && matches!(state.progress, BisectProgress::OnlySkipped { .. })
    };
    let state = bisect.state(git, workdir, cancel)?;
    if !answered(&state) {
        let stdout = output.stdout_str();
        return Err(GitError {
            kind: classify(&output.stderr, &stdout),
            command: format!("git bisect {verb}"),
            stderr: format!("{stdout}{}", output.stderr),
            exit_code: Some(output.status),
        });
    }
    Ok(state)
}

/// Ends the bisect and checks out where it started (`git bisect reset`). Nothing happens
/// when no bisect is running. If local changes are in the way, this fails with
/// [`GitErrorKind::WouldOverwrite`] and the bisect keeps running.
pub fn bisect_reset(git: &GitCli, workdir: &Path, cancel: &CancelToken) -> Result<(), GitError> {
    git.command(workdir).args(["bisect", "reset"]).run(cancel)?;
    Ok(())
}

/// How a [`bisect_run`] ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BisectRunOutcome {
    /// The first bad commit, from git's `<id> is the first '<term>' commit` line (older
    /// git: `… is the first bad commit`). `None` when git couldn't narrow it down to one
    /// because only skipped commits are left (the command exited with 125 for them):
    /// `state` then lists the candidates.
    pub first_bad: Option<ObjectId>,
    /// Where the bisect stands afterwards. It is still running, HEAD at the last commit
    /// tested; [`bisect_reset`] ends it.
    pub state: BisectState,
}

/// Lets git find the first bad commit by itself (`git bisect run sh -c -- <command>`): at
/// each commit it checks out, git runs `command` with `sh`, in the top folder of the work
/// tree, and marks the commit by the exit code: 0 good, 125 skip, any other code below 128
/// bad. From 128 up (a crash, a signal) git stops with `error: bisect run failed: exit code
/// <n> …`, and when the command exits with 126 or 127 (can't run, not found) on the first
/// commit, git tries it on a good one and stops if it fails there too (`error: bogus exit
/// code 127 for 'good' revision`). Those failures, and a checkout that local changes get in
/// the way of ([`GitErrorKind::WouldOverwrite`]), are errors; the marks made so far stay.
///
/// Every line git and the command print reaches `on_line` as it comes: stdout (git's
/// `running …`, `Bisecting: …`, the result) and stderr, neither parsed as progress. The
/// two streams are separate pipes, so their lines interleave only roughly.
///
/// Refused before git runs when no bisect is running or it lacks a bad or a good commit
/// (git would exit 1 without a word), and for an empty command. Cancelling kills git, the
/// shell and whatever the command started (one process group); the bisect keeps running
/// where it was.
pub fn bisect_run(
    git: &GitCli,
    workdir: &Path,
    command: &str,
    cancel: &CancelToken,
    on_line: impl FnMut(&str),
) -> Result<BisectRunOutcome, GitError> {
    let refuse = |message: &str| GitError::new(GitErrorKind::Other, "git bisect run", message);
    if command.trim().is_empty() {
        return Err(refuse("fatal: no command to run"));
    }
    let before = bisect_state(git, workdir, cancel)?.ok_or_else(not_bisecting)?;
    if matches!(before.progress, BisectProgress::NeedsMarks { .. }) {
        return Err(refuse(
            "fatal: the bisect needs a bad and a good commit before it can run",
        ));
    }

    let sink = RefCell::new(RunSink {
        on_line,
        first_bad: None,
    });
    let mut stdout = LineSplitter::default();
    let output = bisect_command(git, workdir)
        // `--`: a command starting with `-` is still the command, not an option of sh.
        .args(["bisect", "run", "sh", "-c", "--", command])
        .plain_stderr()
        // 2: only skipped commits are left; 1: git's refusals. Told apart below.
        .allow_exit_codes(&[1, 2])
        .run_streaming_with_events(
            cancel,
            |event| {
                if let StderrEvent::Line(line) = event {
                    (sink.borrow_mut().on_line)(&line);
                }
            },
            |chunk| {
                for line in stdout.feed(chunk) {
                    sink.borrow_mut().stdout_line(&line);
                }
            },
        )?;
    let mut sink = sink.into_inner();
    if let Some(line) = stdout.finish() {
        sink.stdout_line(&line);
    }

    let state = bisect_state(git, workdir, cancel)?.ok_or_else(not_bisecting)?;
    match output.status {
        0 => {
            let found = match state.progress {
                BisectProgress::Found { first_bad } => Some(first_bad),
                _ => None,
            };
            Ok(BisectRunOutcome {
                first_bad: sink.first_bad.or(found),
                state,
            })
        }
        2 if matches!(state.progress, BisectProgress::OnlySkipped { .. }) => Ok(BisectRunOutcome {
            first_bad: None,
            state,
        }),
        status => Err(GitError {
            kind: classify(&output.stderr, ""),
            command: "git bisect run".to_owned(),
            stderr: output.stderr,
            exit_code: Some(status),
        }),
    }
}

/// Where [`bisect_run`]'s lines go.
struct RunSink<F> {
    on_line: F,
    first_bad: Option<ObjectId>,
}

impl<F: FnMut(&str)> RunSink<F> {
    fn stdout_line(&mut self, line: &str) {
        if let Some(id) = parse_first_bad(line) {
            self.first_bad = Some(id);
        }
        (self.on_line)(line);
    }
}

/// `<id> is the first '<term>' commit` (git 2.55; older: `<id> is the first bad commit`).
fn parse_first_bad(line: &str) -> Option<ObjectId> {
    let (hex, rest) = line.split_once(' ')?;
    (rest.starts_with("is the first ") && rest.ends_with(" commit"))
        .then(|| ObjectId::from_hex(hex.as_bytes()).ok())
        .flatten()
}

/// Splits a byte stream into `\n`-terminated lines (a `\r` before the `\n` dropped),
/// keeping empty ones.
#[derive(Debug, Default)]
struct LineSplitter {
    buf: Vec<u8>,
}

impl LineSplitter {
    fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut lines = Vec::new();
        while let Some(newline) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=newline).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            lines.push(String::from_utf8_lossy(&line).into_owned());
        }
        lines
    }

    /// The last line, when it had no `\n`.
    fn finish(&mut self) -> Option<String> {
        (!self.buf.is_empty())
            .then(|| String::from_utf8_lossy(&std::mem::take(&mut self.buf)).into_owned())
    }
}

/// A running bisect: its git dir and terms.
struct Bisect {
    git_dir: PathBuf,
    terms: Terms,
}

/// The names of the two marks (`git bisect start --term-new=<bad> --term-old=<good>`).
#[derive(Debug, PartialEq, Eq)]
struct Terms {
    bad: String,
    good: String,
}

impl Default for Terms {
    fn default() -> Self {
        Self {
            bad: "bad".into(),
            good: "good".into(),
        }
    }
}

/// The marks, from `refs/bisect/*`.
#[derive(Debug, Default, PartialEq, Eq)]
struct Marks {
    bad: Option<ObjectId>,
    good: Vec<ObjectId>,
    skipped: Vec<ObjectId>,
}

/// What `rev-list --bisect-all --bisect-vars` reports.
#[derive(Debug, Default, PartialEq, Eq)]
struct BisectWalk {
    /// Every commit between the good ones and the bad one (the bad one included), best
    /// first.
    candidates: Vec<ObjectId>,
    revisions_left: usize,
    steps_left: usize,
}

/// What's left once the skipped commits are set aside.
#[derive(Debug, PartialEq, Eq)]
enum Remaining {
    /// No candidate at all.
    Nothing,
    /// Some untested commit; the best one.
    Untested(ObjectId),
    /// Only skipped commits (and the bad one).
    OnlySkipped,
    /// Only the bad commit.
    Found,
}

fn running_bisect(
    git: &GitCli,
    workdir: &Path,
    cancel: &CancelToken,
) -> Result<Option<Bisect>, GitError> {
    let output = git
        .command(workdir)
        .args(["rev-parse", "--absolute-git-dir"])
        .read_only()
        .run(cancel)?;
    let mut dir = output.stdout;
    while dir.last() == Some(&b'\n') {
        dir.pop();
    }
    let git_dir = PathBuf::from(OsString::from_vec(dir));
    if !git_dir.join("BISECT_START").is_file() {
        return Ok(None);
    }
    let terms = read_file(&git_dir.join("BISECT_TERMS"))?
        .map(|bytes| parse_terms(&bytes))
        .unwrap_or_default();
    Ok(Some(Bisect { git_dir, terms }))
}

impl Bisect {
    fn state(
        &self,
        git: &GitCli,
        workdir: &Path,
        cancel: &CancelToken,
    ) -> Result<BisectState, GitError> {
        let refs = git
            .command(workdir)
            .args([
                "for-each-ref",
                "--format=%(objectname) %(refname)",
                "refs/bisect/",
            ])
            .read_only()
            .run(cancel)?;
        let marks = parse_marks(&refs.stdout_str(), &self.terms);
        let names = self.names()?;
        let progress = match marks.bad {
            Some(bad) if !marks.good.is_empty() => {
                self.progress(git, workdir, bad, &marks, &names, cancel)?
            }
            _ => BisectProgress::NeedsMarks {
                needs_bad: marks.bad.is_none(),
                needs_good: marks.good.is_empty(),
            },
        };
        Ok(BisectState {
            bad: marks.bad,
            good: marks.good,
            skipped: marks.skipped,
            paths: bisect_paths(&names),
            progress,
        })
    }

    /// The words of `BISECT_NAMES`: nothing for the whole history, else `--` and the
    /// pathspecs. Git itself goes on if it can't read them (just less precisely), and so do
    /// we.
    fn names(&self) -> Result<Vec<Vec<u8>>, GitError> {
        Ok(read_file(&self.git_dir.join("BISECT_NAMES"))?
            .and_then(|bytes| sq_dequote(&bytes))
            .unwrap_or_default())
    }

    fn progress(
        &self,
        git: &GitCli,
        workdir: &Path,
        bad: ObjectId,
        marks: &Marks,
        names: &[Vec<u8>],
        cancel: &CancelToken,
    ) -> Result<BisectProgress, GitError> {
        let range = self.range(bad, &marks.good, names);
        // rev-list exits with 1 (and prints nothing) when no commit is left.
        let output = bisect_command(git, workdir)
            .args(["rev-list", "--bisect-all", "--bisect-vars"])
            .args(&range)
            .allow_exit_codes(&[1])
            .read_only()
            .run(cancel)?;
        let walk = parse_bisect_walk(&output.stdout_str());
        let skipped: HashSet<ObjectId> = marks.skipped.iter().copied().collect();
        // Git writes the commit it checks out to BISECT_EXPECTED_REV, even when the checkout
        // then fails; once marked it's stale (e.g. marked in a terminal without a checkout
        // since).
        let marked = |id: &ObjectId| *id == bad || marks.good.contains(id) || skipped.contains(id);
        Ok(match remaining(bad, &walk.candidates, &skipped) {
            Remaining::Nothing => BisectProgress::Contradictory,
            Remaining::Found => {
                // Git may still test a merge base first, with nothing else left to bisect
                // (bad m5, good s3 off m4: m4 is tested, then m5 is the first bad commit).
                let (commit, expected) = if self.ancestors_checked() {
                    (None, None)
                } else {
                    checked_out(git, workdir, cancel)?
                };
                match commit {
                    Some(commit)
                        if expected == Some(commit)
                            && !marked(&commit)
                            && self.is_merge_base_to_test(
                                git,
                                workdir,
                                commit,
                                bad,
                                &marks.good,
                                &walk.candidates,
                                cancel,
                            )? =>
                    {
                        BisectProgress::MergeBase { commit }
                    }
                    _ => BisectProgress::Found { first_bad: bad },
                }
            }
            Remaining::OnlySkipped => {
                // Newest first, as a plain rev-list lists them.
                let output = bisect_command(git, workdir)
                    .arg("rev-list")
                    .args(&range)
                    .read_only()
                    .run(cancel)?;
                let candidates = output
                    .stdout_str()
                    .lines()
                    .filter_map(|line| ObjectId::from_hex(line.trim().as_bytes()).ok())
                    .filter(|id| *id == bad || skipped.contains(id))
                    .collect();
                BisectProgress::OnlySkipped { candidates }
            }
            Remaining::Untested(best) => {
                let (commit, expected) = checked_out(git, workdir, cancel)?;
                let commit = commit.ok_or_else(|| {
                    GitError::new(
                        GitErrorKind::Other,
                        "git bisect",
                        "fatal: HEAD does not point to a commit",
                    )
                })?;
                let suggested = expected.filter(|id| !marked(id)).unwrap_or(best);
                if commit == suggested
                    && self.is_merge_base_to_test(
                        git,
                        workdir,
                        commit,
                        bad,
                        &marks.good,
                        &walk.candidates,
                        cancel,
                    )?
                {
                    BisectProgress::MergeBase { commit }
                } else {
                    BisectProgress::Testing {
                        commit,
                        suggested,
                        revisions_left: walk.revisions_left,
                        steps_left: walk.steps_left,
                    }
                }
            }
        })
    }

    /// Whether git is done testing merge bases (`BISECT_ANCESTORS_OK`, written once every
    /// good commit is an ancestor of the bad one or a tested merge base is). Git doesn't
    /// look at them again after that, even for good commits marked later.
    fn ancestors_checked(&self) -> bool {
        self.git_dir.join("BISECT_ANCESTORS_OK").is_file()
    }

    /// Whether `commit`, checked out by git, is a merge base of the bad commit and a good
    /// one, which git tests before bisecting: outside the range (`candidates`), an ancestor
    /// of the bad commit and of a good one.
    #[allow(clippy::too_many_arguments)]
    fn is_merge_base_to_test(
        &self,
        git: &GitCli,
        workdir: &Path,
        commit: ObjectId,
        bad: ObjectId,
        good: &[ObjectId],
        candidates: &[ObjectId],
        cancel: &CancelToken,
    ) -> Result<bool, GitError> {
        if self.ancestors_checked()
            || candidates.contains(&commit)
            || !is_ancestor(git, workdir, commit, bad, cancel)?
        {
            return Ok(false);
        }
        for good in good {
            if is_ancestor(git, workdir, commit, *good, cancel)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The rev-list arguments for what's left to bisect, with the bisect's own limits:
    /// `[--first-parent] <bad> --not <good>… [-- <BISECT_NAMES words>…]`.
    fn range(&self, bad: ObjectId, good: &[ObjectId], names: &[Vec<u8>]) -> Vec<OsString> {
        let mut args = Vec::new();
        if self.git_dir.join("BISECT_FIRST_PARENT").is_file() {
            args.push("--first-parent".into());
        }
        args.push(bad.to_string().into());
        args.push("--not".into());
        args.extend(good.iter().map(|id| id.to_string().into()));
        if !names.is_empty() {
            // Exactly as git passes them, a `--` of their own included (`start -- <path>`):
            // the second `--` is a pathspec that matches nothing.
            args.push("--".into());
            args.extend(names.iter().map(|name| OsString::from_vec(name.clone())));
        }
        args
    }
}

/// The checked-out commit (`BISECT_HEAD` in a `--no-checkout` bisect, else `HEAD`) and
/// the one git last chose to test (`BISECT_EXPECTED_REV`).
fn checked_out(
    git: &GitCli,
    workdir: &Path,
    cancel: &CancelToken,
) -> Result<(Option<ObjectId>, Option<ObjectId>), GitError> {
    let [head, bisect_head, expected] = resolve(
        git,
        workdir,
        ["HEAD", "BISECT_HEAD", "BISECT_EXPECTED_REV"],
        cancel,
    )?;
    Ok((bisect_head.or(head), expected))
}

/// `git merge-base --is-ancestor <ancestor> <descendant>`: exit 1 is a no.
fn is_ancestor(
    git: &GitCli,
    workdir: &Path,
    ancestor: ObjectId,
    descendant: ObjectId,
    cancel: &CancelToken,
) -> Result<bool, GitError> {
    let output = git
        .command(workdir)
        .args(["merge-base", "--is-ancestor"])
        .arg(ancestor.to_string())
        .arg(descendant.to_string())
        .allow_exit_codes(&[1])
        .read_only()
        .run(cancel)?;
    Ok(output.status == 0)
}

/// Resolves each name to a commit id; `None` for names that don't exist.
fn resolve<const N: usize>(
    git: &GitCli,
    workdir: &Path,
    names: [&str; N],
    cancel: &CancelToken,
) -> Result<[Option<ObjectId>; N], GitError> {
    let mut stdin = names.join("\n");
    stdin.push('\n');
    let output = git
        .command(workdir)
        .args(["cat-file", "--batch-check=%(objectname)"])
        .stdin(stdin)
        .read_only()
        .run(cancel)?;
    // A missing name is reported as `<name> missing`.
    let text = output.stdout_str();
    let mut lines = text.lines();
    Ok(std::array::from_fn(|_| {
        lines
            .next()
            .and_then(|line| ObjectId::from_hex(line.trim().as_bytes()).ok())
    }))
}

/// Reads `refs/bisect/*` as `<id> <refname>` lines.
fn parse_marks(text: &str, terms: &Terms) -> Marks {
    let bad_ref = format!("refs/bisect/{}", terms.bad);
    let good_prefix = format!("refs/bisect/{}-", terms.good);
    let mut marks = Marks::default();
    for line in text.lines() {
        let Some((hex, name)) = line.split_once(' ') else {
            continue;
        };
        let Ok(id) = ObjectId::from_hex(hex.as_bytes()) else {
            continue;
        };
        if name == bad_ref {
            marks.bad = Some(id);
        } else if name.starts_with(&good_prefix) {
            marks.good.push(id);
        } else if name.starts_with("refs/bisect/skip-") {
            marks.skipped.push(id);
        }
    }
    marks
}

/// `BISECT_TERMS`: the bad term on the first line, the good one on the second.
fn parse_terms(bytes: &[u8]) -> Terms {
    let text = String::from_utf8_lossy(bytes);
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    match (lines.next(), lines.next()) {
        (Some(bad), Some(good)) => Terms {
            bad: bad.into(),
            good: good.into(),
        },
        _ => Terms::default(),
    }
}

/// Parses `rev-list --bisect-all --bisect-vars`: one `<id> (<decorations>, dist=<n>)` line
/// per candidate, best first, then `------`, then `bisect_<name>=<value>` lines.
fn parse_bisect_walk(text: &str) -> BisectWalk {
    let mut walk = BisectWalk::default();
    let mut listing = true;
    for line in text.lines() {
        if listing {
            if line == "------" {
                listing = false;
            } else if let Some(Ok(id)) = line
                .split(' ')
                .next()
                .map(|hex| ObjectId::from_hex(hex.as_bytes()))
            {
                walk.candidates.push(id);
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let number = || value.trim_matches('\'').parse().unwrap_or(0);
        match key {
            "bisect_nr" => walk.revisions_left = number(),
            "bisect_steps" => walk.steps_left = number(),
            _ => {}
        }
    }
    walk
}

fn remaining(bad: ObjectId, candidates: &[ObjectId], skipped: &HashSet<ObjectId>) -> Remaining {
    if candidates.is_empty() {
        return Remaining::Nothing;
    }
    let others = || candidates.iter().filter(|id| **id != bad);
    if let Some(best) = others().find(|id| !skipped.contains(*id)) {
        return Remaining::Untested(*best);
    }
    if others().next().is_some() {
        Remaining::OnlySkipped
    } else {
        Remaining::Found
    }
}

/// Splits git's shell quoting (`sq_quote_argv`, as in `BISECT_NAMES`): words in single
/// quotes, separated by spaces, with `'` written as `'\''` and `!` as `'\!'`. `None` when
/// it isn't quoted that way.
fn sq_dequote(bytes: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut words = Vec::new();
    let mut rest = bytes;
    loop {
        while let [b' ' | b'\t' | b'\n', tail @ ..] = rest {
            rest = tail;
        }
        let [b'\'', tail @ ..] = rest else {
            return rest.is_empty().then_some(words);
        };
        rest = tail;
        let mut word = Vec::new();
        loop {
            let end = rest.iter().position(|&b| b == b'\'')?;
            word.extend_from_slice(&rest[..end]);
            rest = &rest[end + 1..];
            // An escaped character between two quoted runs.
            match rest {
                [b'\\', c, b'\'', tail @ ..] => {
                    word.push(*c);
                    rest = tail;
                }
                _ => break,
            }
        }
        words.push(word);
    }
}

fn read_file(path: &Path) -> Result<Option<Vec<u8>>, GitError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(GitError::new(
            GitErrorKind::Other,
            "git bisect",
            format!("{}: {e}", path.display()),
        )),
    }
}

/// The magic that makes a pathspec literal (`*.txt` is that file, not a glob).
const LITERAL: &[u8] = b":(literal)";

/// A git command that may read `BISECT_NAMES`: its `:(literal)` magic needs
/// `GIT_LITERAL_PATHSPECS` off (set to 1, maybe inherited, git would take the magic for
/// part of a file name).
fn bisect_command<'a>(git: &'a GitCli, workdir: &Path) -> GitCommand<'a> {
    git.command(workdir).env("GIT_LITERAL_PATHSPECS", "0")
}

fn literal_pathspec(path: &Path) -> OsString {
    let mut bytes = LITERAL.to_vec();
    bytes.extend_from_slice(path.as_os_str().as_bytes());
    OsString::from_vec(bytes)
}

/// The paths in the words of `BISECT_NAMES`: git writes `--` before them (and nothing
/// without paths); the `:(literal)` magic of [`bisect_start`] is dropped.
fn bisect_paths(names: &[Vec<u8>]) -> Vec<PathBuf> {
    let names = match names {
        [dashdash, rest @ ..] if dashdash.as_slice() == b"--" => rest,
        _ => names,
    };
    names
        .iter()
        .map(|name| {
            let path = name.strip_prefix(LITERAL).unwrap_or(name);
            PathBuf::from(OsString::from_vec(path.to_vec()))
        })
        .collect()
}

fn not_bisecting() -> GitError {
    GitError::new(
        GitErrorKind::Other,
        "git bisect",
        "fatal: no bisect is running",
    )
}

/// `git bisect` reads a revision starting with `-` as an option (and `git bisect good`
/// doesn't take `--`): refuse it up front.
fn refuse_option_like(revision: &str) -> Result<(), GitError> {
    if revision.starts_with('-') {
        return Err(GitError::new(
            GitErrorKind::Other,
            "git bisect",
            format!("fatal: '{revision}' is not a valid revision"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> ObjectId {
        ObjectId::from_hex(format!("{n:02x}").repeat(20).as_bytes()).unwrap()
    }

    #[test]
    fn reads_the_marks_with_the_default_and_custom_terms() {
        let refs = format!(
            "{} refs/bisect/bad\n{} refs/bisect/good-{0}\n{} refs/bisect/good-{2}\n\
             {} refs/bisect/skip-{3}\n{} refs/bisect/other\n",
            id(1),
            id(2),
            id(3),
            id(4),
            id(5),
        );
        let expected = Marks {
            bad: Some(id(1)),
            good: vec![id(2), id(3)],
            skipped: vec![id(4)],
        };
        assert_eq!(parse_marks(&refs, &Terms::default()), expected);

        let terms = parse_terms(b"broken\nfine\n");
        assert_eq!(
            terms,
            Terms {
                bad: "broken".into(),
                good: "fine".into()
            }
        );
        let refs = format!(
            "{} refs/bisect/broken\n{} refs/bisect/fine-{1}\n{} refs/bisect/bad\n",
            id(1),
            id(2),
            id(3),
        );
        assert_eq!(
            parse_marks(&refs, &terms),
            Marks {
                bad: Some(id(1)),
                good: vec![id(2)],
                skipped: vec![],
            }
        );
        // A torn file falls back to the defaults.
        assert_eq!(parse_terms(b"broken\n"), Terms::default());
    }

    #[test]
    fn parses_the_bisect_walk() {
        let text = format!(
            "{} (HEAD, dist=7)\n{} (dist=7)\n{} (main, refs/bisect/bad, dist=0)\n------\n\
             bisect_rev='{0}'\nbisect_nr=7\nbisect_good=7\nbisect_bad=6\nbisect_all=15\n\
             bisect_steps=3\n",
            id(1),
            id(2),
            id(3),
        );
        assert_eq!(
            parse_bisect_walk(&text),
            BisectWalk {
                candidates: vec![id(1), id(2), id(3)],
                revisions_left: 7,
                steps_left: 3,
            }
        );
        assert_eq!(parse_bisect_walk(""), BisectWalk::default());
    }

    #[test]
    fn skipped_commits_are_set_aside() {
        let bad = id(9);
        let skipped: HashSet<_> = [id(1), id(2)].into();
        let remains = |candidates: &[ObjectId]| remaining(bad, candidates, &skipped);
        assert_eq!(remains(&[]), Remaining::Nothing);
        assert_eq!(remains(&[bad]), Remaining::Found);
        assert_eq!(remains(&[id(1), bad, id(2)]), Remaining::OnlySkipped);
        assert_eq!(
            remains(&[id(1), id(3), bad, id(4)]),
            Remaining::Untested(id(3))
        );
    }

    #[test]
    fn dequotes_bisect_names() {
        let words = |bytes: &[u8]| sq_dequote(bytes);
        assert_eq!(words(b""), Some(vec![]));
        assert_eq!(words(b"\n"), Some(vec![]));
        assert_eq!(
            words(b" '--' '-x'"),
            Some(vec![b"--".to_vec(), b"-x".to_vec()])
        );
        assert_eq!(
            words(b" 'it'\\''s here'\\!'.txt' 'caf\xe9'"),
            Some(vec![b"it's here!.txt".to_vec(), b"caf\xe9".to_vec()])
        );
        assert_eq!(words(b"'a b'"), Some(vec![b"a b".to_vec()]));
        assert_eq!(words(b"'unterminated"), None);
        assert_eq!(words(b"bare"), None);
    }

    #[test]
    fn reads_the_paths_from_bisect_names() {
        let words = |words: &[&[u8]]| words.iter().map(|w| w.to_vec()).collect::<Vec<_>>();
        assert_eq!(bisect_paths(&[]), Vec::<PathBuf>::new());
        assert_eq!(bisect_paths(&words(&[b"--"])), Vec::<PathBuf>::new());
        assert_eq!(
            bisect_paths(&words(&[b"--", b":(literal)*.txt", b"src", b"--"])),
            [PathBuf::from("*.txt"), "src".into(), "--".into()]
        );
        // Written without `--` (`git bisect start <bad> <good> <path>`).
        assert_eq!(bisect_paths(&words(&[b"src"])), [PathBuf::from("src")]);
        assert_eq!(
            literal_pathspec(Path::new("-x")),
            OsString::from(":(literal)-x")
        );
    }

    #[test]
    fn option_like_revisions_are_refused() {
        assert!(refuse_option_like("-x").is_err());
        assert!(refuse_option_like("--foo").is_err());
        assert!(refuse_option_like("HEAD~1").is_ok());
    }

    #[test]
    fn finds_the_first_bad_line() {
        let hex = id(7).to_string();
        for line in [
            format!("{hex} is the first 'bad' commit"),
            format!("{hex} is the first 'broken' commit"),
            format!("{hex} is the first bad commit"),
        ] {
            assert_eq!(parse_first_bad(&line), Some(id(7)), "{line}");
        }
        for line in [
            "bisect found first 'bad' commit".to_owned(),
            format!("[{hex}] c11"),
            format!("{hex} is the first 'bad' commit, maybe"),
            "nothex is the first 'bad' commit".to_owned(),
        ] {
            assert_eq!(parse_first_bad(&line), None, "{line}");
        }
    }

    #[test]
    fn splits_lines_across_chunks() {
        let mut lines = LineSplitter::default();
        assert_eq!(lines.feed(b"running 'sh'\nBisect"), ["running 'sh'"]);
        assert_eq!(
            lines.feed(b"ing: 3\r\n\ncaf\xe9\n"),
            ["Bisecting: 3", "", "caf\u{fffd}"]
        );
        assert!(lines.feed(b"no newline").is_empty());
        assert_eq!(lines.finish().as_deref(), Some("no newline"));
        assert_eq!(lines.finish(), None);
    }
}
