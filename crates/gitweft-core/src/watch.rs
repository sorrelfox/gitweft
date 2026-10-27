//! Watches one repository's files and reports what needs refreshing.
//!
//! * Git's own files: the git dir (non-recursive), `refs/` (recursive), `reftable/` and
//!   `info/`, plus the common dir of a linked worktree. Objects and logs are not watched.
//! * The working tree: one non-recursive watch per directory that holds tracked files
//!   (from the index) or untracked, non-ignored files (from the last status). Ignored
//!   trees like `target/` or `node_modules/` therefore cost no watches, and events for
//!   ignored files in watched directories are filtered out with `git check-ignore`.
//!
//! Events are batched: the first one opens a window (75 ms for git's files, 200 ms for
//! the working tree) and everything that arrives within it is reported as one
//! [`Changes`]. A steady stream of events still gets reported once per window.
//!
//! If the OS limit on watches is reached, working-tree watches are dropped, the watcher
//! reports [`Update::Limited`] and sends [`Update::Poll`] every few seconds instead. It
//! tries full watching again every minute and on [`RepoWatcher::retry`], and reports
//! [`Update::Recovered`] once that works (#21).

use std::collections::{BTreeSet, HashSet};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use gitweft_git::gix;
use gitweft_git::gix::bstr::BString;
use gitweft_git::read::{dirs_of, ignored_paths, tracked_dirs};
use gitweft_git::{CancelToken, GitCli};
use notify::event::{AccessKind, AccessMode};
use notify::{EventKind, RecommendedWatcher, RecursiveMode};

/// Batching window for changes inside the git dir (refs, index, HEAD).
const GIT_WINDOW: Duration = Duration::from_millis(75);
/// Batching window for working-tree changes (editors and builds write in bursts).
const WORKTREE_WINDOW: Duration = Duration::from_millis(200);
/// Status polling interval once the watch limit has been reached.
const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// How often full watching is tried again in limited mode.
const RETRY_INTERVAL: Duration = Duration::from_secs(60);
/// The git dir's subdirectories that are watched when they exist, and once they appear.
const GIT_SUBDIRS: [(&str, RecursiveMode); 3] = [
    ("refs", RecursiveMode::Recursive),
    ("reftable", RecursiveMode::NonRecursive),
    ("info", RecursiveMode::NonRecursive),
];
/// Paths remembered as ignored before the cache starts over.
const IGNORED_CACHE_LIMIT: usize = 10_000;

/// What needs refreshing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Changes {
    /// The working tree, the index or an operation's state: re-read the status.
    pub status: bool,
    /// HEAD, branches, tags, remote-tracking branches or the config naming upstreams.
    pub refs: bool,
    /// A submodule's own HEAD or index moved, or submodules were set up or removed:
    /// re-read the submodule list.
    pub submodules: bool,
}

impl Changes {
    pub fn is_empty(self) -> bool {
        !self.status && !self.refs && !self.submodules
    }

    pub fn merge(&mut self, other: Changes) {
        self.status |= other.status;
        self.refs |= other.refs;
        self.submodules |= other.submodules;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    /// The initial watches are in place; later changes will be reported.
    Ready,
    Changed(Changes),
    /// The working tree can't be watched (message for the user): from now on only git's
    /// own files are, and [`Update::Poll`] arrives periodically.
    Limited(String),
    /// Limited mode: time to re-read the status.
    Poll,
    /// Limited mode is over: the working tree is watched again (#21).
    Recovered,
    /// The worker kept failing and watching has stopped: from now on only [`Update::Poll`]
    /// arrives (#62).
    Stopped,
}

/// How often the worker starts before the watcher gives up on it and polls.
const WORKER_STARTS: usize = 3;

/// What's left when the worker keeps panicking: [`Update::Stopped`], then an
/// [`Update::Poll`] every [`POLL_INTERVAL`] until the watcher is dropped.
fn poll_only(rx: &mpsc::Receiver<Msg>, updates: &async_channel::Sender<Update>) {
    if updates.send_blocking(Update::Stopped).is_err() {
        return;
    }
    loop {
        match rx.recv_timeout(POLL_INTERVAL) {
            Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => return,
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {
                if updates.send_blocking(Update::Poll).is_err() {
                    return;
                }
            }
        }
    }
}

/// A running watcher. Dropping it stops the watching thread.
pub struct RepoWatcher {
    tx: mpsc::Sender<Msg>,
}

enum Msg {
    Fs(notify::Result<notify::Event>),
    Untracked(Vec<BString>),
    /// In limited mode, try watching the working tree again now.
    Retry,
    Stop,
}

/// What the worker needs of a file watcher; a test gives it one that fails on purpose.
trait WatchDirs: Send {
    fn watch(&mut self, dir: &Path, mode: RecursiveMode) -> notify::Result<()>;
    fn unwatch(&mut self, dir: &Path) -> notify::Result<()>;
}

impl WatchDirs for RecommendedWatcher {
    fn watch(&mut self, dir: &Path, mode: RecursiveMode) -> notify::Result<()> {
        notify::Watcher::watch(self, dir, mode)
    }

