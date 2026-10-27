//! A repository's undo and redo stacks, kept across restarts in
//! `$XDG_DATA_HOME/gitweft/repos/<name>-<hash of the path>/oplog.jsonl`: one JSON object
//! per line, the undo stack oldest first, then the redo stack. Entries hold the ref
//! values to compare against, so one that no longer fits the repository (changed in a
//! terminal meanwhile) is refused when used, never applied blindly. Callers run these
//! in a job: they do file I/O.
//!
//! Several writers can share one log: two tabs on one repository, or two Gitweft
//! processes. Each change is a read-modify-write of the file under a lock ([`update`]), so
//! one writer's steps never overwrite another's.

use std::io;
use std::path::{Path, PathBuf};

use gitweft_git::gix::bstr::BString;
use gitweft_git::undo::{BranchConfig, RefChange, UndoEntry, Worktree};
use gitweft_git::{CancelToken, GitCli, GitError, ObjectId};
use serde::{Deserialize, Serialize};

use crate::paths::repo_data_dir;

/// How many steps each stack keeps.
pub const LIMIT: usize = 100;

const VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UndoHistory {
    /// Newest last.
    pub undo: Vec<UndoEntry>,
    /// Newest last.
    pub redo: Vec<UndoEntry>,
}

impl UndoHistory {
    /// A new operation: it can be undone, and nothing undone before can be redone.
    pub fn push(&mut self, entry: UndoEntry) {
        self.undo.push(entry);
        self.redo.clear();
        self.cap();
    }

    /// After `entry`, taken from the undo stack (`redo`: from the redo stack), was used: it
    /// leaves that stack (if another writer hasn't taken it already), and `reverse`, the
    /// step back, goes on the other one.
    pub fn used(&mut self, entry: &UndoEntry, redo: bool, reverse: Option<UndoEntry>) {
        let (from, to) = if redo {
            (&mut self.redo, &mut self.undo)
        } else {
            (&mut self.undo, &mut self.redo)
        };
        if let Some(at) = from.iter().rposition(|other| other == entry) {
            from.remove(at);
        }
        to.extend(reverse);
        self.cap();
    }

    fn cap(&mut self) {
        for stack in [&mut self.undo, &mut self.redo] {
            let excess = stack.len().saturating_sub(LIMIT);
            stack.drain(..excess);
        }
    }
}

/// The log of the repository at `root` (its working tree, or git dir when bare).
pub fn oplog_file(root: &Path) -> PathBuf {
    repo_data_dir(root).join("oplog.jsonl")
}

/// The stacks; empty if the file is missing or unreadable. Lines that can't be read are
/// skipped here ([`update`] keeps them in the file).
pub fn load(file: &Path) -> UndoHistory {
    std::fs::read(file)
        .map(|bytes| parse(&bytes).0)
        .unwrap_or_default()
}

/// The stacks in the log's `bytes`, and the lines this build can't read (a newer version's,
/// or damaged ones, not even UTF-8), which [`update`] writes back as they were: a rewrite
/// never loses another writer's steps (#4). At most [`LIMIT`] of those, the newest.
fn parse(bytes: &[u8]) -> (UndoHistory, Vec<Vec<u8>>) {
    let mut history = UndoHistory::default();
    let mut foreign = Vec::new();
    for raw in bytes.split(|byte| *byte == b'\n') {
        if raw.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let entry = std::str::from_utf8(raw)
            .ok()
            .and_then(|text| serde_json::from_str::<Line>(text).ok())
            .filter(|line| line.v == VERSION)
            .and_then(|line| {
                let stack = line.stack;
                line.into_entry().map(|entry| (stack, entry))
            });
        match entry {
            Some((Stack::Undo, entry)) => history.undo.push(entry),
            Some((Stack::Redo, entry)) => history.redo.push(entry),
            None => foreign.push(raw.to_vec()),
        }
    }
    history.cap();
    let excess = foreign.len().saturating_sub(LIMIT);
    foreign.drain(..excess);
    (history, foreign)
}

