//! The activity log: every git command Gitweft runs, with its outcome, so users can see
//! what happened (and repeat it in a terminal). Kept in memory, bounded, local only.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use crate::cli::GitErrorKind;

/// Entries kept; the oldest go first.
pub const ACTIVITY_CAPACITY: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityOutcome {
    Succeeded,
    Failed { kind: GitErrorKind, summary: String },
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct ActivityEntry {
    /// The command line as a user would type it (`git push origin main`).
    pub command: String,
    pub cwd: PathBuf,
    pub started: SystemTime,
    pub duration: Duration,
    /// A pure read (status, log, diff): the UI may hide these.
    pub read_only: bool,
    pub outcome: ActivityOutcome,
}

/// A shared, bounded log; clones share it.
#[derive(Clone)]
pub struct ActivityLog {
    entries: Arc<Mutex<VecDeque<ActivityEntry>>>,
    changed: async_channel::Sender<()>,
    changes: async_channel::Receiver<()>,
}

impl std::fmt::Debug for ActivityLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActivityLog")
            .field("entries", &lock(&self.entries).len())
            .finish()
    }
}

impl Default for ActivityLog {
    fn default() -> Self {
        Self::new()
    }
}

impl ActivityLog {
    pub fn new() -> Self {
        // Capacity 1: a burst of commands wakes a listener once.
        let (changed, changes) = async_channel::bounded(1);
        Self {
            entries: Arc::default(),
            changed,
            changes,
        }
    }

    pub fn record(&self, entry: ActivityEntry) {
        let mut entries = lock(&self.entries);
        if entries.len() == ACTIVITY_CAPACITY {
            entries.pop_front();
        }
        entries.push_back(entry);
        drop(entries);
        let _ = self.changed.try_send(());
    }

    /// All entries, newest first.
    pub fn entries(&self) -> Vec<ActivityEntry> {
        lock(&self.entries).iter().rev().cloned().collect()
    }

    /// Receives a message whenever entries were recorded since the last one.
    pub fn changes(&self) -> async_channel::Receiver<()> {
        self.changes.clone()
    }
}

/// Recovers from poisoning: recording never panics while holding the lock.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