    fn unwatch(&mut self, dir: &Path) -> notify::Result<()> {
        notify::Watcher::unwatch(self, dir)
    }
}

impl RepoWatcher {
    /// Starts watching on a thread of its own; nothing blocks the caller. The first
    /// update is [`Update::Ready`].
    pub fn start(
        git: GitCli,
        repo: gix::ThreadSafeRepository,
    ) -> (Self, async_channel::Receiver<Update>) {
        let (tx, rx) = mpsc::channel();
        let (updates, updates_rx) = async_channel::unbounded();
        let events = tx.clone();
        std::thread::Builder::new()
            .name("gitweft-watch".into())
            .spawn(move || {
                // A panic in a dependency (gix-index on a damaged index, #62) must not end
                // watching for good: the worker starts over, and after a few panics the tab
                // is polled instead, and told so.
                for _ in 0..WORKER_STARTS {
                    let events = events.clone();
                    let watcher = notify::recommended_watcher(move |event| {
                        let _ = events.send(Msg::Fs(event));
                    })
                    .map(|watcher| Box::new(watcher) as Box<dyn WatchDirs>);
                    let worker = Worker::new(
                        git.clone(),
                        repo.to_thread_local(),
                        watcher,
                        updates.clone(),
                    );
                    // The panic hook has printed the panic.
                    let run = std::panic::AssertUnwindSafe(|| worker.run(&rx));
                    if std::panic::catch_unwind(run).is_ok() {
                        return;
                    }
                    // What the worker that panicked had seen is lost: re-read the status.
                    if updates.send_blocking(Update::Poll).is_err() {
                        return;
                    }
                }
                poll_only(&rx, &updates);
            })
            .expect("spawn watcher thread");
        (Self { tx }, updates_rx)
    }

    /// Untracked files from the latest status: their directories get watched too, so
    /// changes inside a new directory are seen once the status knows about it.
    pub fn set_untracked(&self, paths: Vec<BString>) {
        let _ = self.tx.send(Msg::Untracked(paths));
    }

    /// In limited mode, tries watching the working tree again now (the tab came into view);
    /// [`Update::Recovered`] follows if that works. Does nothing otherwise.
    pub fn retry(&self) {
        let _ = self.tx.send(Msg::Retry);
    }
}

impl Drop for RepoWatcher {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
    }
}

/// What a path inside the git dir means for the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitPath {
    Irrelevant,
    Index,
    Head,
    Refs,
    /// Merge, rebase, cherry-pick, revert or bisect state.
    OpState,
    /// `info/exclude` (and other `info/` files): ignore rules may have changed.
    Excludes,
    /// A submodule's own `HEAD` or `index` (`modules/<name>/…`).
    Submodule,
}