/// Applies `change` to the stacks as saved in `file` and saves them, holding a lock
/// (`oplog.lock` beside it) so that writers take turns. Returns the stacks as saved. A
/// plain read doesn't need the lock: [`save`] replaces the file in one rename.
pub fn update(file: &Path, change: impl FnOnce(&mut UndoHistory)) -> io::Result<UndoHistory> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(file.with_extension("lock"))?;
    // Released when `lock` is dropped (also if this process dies).
    lock.lock()?;
    // A log that is there but can't be read (permissions) is an error, never an empty one
    // to save over.
    let (mut history, foreign) = match std::fs::read(file) {
        Ok(bytes) => parse(&bytes),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Default::default(),
        Err(err) => return Err(err),
    };
    let before = history.clone();
    change(&mut history);
    history.cap();
    if history != before {
        write(file, &history, &foreign)?;
    }
    Ok(history)
}

/// What [`apply`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// The step back (see `gitweft_git::undo::undo`).
    pub reverse: UndoEntry,
    /// Files the undone hard reset had moved to the trash that couldn't go back
    /// (something else is at their path now): they stay in that trash entry.
    pub left_in_trash: usize,
}

/// Undoes `entry` (`gitweft_git::undo::undo`), then puts back the untracked files the
/// undone hard reset moved to the trash, never over anything ([`crate::trash::restore`]).
pub fn apply(
    git: &GitCli,
    workdir: &Path,
    entry: &UndoEntry,
    trash_root: &Path,
    cancel: &CancelToken,
) -> Result<Applied, GitError> {
    let reverse = gitweft_git::undo::undo(git, workdir, entry, trash_root, cancel)?;
    // A trash entry deleted or restored from the Trash page meanwhile has nothing to give.
    let left_in_trash = entry
        .trash
        .as_deref()
        .filter(|dir| dir.join(crate::trash::INDEX_FILE).is_file())
        .map_or(0, |dir| {
            let trashed = crate::trash::read_entry(dir);
            let paths: Vec<BString> = trashed.files.iter().map(|f| f.path.clone()).collect();
            // The undo is done: finish putting the files back even if cancelled now.
            match crate::trash::restore(git, &trashed, &paths, &CancelToken::new()) {
                Ok(report) => report.conflicts.len() + report.failed.len() + report.blocked.len(),
                Err(_) => paths.len(),
            }
        });
    Ok(Applied {
        reverse,
        left_in_trash,
    })
}

/// Writes a temporary file and renames it, so a crash never leaves half a log.
pub fn save(file: &Path, history: &UndoHistory) -> io::Result<()> {
    write(file, history, &[])
}

/// [`save`], with the lines this build couldn't read (`foreign`) first, as they were.
fn write(file: &Path, history: &UndoHistory, foreign: &[Vec<u8>]) -> io::Result<()> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text: Vec<u8> = Vec::new();
    for line in foreign {
        text.extend_from_slice(line);
        text.push(b'\n');
    }
    let lines = history
        .undo
        .iter()
        .map(|entry| (Stack::Undo, entry))
        .chain(history.redo.iter().map(|entry| (Stack::Redo, entry)));
    for (stack, entry) in lines {
        let line = serde_json::to_string(&Line::new(stack, entry)).map_err(io::Error::other)?;
        text.extend_from_slice(line.as_bytes());
        text.push(b'\n');
    }
    crate::datafile::atomic_write(file, &text)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Stack {
    Undo,
    Redo,
}

#[derive(Serialize, Deserialize)]
struct Line {
    v: u32,
    stack: Stack,
    label: String,
    /// `soft`, `follow`, `mixed`, `discard`, `restore-index` or `restore-changes`.
    worktree: String,
    /// The tree or commit of `restore-index` and `restore-changes`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    object: Option<String>,
    changes: Vec<Change>,
    /// Branch config kept with the entry (from G15i2; older lines have none).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    config: Vec<ConfigOf>,
    /// The trash entry whose files the undo puts back (a UTF-8 path; others aren't kept).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    trash: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct ConfigOf {
    branch: String,
    /// `[key, value]` pairs, in order.
    entries: Vec<(String, String)>,
}