fn classify_git_path(relative: &Path) -> GitPath {
    // Lock files come and go around every write; the rename onto the real name follows.
    if relative.extension() == Some(OsStr::new("lock")) {
        return GitPath::Irrelevant;
    }
    let Some(first) = relative.components().next() else {
        return GitPath::Irrelevant;
    };
    match first.as_os_str().as_bytes() {
        b"index" => GitPath::Index,
        b"HEAD" => GitPath::Head,
        b"refs" | b"packed-refs" | b"reftable" | b"config" => GitPath::Refs,
        b"MERGE_HEAD" | b"CHERRY_PICK_HEAD" | b"REVERT_HEAD" | b"BISECT_LOG" | b"rebase-merge"
        | b"rebase-apply" | b"sequencer" => GitPath::OpState,
        b"info" => GitPath::Excludes,
        // A submodule's own git dir (`modules/<name>/…`): its HEAD or index moving changes
        // this repository's status (the submodule turns modified, or clean again).
        b"modules" => {
            let in_logs = relative.components().any(|c| c.as_os_str() == "logs");
            match relative.file_name().map(OsStrExt::as_bytes) {
                Some(b"HEAD" | b"index") if !in_logs => GitPath::Submodule,
                _ => GitPath::Irrelevant,
            }
        }
        _ => GitPath::Irrelevant,
    }
}