#[derive(Serialize, Deserialize)]
struct Change {
    name: String,
    before: Option<String>,
    after: Option<String>,
}

impl Line {
    fn new(stack: Stack, entry: &UndoEntry) -> Self {
        let (worktree, object) = match entry.worktree {
            Worktree::Soft => ("soft", None),
            Worktree::Follow => ("follow", None),
            Worktree::Mixed => ("mixed", None),
            Worktree::Discard => ("discard", None),
            Worktree::RestoreIndex(tree) => ("restore-index", Some(tree.to_string())),
            Worktree::RestoreChanges(commit) => ("restore-changes", Some(commit.to_string())),
        };
        Self {
            v: VERSION,
            stack,
            label: entry.label.clone(),
            worktree: worktree.to_owned(),
            object,
            changes: entry
                .changes
                .iter()
                .map(|change| Change {
                    name: change.name.clone(),
                    before: change.before.map(|id| id.to_string()),
                    after: change.after.map(|id| id.to_string()),
                })
                .collect(),
            config: entry
                .branch_config
                .iter()
                .map(|config| ConfigOf {
                    branch: config.branch.clone(),
                    entries: config.entries.clone(),
                })
                .collect(),
            trash: entry
                .trash
                .as_deref()
                .and_then(Path::to_str)
                .map(str::to_owned),
        }
    }