/// The git dirs of the submodules under `modules` (the folders holding a `HEAD`, however
/// deep their names nest). Their objects and logs aren't descended into.
fn submodule_git_dirs(modules: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![modules.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if dir != modules && dir.join("HEAD").is_file() {
            found.push(dir);
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        stack.extend(
            entries
                .flatten()
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                .map(|entry| entry.path()),
        );
    }
    found.sort();
    found
}

#[derive(Default)]
struct Pending {
    changes: Changes,
    /// The index changed: tracked directories (and the ignored cache) may be stale.
    index: bool,
    /// Ignore rules changed.
    excludes: bool,
    /// Changed working-tree paths, still to be checked against the ignore rules.
    worktree: Vec<BString>,
    /// When the current batch is reported.
    deadline: Option<Instant>,
}

impl Pending {
    fn arm(&mut self, window: Duration) {
        let at = Instant::now() + window;
        self.deadline = Some(self.deadline.map_or(at, |deadline| deadline.min(at)));
    }
}

struct Worker {
    git: GitCli,
    repo: gix::Repository,
    git_dir: PathBuf,
    common_dir: PathBuf,
    workdir: Option<PathBuf>,
    watcher: Option<Box<dyn WatchDirs>>,
    /// Why there is no watcher, reported right after [`Update::Ready`].
    startup_error: Option<String>,
    updates: async_channel::Sender<Update>,
    tracked: BTreeSet<BString>,
    untracked: BTreeSet<BString>,
    /// Working-tree directories currently watched.
    watched: BTreeSet<PathBuf>,
    /// Submodule git dirs currently watched (`modules/<name>`, non-recursive).
    submodule_dirs: BTreeSet<PathBuf>,
    /// Working-tree watches were given up (the watch limit, or no watcher at all).
    limited: bool,
    /// Set in limited mode: when to send the next [`Update::Poll`].
    next_poll: Option<Instant>,
    /// Set in limited mode with a watcher: when to try full watching again.
    next_retry: Option<Instant>,
    /// The subdirectories of [`GIT_SUBDIRS`] being watched.
    git_subdirs: BTreeSet<PathBuf>,
    /// Working-tree paths known to be ignored (cleared when rules or the index change).
    ignored: HashSet<BString>,
    pending: Pending,
}

impl Worker {
    fn new(
        git: GitCli,
        repo: gix::Repository,
        watcher: notify::Result<Box<dyn WatchDirs>>,
        updates: async_channel::Sender<Update>,
    ) -> Self {
        let (watcher, startup_error) = match watcher {
            Ok(watcher) => (Some(watcher), None),
            Err(err) => (None, Some(err.to_string())),
        };
        // Events come with real paths, and they're matched to these by prefix. A linked
        // worktree's common dir comes from its `commondir` file as `…/worktrees/<name>/../..`:
        // watching it works, but no event path starts with it, so every change to its refs
        // was dropped (#21).
        let real = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
        Self {
            git_dir: real(repo.git_dir()),
            common_dir: real(repo.common_dir()),
            workdir: repo.workdir().map(real),
            git,
            repo,
            watcher,
            startup_error,
            updates,
            tracked: BTreeSet::new(),
            untracked: BTreeSet::new(),
            watched: BTreeSet::new(),
            submodule_dirs: BTreeSet::new(),
            limited: false,
            next_poll: None,
            next_retry: None,
            git_subdirs: BTreeSet::new(),
            ignored: HashSet::new(),
            pending: Pending::default(),
        }
    }

    fn run(mut self, rx: &mpsc::Receiver<Msg>) {
        self.watch_git_dirs();
        self.sync_submodule_dirs();
        self.reload_tracked();
        // Reaching the watch limit here is reported after `Ready`, which comes first.
        let limit = self.watch_worktree().err();
        if !self.send(Update::Ready) {
            return;
        }
        if let Some(message) = self.startup_error.take().or(limit) {
            self.enter_limited(message);
        }
        loop {
            let deadline = [self.pending.deadline, self.next_poll, self.next_retry]
                .into_iter()
                .flatten()
                .min();
            let message = match deadline {
                Some(at) => rx.recv_timeout(at.saturating_duration_since(Instant::now())),
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match message {
                Ok(Msg::Fs(Ok(event))) => self.on_event(&event),
                Ok(Msg::Fs(Err(err))) => {
                    eprintln!("gitweft: file watcher error: {err}");
                    self.rescan();
                }
                Ok(Msg::Untracked(paths)) => {
                    let dirs = dirs_of(&paths);
                    if dirs != self.untracked {
                        self.untracked = dirs;
                        self.sync_worktree();
                    }
                }
                Ok(Msg::Retry) => {
                    if !self.try_recover() {
                        return;
                    }
                }
                Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {}
            }
            let now = Instant::now();
            if self.pending.deadline.is_some_and(|at| at <= now) && !self.flush() {
                return;
            }
            if self.next_retry.is_some_and(|at| at <= now) && !self.try_recover() {
                return;
            }
            if self.next_poll.is_some_and(|at| at <= now) {
                if !self.send(Update::Poll) {
                    return;
                }
                self.next_poll = Some(now + POLL_INTERVAL);
            }
        }
    }

    /// `false` once nobody listens any more (the watcher was dropped).
    fn send(&self, update: Update) -> bool {
        self.updates.send_blocking(update).is_ok()
    }

    fn watch_git_dirs(&mut self) {
        let Some(watcher) = &mut self.watcher else {
            return;
        };
        let mut dirs = vec![(self.git_dir.clone(), RecursiveMode::NonRecursive)];
        if self.common_dir != self.git_dir {
            dirs.push((self.common_dir.clone(), RecursiveMode::NonRecursive));
        }
        for (dir, mode) in dirs {
            if let Err(err) = watcher.watch(&dir, mode) {
                eprintln!("gitweft: cannot watch {}: {err}", dir.display());
            }
        }
        self.watch_new_git_subdirs();
    }

    /// Watches the subdirectories of [`GIT_SUBDIRS`] that exist and aren't watched yet: at
    /// the start, and again when the git dir says something was made in it (an `info/`
    /// created after the start holds an `exclude` that counts, #21).
    fn watch_new_git_subdirs(&mut self) {
        let Some(watcher) = &mut self.watcher else {
            return;
        };
        for (name, mode) in GIT_SUBDIRS {
            let dir = self.common_dir.join(name);
            if self.git_subdirs.contains(&dir) || !dir.is_dir() {
                continue;
            }
            match watcher.watch(&dir, mode) {
                Ok(()) => {
                    self.git_subdirs.insert(dir);
                }
                Err(err) => eprintln!("gitweft: cannot watch {}: {err}", dir.display()),
            }
        }
    }

    /// Watches the git dirs of submodules set up since the last call and drops those of
    /// removed ones. Returns whether the set changed.
    fn sync_submodule_dirs(&mut self) -> bool {
        let found: BTreeSet<PathBuf> = submodule_git_dirs(&self.common_dir.join("modules"))
            .into_iter()
            .collect();
        let (added, removed) = set_difference(&self.submodule_dirs, &found);
        if added.is_empty() && removed.is_empty() {
            return false;
        }
        if let Some(watcher) = &mut self.watcher {
            for dir in removed {
                let _ = watcher.unwatch(dir);
            }
            for dir in added {
                if let Err(err) = watcher.watch(dir, RecursiveMode::NonRecursive) {
                    eprintln!("gitweft: cannot watch {}: {err}", dir.display());
                }
            }
        }
        self.submodule_dirs = found;
        true
    }

    fn reload_tracked(&mut self) {
        if self.workdir.is_none() {
            return;
        }
        match tracked_dirs(&self.repo) {
            Ok(dirs) => self.tracked = dirs,
            Err(err) => eprintln!("gitweft: reading the index for the watcher failed: {err}"),
        }
    }

    /// Brings the working-tree watches in line with the tracked and untracked directories.
    fn sync_worktree(&mut self) {
        if self.limited {
            return;
        }
        if let Err(message) = self.watch_worktree() {
            self.enter_limited(message);
        }
    }

    /// [`Self::sync_worktree`]'s watching; `Err` with the message when the watch limit was
    /// reached (the watches made so far stay, and `watched` says which are wanted).
    fn watch_worktree(&mut self) -> Result<(), String> {
        let (Some(workdir), Some(watcher)) = (&self.workdir, &mut self.watcher) else {
            return Ok(());
        };
        let desired: BTreeSet<PathBuf> = self
            .tracked
            .union(&self.untracked)
            .map(|dir| {
                if dir.is_empty() {
                    workdir.clone()
                } else {
                    workdir.join(path_of(dir))
                }
            })
            .collect();
        for gone in self.watched.difference(&desired) {
            let _ = watcher.unwatch(gone);
        }
        let mut limit = None;
        for dir in desired.difference(&self.watched) {
            // Other errors mean the directory vanished meanwhile (or lies outside a sparse
            // checkout); the next status or index change sorts that out.
            if let Err(err) = watcher.watch(dir, RecursiveMode::NonRecursive)
                && matches!(err.kind, notify::ErrorKind::MaxFilesWatch)
            {
                limit = Some(err.to_string());
                break;
            }
        }
        self.watched = desired;
        limit.map_or(Ok(()), Err)
    }

    /// Gives up the working-tree watches and polls instead; says so only on the way in, so a
    /// failed retry doesn't repeat it.
    fn enter_limited(&mut self, message: String) {
        self.drop_worktree_watches();
        let now = Instant::now();
        self.next_poll = Some(now + POLL_INTERVAL);
        self.next_retry = self.watcher.is_some().then_some(now + RETRY_INTERVAL);
        if !std::mem::replace(&mut self.limited, true) {
            self.send(Update::Limited(message));
        }
    }

    fn drop_worktree_watches(&mut self) {
        if let Some(watcher) = &mut self.watcher {
            for dir in &self.watched {
                let _ = watcher.unwatch(dir);
            }
        }
        self.watched.clear();
    }

    /// In limited mode with a watcher, tries watching the working tree again: on success
    /// polling ends, [`Update::Recovered`] is sent and everything is re-read (changes made
    /// while unwatched are unknown); on failure the next try is a minute away. `false` once
    /// nobody listens any more.
    fn try_recover(&mut self) -> bool {
        if !self.limited || self.watcher.is_none() {
            return true;
        }
        match self.watch_worktree() {
            Ok(()) => {
                self.limited = false;
                self.next_poll = None;
                self.next_retry = None;
                self.rescan();
                self.send(Update::Recovered)
            }
            Err(_) => {
                self.drop_worktree_watches();
                self.next_retry = Some(Instant::now() + RETRY_INTERVAL);
                true
            }
        }
    }

    /// Events were lost (queue overflow or a watcher error): assume everything changed.
    fn rescan(&mut self) {
        self.pending.changes = Changes {
            status: true,
            refs: true,
            submodules: true,
        };
        self.pending.index = true;
        self.pending.arm(GIT_WINDOW);
    }

    fn on_event(&mut self, event: &notify::Event) {
        if event.need_rescan() {
            self.rescan();
            return;
        }
        // Reads (including our own `git status`) must not trigger refreshes.
        if let EventKind::Access(kind) = event.kind
            && kind != AccessKind::Close(AccessMode::Write)
        {
            return;
        }
        for path in &event.paths {
            self.on_path(path);
        }
    }

    fn on_path(&mut self, path: &Path) {
        // The git dir first: it usually lies inside the working tree, and a linked
        // worktree's git dir lies inside the common dir.
        for dir in [&self.git_dir, &self.common_dir] {
            let Ok(relative) = path.strip_prefix(dir) else {
                continue;
            };
            let pending = &mut self.pending;
            match classify_git_path(relative) {
                GitPath::Irrelevant => return,
                GitPath::Index => {
                    pending.changes.status = true;
                    pending.index = true;
                }
                GitPath::Head => {
                    pending.changes.status = true;
                    pending.changes.refs = true;
                }
                GitPath::Refs => pending.changes.refs = true,
                GitPath::OpState => pending.changes.status = true,
                GitPath::Excludes => pending.excludes = true,
                GitPath::Submodule => {
                    pending.changes.status = true;
                    pending.changes.submodules = true;
                }
            }
            pending.arm(GIT_WINDOW);
            return;
        }
        let Some(workdir) = &self.workdir else {
            return;
        };
        let Ok(relative) = path.strip_prefix(workdir) else {
            return;
        };
        match relative.components().next() {
            // The root itself, or a linked worktree's `.git` file.
            None => return,
            Some(first) if first.as_os_str() == ".git" => return,
            Some(_) => {}
        }
        if relative.file_name() == Some(OsStr::new(".gitignore")) {
            self.pending.excludes = true;
        }
        self.pending
            .worktree
            .push(BString::from(relative.as_os_str().as_bytes()));
        self.pending.arm(WORKTREE_WINDOW);
    }

    /// Reports the current batch. `false` once nobody listens any more.
    fn flush(&mut self) -> bool {
        let mut pending = std::mem::take(&mut self.pending);
        if pending.excludes || pending.index {
            self.ignored.clear();
        }
        if pending.excludes {
            pending.changes.status = true;
        }
        if !pending.changes.status && self.any_relevant(&mut pending.worktree) {
            pending.changes.status = true;
        }
        if pending.index {
            self.reload_tracked();
            self.sync_worktree();
        }
        // Something was made in the git dir, maybe `info/`, `refs/` or `reftable/`.
        if pending.excludes || pending.changes.refs {
            self.watch_new_git_subdirs();
        }
        if pending.changes.is_empty() {
            return true;
        }
        // Submodules are set up (`git submodule update --init`: config and working tree)
        // and removed (config, index) by commands that change something else too.
        if self.sync_submodule_dirs() {
            pending.changes.submodules = true;
        }
        self.send(Update::Changed(pending.changes))
    }

    /// Whether any of the changed working-tree paths is not ignored.
    fn any_relevant(&mut self, paths: &mut Vec<BString>) -> bool {
        paths.sort_unstable();
        paths.dedup();
        paths.retain(|path| !self.ignored.contains(path));
        let Some(workdir) = &self.workdir else {
            return false;
        };
        if paths.is_empty() {
            return false;
        }
        match ignored_paths(&self.git, workdir, paths, &CancelToken::new()) {
            Ok(ignored) => {
                let relevant = paths.iter().any(|path| !ignored.contains(path));
                if self.ignored.len() + ignored.len() > IGNORED_CACHE_LIMIT {
                    self.ignored.clear();
                }
                self.ignored.extend(ignored);
                relevant
            }
            Err(err) => {
                eprintln!("gitweft: git check-ignore failed: {err}");
                true
            }
        }
    }
}

/// What `new` has that `old` lacks, and what `old` has that `new` lacks.
fn set_difference<'a, T: Ord>(
    old: &'a BTreeSet<T>,
    new: &'a BTreeSet<T>,
) -> (Vec<&'a T>, Vec<&'a T>) {
    (new.difference(old).collect(), old.difference(new).collect())
}

fn path_of(path: &BString) -> &Path {
    Path::new(OsStr::from_bytes(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use gitweft_testkit::Sandbox;

    /// A watcher that refuses watches past `limit` as the OS does at its watch limit.
    struct AtLimit {
        limit: Arc<AtomicUsize>,
        watched: BTreeSet<PathBuf>,
    }

    impl WatchDirs for AtLimit {
        fn watch(&mut self, dir: &Path, _: RecursiveMode) -> notify::Result<()> {
            if self.watched.len() >= self.limit.load(Ordering::SeqCst) {
                return Err(notify::Error::new(notify::ErrorKind::MaxFilesWatch));
            }
            self.watched.insert(dir.to_owned());
            Ok(())
        }

        fn unwatch(&mut self, dir: &Path) -> notify::Result<()> {
            self.watched.remove(dir);
            Ok(())
        }
    }

    /// A worker on a repository with five tracked folders, on its own thread, with a watcher
    /// allowing `limit` watches; returns its message sender and updates.
    fn worker(
        sb: &Sandbox,
        limit: &Arc<AtomicUsize>,
    ) -> (mpsc::Sender<Msg>, async_channel::Receiver<Update>) {
        let repo = sb.init_repo("r");
        for n in 0..5 {
            std::fs::create_dir_all(repo.join(format!("d{n}"))).unwrap();
            std::fs::write(repo.join(format!("d{n}/f.txt")), "x\n").unwrap();
        }
        sb.run(&repo, &["add", "."]);
        sb.run(&repo, &["commit", "-qm", "folders"]);
        let (tx, rx) = mpsc::channel();
        let (updates, updates_rx) = async_channel::unbounded();
        let watcher = AtLimit {
            limit: limit.clone(),
            watched: BTreeSet::new(),
        };
        let repo = gitweft_git::read::open_repo(&repo).unwrap();
        let worker = Worker::new(
            sb.git().clone(),
            repo,
            Ok(Box::new(watcher) as Box<dyn WatchDirs>),
            updates,
        );
        std::thread::spawn(move || worker.run(&rx));
        (tx, updates_rx)
    }

    fn next(updates: &async_channel::Receiver<Update>, within: Duration) -> Option<Update> {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if let Ok(update) = updates.try_recv() {
                return Some(update);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        None
    }

    const WAIT: Duration = Duration::from_secs(5);

    /// At the watch limit the worker polls; a retry that still hits it says nothing more,
    /// and one that works ends limited mode and re-reads everything (#21).
    #[test]
    fn limited_mode_ends_once_watching_works_again() {
        let sb = Sandbox::new();
        // The git dir, `refs/` and `info/` fit; the working tree's six folders don't.
        let limit = Arc::new(AtomicUsize::new(5));
        let (tx, updates) = worker(&sb, &limit);
        assert_eq!(next(&updates, WAIT), Some(Update::Ready));
        assert!(matches!(next(&updates, WAIT), Some(Update::Limited(_))));

        tx.send(Msg::Retry).unwrap();
        assert_eq!(
            next(&updates, Duration::from_millis(300)),
            None,
            "still limited: quiet"
        );

        limit.store(100, Ordering::SeqCst);
        tx.send(Msg::Retry).unwrap();
        assert_eq!(next(&updates, WAIT), Some(Update::Recovered));
        let all = Changes {
            status: true,
            refs: true,
            submodules: true,
        };
        assert_eq!(next(&updates, WAIT), Some(Update::Changed(all)));
        tx.send(Msg::Stop).unwrap();
    }

    /// Lost events (a watcher error) mean anything may have changed (#21).
    #[test]
    fn lost_events_rescan_everything() {
        let sb = Sandbox::new();
        let limit = Arc::new(AtomicUsize::new(100));
        let (tx, updates) = worker(&sb, &limit);
        assert_eq!(next(&updates, WAIT), Some(Update::Ready));
        tx.send(Msg::Fs(Err(notify::Error::generic("queue overflow"))))
            .unwrap();
        let all = Changes {
            status: true,
            refs: true,
            submodules: true,
        };
        assert_eq!(next(&updates, WAIT), Some(Update::Changed(all)));
        tx.send(Msg::Stop).unwrap();
    }

    #[test]
    fn git_paths_are_classified() {
        let cases = [
            ("index", GitPath::Index),
            ("index.lock", GitPath::Irrelevant),
            ("HEAD", GitPath::Head),
            ("HEAD.lock", GitPath::Irrelevant),
            ("refs/heads/feature/x", GitPath::Refs),
            ("refs/heads/main.lock", GitPath::Irrelevant),
            ("packed-refs", GitPath::Refs),
            ("reftable/tables.list", GitPath::Refs),
            ("config", GitPath::Refs),
            ("MERGE_HEAD", GitPath::OpState),
            ("rebase-merge/done", GitPath::OpState),
            ("info/exclude", GitPath::Excludes),
            ("objects/ab/cdef", GitPath::Irrelevant),
            ("logs/HEAD", GitPath::Irrelevant),
            ("FETCH_HEAD", GitPath::Irrelevant),
            ("COMMIT_EDITMSG", GitPath::Irrelevant),
            ("modules/libs/core/HEAD", GitPath::Submodule),
            ("modules/libs/core/index", GitPath::Submodule),
            ("modules/libs/core/index.lock", GitPath::Irrelevant),
            ("modules/tool/modules/inner/HEAD", GitPath::Submodule),
            ("modules/libs/core/config", GitPath::Irrelevant),
            ("modules/libs/core/logs/HEAD", GitPath::Irrelevant),
            ("modules/libs/core/objects/ab/cdef", GitPath::Irrelevant),
            ("modules/libs/core/HEAD.lock", GitPath::Irrelevant),
            ("", GitPath::Irrelevant),
        ];
        for (path, expected) in cases {
            assert_eq!(classify_git_path(Path::new(path)), expected, "{path}");
        }
    }

    #[test]
    fn submodule_git_dirs_are_found_at_any_depth() {
        let dir = tempfile::tempdir().unwrap();
        let modules = dir.path().join("modules");
        for sub in ["libs/core", "tool"] {
            std::fs::create_dir_all(modules.join(sub).join("objects/ab")).unwrap();
            std::fs::write(modules.join(sub).join("HEAD"), "ref: refs/heads/main\n").unwrap();
        }
        // Nested submodules of a submodule live inside its git dir and aren't listed.
        std::fs::create_dir_all(modules.join("tool/modules/inner")).unwrap();
        std::fs::write(modules.join("tool/modules/inner/HEAD"), "x").unwrap();
        assert_eq!(
            submodule_git_dirs(&modules),
            [modules.join("libs/core"), modules.join("tool")]
        );
        assert!(submodule_git_dirs(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn changes_merge() {
        let mut changes = Changes::default();
        assert!(changes.is_empty());
        changes.merge(Changes {
            status: true,
            ..Changes::default()
        });
        changes.merge(Changes {
            refs: true,
            ..Changes::default()
        });
        assert_eq!(
            changes,
            Changes {
                status: true,
                refs: true,
                submodules: false,
            }
        );
        let submodules = Changes {
            submodules: true,
            ..Changes::default()
        };
        assert!(!submodules.is_empty());
        changes.merge(submodules);
        assert!(changes.submodules && changes.status && changes.refs);
    }

    #[test]
    fn set_difference_splits_added_and_removed() {
        let old: BTreeSet<&str> = ["a", "b", "c"].into();
        let new: BTreeSet<&str> = ["b", "c", "d", "e"].into();
        let (added, removed) = set_difference(&old, &new);
        assert_eq!(added, [&"d", &"e"]);
        assert_eq!(removed, [&"a"]);
        let (added, removed) = set_difference(&new, &new);
        assert!(added.is_empty() && removed.is_empty());
    }
}