    fn into_entry(self) -> Option<UndoEntry> {
        let object = || ObjectId::from_hex(self.object.as_deref()?.as_bytes()).ok();
        let worktree = match self.worktree.as_str() {
            "soft" => Worktree::Soft,
            "follow" => Worktree::Follow,
            "mixed" => Worktree::Mixed,
            "discard" => Worktree::Discard,
            "restore-index" => Worktree::RestoreIndex(object()?),
            "restore-changes" => Worktree::RestoreChanges(object()?),
            _ => return None,
        };
        // A value that is there but isn't an id makes the whole entry unusable.
        let id = |value: Option<String>| -> Option<Option<ObjectId>> {
            match value {
                None => Some(None),
                Some(hex) => ObjectId::from_hex(hex.as_bytes()).ok().map(Some),
            }
        };
        let changes = self
            .changes
            .into_iter()
            .map(|change| {
                Some(RefChange {
                    name: change.name,
                    before: id(change.before)?,
                    after: id(change.after)?,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        let mut entry = UndoEntry::new(self.label, changes, worktree)?;
        entry.branch_config = self
            .config
            .into_iter()
            .map(|config| BranchConfig {
                branch: config.branch,
                entries: config.entries,
            })
            .collect();
        entry.trash = self.trash.map(PathBuf::from);
        Some(entry)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStrExt;

    use super::*;

    fn id(byte: u8) -> ObjectId {
        ObjectId::from_bytes_or_panic(&[byte; 20])
    }

    fn entry(label: &str, worktree: Worktree) -> UndoEntry {
        UndoEntry {
            label: label.to_owned(),
            changes: vec![
                RefChange {
                    name: "refs/heads/main".into(),
                    before: Some(id(1)),
                    after: Some(id(2)),
                },
                RefChange {
                    name: "refs/tags/v1".into(),
                    before: None,
                    after: Some(id(3)),
                },
            ],
            worktree,
            branch_config: Vec::new(),
            trash: None,
        }
    }

    #[test]
    fn both_stacks_round_trip_with_every_worktree_mode() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("repos/r/oplog.jsonl");
        assert_eq!(load(&file), UndoHistory::default());
        let history = UndoHistory {
            undo: vec![
                entry("commit “a \"quoted\" \n two lines”", Worktree::Soft),
                entry("merge", Worktree::Follow),
                entry("reset", Worktree::RestoreIndex(id(7))),
                entry("reset hard", Worktree::RestoreChanges(id(8))),
            ],
            redo: vec![
                entry("reset", Worktree::Mixed),
                entry("reset hard", Worktree::Discard),
            ],
        };
        let mut history = history;
        history.undo[3].trash = Some(PathBuf::from("/data/gitweft/trash/17-reset"));
        history.redo[0].branch_config.push(BranchConfig {
            branch: "refs/heads/main".into(),
            entries: vec![
                ("branch.main.remote".into(), "origin".into()),
                ("branch.main.description".into(), "two\nlines".into()),
            ],
        });
        save(&file, &history).unwrap();
        assert_eq!(load(&file), history);
        assert_eq!(std::fs::read_to_string(&file).unwrap().lines().count(), 6);
    }

    #[test]
    fn lines_from_before_the_config_and_trash_fields_still_read() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("oplog.jsonl");
        let old = format!(
            r#"{{"v":1,"stack":"undo","label":"old","worktree":"follow","changes":[{{"name":"refs/heads/x","before":"{}","after":null}}]}}"#,
            id(1)
        );
        std::fs::write(&file, old).unwrap();
        let loaded = load(&file).undo;
        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].branch_config.is_empty());
        assert_eq!(loaded[0].trash, None);
        // And a plain entry writes neither field.
        save(
            &file,
            &UndoHistory {
                undo: loaded,
                redo: Vec::new(),
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            !text.contains("config") && !text.contains("trash"),
            "{text}"
        );
    }

    #[test]
    fn unreadable_lines_are_skipped_and_stacks_capped() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("oplog.jsonl");
        let good =
            serde_json::to_string(&Line::new(Stack::Undo, &entry("ok", Worktree::Soft))).unwrap();
        let text = [
            "not json".to_owned(),
            good.replace("\"v\":1", "\"v\":2"),
            good.replace("\"soft\"", "\"sideways\""),
            good.replace(&id(2).to_string(), "xyz"),
            good.replace("\"soft\"", "\"restore-index\""), // no object
            good.clone(),
        ]
        .join("\n");
        std::fs::write(&file, text).unwrap();
        assert_eq!(load(&file).undo, [entry("ok", Worktree::Soft)]);

        let many = UndoHistory {
            undo: (0..LIMIT + 5)
                .map(|n| entry(&n.to_string(), Worktree::Soft))
                .collect(),
            redo: Vec::new(),
        };
        save(&file, &many).unwrap();
        let loaded = load(&file);
        assert_eq!(loaded.undo.len(), LIMIT);
        assert_eq!(loaded.undo[0].label, "5");
    }

    #[test]
    fn an_update_keeps_the_lines_it_cant_read() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("oplog.jsonl");
        let ours =
            serde_json::to_string(&Line::new(Stack::Undo, &entry("ours", Worktree::Soft))).unwrap();
        let newer = ours
            .replace("\"v\":1", "\"v\":2")
            .replace("ours", "a newer build's");
        let damaged: &[u8] = b"{\"v\":1,\"stack\":\"undo\",\"label\":\"caf\xe9";
        let mut text = Vec::new();
        for line in [newer.as_bytes(), damaged, ours.as_bytes()] {
            text.extend_from_slice(line);
            text.push(b'\n');
        }
        std::fs::write(&file, &text).unwrap();

        let history = update(&file, |history| {
            history.push(entry("new", Worktree::Follow))
        })
        .unwrap();
        let labels: Vec<&str> = history.undo.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(labels, ["ours", "new"]);
        // The newer version's line and the damaged one are still there, byte for byte.
        let written = std::fs::read(&file).unwrap();
        let lines: Vec<&[u8]> = written
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .collect();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], newer.as_bytes());
        assert_eq!(lines[1], damaged);
        assert_eq!(load(&file).undo.len(), 2);
    }

    #[test]
    fn one_file_per_repository_path() {
        let a = oplog_file(Path::new("/home/u/code/app"));
        assert_eq!(a, oplog_file(Path::new("/home/u/code/app")));
        assert_ne!(a, oplog_file(Path::new("/home/u/other/app")));
        let folder = a.parent().unwrap().file_name().unwrap().to_string_lossy();
        assert!(folder.starts_with("app-"), "{folder}");
        let odd = oplog_file(Path::new(std::ffi::OsStr::from_bytes(b"/r/caf\xe9 x")));
        let folder = odd.parent().unwrap().file_name().unwrap().to_string_lossy();
        assert!(folder.starts_with("caf__x-"), "{folder}");
    }
}
