//! The Trash page's model: what Gitweft moved to its trash (discarded changes, cleaned
//! files, untracked files a hard reset moved out of the way), listed, restored and
//! deleted.
//!
//! An entry is one folder under the trash root, written by the operation itself
//! (`gitweft_git::trash`, the layout is described there): an index
//! ([`INDEX_FILE`]) with the repository, the time, the action and the files; untracked
//! files under [`FILES_DIR`]; tracked content as blob ids in the repository's object
//! database. Folders from before records existed (files directly in the folder, no
//! index) are listed as legacy entries whose repository is unknown.
//!
//! Tracked blobs are kept from `git gc` by a ref per entry (`gitweft_git::trash::protect`),
//! written with the entry and deleted with it; entries written before that get theirs when
//! they are listed ([`protect_all`]), keeping what gc hasn't pruned yet. Restoring a blob
//! that is gone reports it as failed.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gitweft_git::gix::bstr::BString;
use gitweft_git::trash::{
    TrashRecord, create_entry_dir, is_protected, move_path, now_ms, protect, relative_path,
    unprotect,
};
use gitweft_git::{CancelToken, GitCli, GitError, GitErrorKind, ObjectId};

pub use gitweft_git::trash::{FILES_DIR, INDEX_FILE, TrashAction, TrashFile, TrashFileKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashEntry {
    /// The entry's folder.
    pub dir: PathBuf,
    /// Where untracked files sit, at their repository-relative paths: `dir/files`, or
    /// `dir` itself for a legacy entry.
    pub files_dir: PathBuf,
    /// The working tree the files came from; `None` for a legacy entry.
    pub root: Option<PathBuf>,
    /// Milliseconds since the epoch.
    pub time_ms: u64,
    pub action: TrashAction,
    pub files: Vec<TrashFile>,
    /// The folder's size on disk (tracked content in the object database not counted).
    pub size_bytes: u64,
    /// No readable index: the files were found by walking the folder, all untracked.
    pub legacy: bool,
}

impl TrashEntry {
    /// Where the untracked file `path` sits in the entry.
    pub fn stored_path(&self, path: &[u8]) -> Option<PathBuf> {
        relative_path(path).map(|relative| self.files_dir.join(relative))
    }
}

/// Every entry under `trash_root`, newest first. A missing root is an empty trash.
pub fn list(trash_root: &Path) -> Vec<TrashEntry> {
    let Ok(children) = std::fs::read_dir(trash_root) else {
        return Vec::new();
    };
    let mut entries: Vec<TrashEntry> = children
        .flatten()
        .filter(|child| child.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|child| read_entry(&child.path()))
        .collect();
    entries.sort_by(|a, b| b.time_ms.cmp(&a.time_ms).then_with(|| b.dir.cmp(&a.dir)));
    entries
}

/// The entries that came from the working tree at `root`, including those recorded where
/// it was before a move (#19).
pub fn list_for_repo(trash_root: &Path, root: &Path) -> Vec<TrashEntry> {
    let canonical = root.canonicalize().ok();
    let previous = crate::repo_id::previous_roots_in(&crate::paths::data_dir(), root);
    list(trash_root)
        .into_iter()
        .filter(|entry| {
            entry.root.as_deref().is_some_and(|recorded| {
                recorded == root
                    || (canonical.is_some() && recorded.canonicalize().ok() == canonical)
                    || previous.iter().any(|old| old == recorded)
            })
        })
        .collect()
}

impl TrashEntry {
    /// The working tree the entry belongs to now: where it was recorded, or where that
    /// repository was moved to (#19). `None` for a legacy entry or a repository that's gone.
    pub fn current_root(&self) -> Option<PathBuf> {
        let recorded = self.root.as_deref()?;
        if recorded.is_dir() {
            return Some(recorded.to_owned());
        }
        crate::repo_id::moved_to_in(&crate::paths::data_dir(), recorded)
    }
}

/// The entry in `dir`: from its index, or (none, or unreadable) a legacy entry.
pub fn read_entry(dir: &Path) -> TrashEntry {
    let size_bytes = size_bytes(dir);
    if let Ok(record) = TrashRecord::read(dir) {
        return TrashEntry {
            dir: dir.to_owned(),
            files_dir: dir.join(FILES_DIR),
            root: Some(record.root),
            time_ms: record.time_ms,
            action: record.action,
            files: record.files,
            size_bytes,
            legacy: false,
        };
    }
    let damaged = dir.join(INDEX_FILE).exists() && dir.join(FILES_DIR).is_dir();
    let files_dir = if damaged {
        dir.join(FILES_DIR)
    } else {
        dir.to_owned()
    };
    let mut files = Vec::new();
    walk(&files_dir, &files_dir, &mut files);
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let name = dir.file_name().unwrap_or_default().to_string_lossy();
    TrashEntry {
        dir: dir.to_owned(),
        files_dir,
        root: None,
        time_ms: legacy_time(dir, &name),
        action: if name.ends_with("-reset") {
            TrashAction::Reset
        } else {
            TrashAction::Unknown
        },
        files,
        size_bytes,
        legacy: true,
    }
}

/// Files, symlinks and empty folders under `dir` (relative to `base`), our own index
/// files left out.
fn walk(base: &Path, dir: &Path, files: &mut Vec<TrashFile>) {
    let Ok(children) = std::fs::read_dir(dir) else {
        return;
    };
    for child in children.flatten() {
        let path = child.path();
        let name = child.file_name();
        if dir == base
            && (name == INDEX_FILE || name.as_bytes() == format!("{INDEX_FILE}.tmp").as_bytes())
        {
            continue;
        }
        let is_dir = child.file_type().is_ok_and(|kind| kind.is_dir());
        let empty = is_dir && std::fs::read_dir(&path).is_ok_and(|mut d| d.next().is_none());
        if is_dir && !empty {
            walk(base, &path, files);
            continue;
        }
        if let Ok(relative) = path.strip_prefix(base) {
            files.push(TrashFile {
                path: BString::from(relative.as_os_str().as_bytes()),
                kind: TrashFileKind::Untracked,
            });
        }
    }
}

/// A legacy folder is named `<ms>` or `<ms>-reset`; otherwise its modification time.
fn legacy_time(dir: &Path, name: &str) -> u64 {
    let digits: String = name.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok().unwrap_or_else(|| {
        std::fs::metadata(dir)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    })
}

/// The bytes the files under `dir` take (symlinks not followed).
pub fn size_bytes(dir: &Path) -> u64 {
    let Ok(children) = std::fs::read_dir(dir) else {
        return 0;
    };
    children
        .flatten()
        .map(|child| match child.file_type() {
            Ok(kind) if kind.is_dir() => size_bytes(&child.path()),
            _ => child.metadata().map_or(0, |meta| meta.len()),
        })
        .sum()
}

/// Removes the entry and everything in it, and lets `git gc` have its tracked content.
pub fn delete(git: &GitCli, entry: &TrashEntry) -> io::Result<()> {
    release(git, entry);
    std::fs::remove_dir_all(&entry.dir)
}

/// The trash's optional limits (#19; off by default): entries older than `max_age`, and
/// the oldest entries while the trash is larger than `max_bytes`, are deleted. Returns how
/// many were. Legacy entries count too: they are the oldest.
pub fn enforce_limits(
    git: &GitCli,
    trash_root: &Path,
    max_age: Option<Duration>,
    max_bytes: Option<u64>,
) -> io::Result<usize> {
    let now = now_ms();
    let mut entries = list(trash_root);
    // Oldest first.
    entries.reverse();
    let mut total: u64 = entries.iter().map(|entry| entry.size_bytes).sum();
    let mut removed = 0;
    for entry in entries {
        let too_old = max_age.is_some_and(|age| {
            let age = u64::try_from(age.as_millis()).unwrap_or(u64::MAX);
            now.saturating_sub(entry.time_ms) > age
        });
        let too_big = max_bytes.is_some_and(|max| total > max);
        if !too_old && !too_big {
            continue;
        }
        delete(git, &entry)?;
        total = total.saturating_sub(entry.size_bytes);
        removed += 1;
    }
    Ok(removed)
}

/// Drops the entry's hold on its blobs (its ref). A repository that is gone has nothing
/// to release.
fn release(git: &GitCli, entry: &TrashEntry) {
    if let Some(root) = entry.current_root() {
        let _ = unprotect(git, &root, &entry.dir, &CancelToken::new());
    }
}

/// Gives each of `entries` that holds tracked content without its ref (written before
/// refs kept it, or the ref was lost) that ref, for the blobs `git gc` hasn't pruned yet.
pub fn protect_all(git: &GitCli, entries: &[TrashEntry], cancel: &CancelToken) {
    for entry in entries {
        let Some(root) = entry.current_root() else {
            continue;
        };
        let root = root.as_path();
        let blobs: Vec<ObjectId> = entry
            .files
            .iter()
            .filter_map(|file| match file.kind {
                TrashFileKind::Tracked { blob, .. } => Some(blob),
                TrashFileKind::Untracked => None,
            })
            .collect();
        if blobs.is_empty() || is_protected(git, root, &entry.dir, cancel) {
            continue;
        }
        let _ = protect(git, root, &entry.dir, &blobs, cancel);
    }
}

/// The first folder on the way to `relative` (inside `root`) that is there but isn't a
/// folder (a file now stands where the path needs one), relative to `root`: nothing can
/// be put back under it. Missing folders are no obstacle (a restore creates them).
pub fn blocker_in(root: &Path, relative: &Path) -> Option<PathBuf> {
    let mut current = PathBuf::new();
    for component in relative.parent()?.components() {
        current.push(component);
        // A symlink to a folder is a way through.
        match std::fs::metadata(root.join(&current)) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Some(current),
            Err(_) => return None,
        }
    }
    None
}

/// Moves `paths` (repository-relative: whatever stands in a restore's way) out of the
/// working tree at `root` into a new trash entry, recorded as [`TrashAction::Aside`], so
/// they can be put back later like anything else there. Returns the entry's folder.
pub fn move_aside(trash_root: &Path, root: &Path, paths: &[BString]) -> io::Result<PathBuf> {
    let time = now_ms();
    let dir = create_entry_dir(trash_root, TrashAction::Aside, time)?;
    let mut record = TrashRecord::new(root, TrashAction::Aside, time);
    let files = dir.join(FILES_DIR);
    let mut moved = Ok(());
    for path in paths {
        let Some(relative) = relative_path(path) else {
            continue;
        };
        if let Err(err) = move_path(&root.join(relative), &files.join(relative)) {
            moved = Err(err);
            break;
        }
        record.files.push(TrashFile {
            path: path.clone(),
            kind: TrashFileKind::Untracked,
        });
    }
    // Whatever was moved is recorded, even when a later move failed.
    record.write(&dir)?;
    moved.map(|()| dir)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreReport {
    /// Back in the working tree (or already there, identical), and out of the trash.
    pub restored: Vec<BString>,
    /// Something else is at the path now: nothing was overwritten, and the file stays
    /// in the trash. For a tracked file, "something else" is anything but what the
    /// discard left there (a file changed since the discard).
    pub conflicts: Vec<BString>,
    /// Could not be restored, with why: the blob was pruned, the file is gone from the
    /// trash… The file stays in the record.
    pub failed: Vec<(BString, RestoreFailure)>,
    /// A file now stands where the path needs a folder: each file with what's in its way
    /// ([`blocker_in`]). They stay in the trash; [`move_aside`] clears the way.
    pub blocked: Vec<(BString, BString)>,
    /// Nothing was left in the entry, so its folder was removed.
    pub emptied: bool,
}

/// Why one file couldn't be restored. The UI words it (core has no translations, #47);
/// only [`Self::Other`] carries text, git's or the system's own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreFailure {
    /// The record's path isn't relative to the repository (a damaged record).
    NotRelative,
    /// The saved copy of an untracked file is no longer in the trash folder.
    GoneFromTrash,
    /// Anything else, as git or the system said it.
    Other(String),
}

/// Marks the error [`restore_untracked`] gives for a saved copy that's gone.
#[derive(Debug)]
struct GoneFromTrash;

impl fmt::Display for GoneFromTrash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "no longer in the trash")
    }
}

impl std::error::Error for GoneFromTrash {}

#[derive(Debug)]
pub enum TrashError {
    /// A legacy entry doesn't say where its files came from; set
    /// [`TrashEntry::root`] to restore into a repository of the user's choice.
    UnknownRepository,
    Io(io::Error),
    Git(GitError),
}

impl TrashError {
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Git(err) if err.kind == GitErrorKind::Cancelled)
    }
}

impl fmt::Display for TrashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownRepository => write!(f, "the repository of this trash entry is unknown"),
            Self::Io(err) => write!(f, "{err}"),
            Self::Git(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for TrashError {}

impl From<io::Error> for TrashError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<GitError> for TrashError {
    fn from(err: GitError) -> Self {
        Self::Git(err)
    }
}

enum Outcome {
    Restored,
    Conflict,
}

/// Puts the entry's files at `paths` (repository-relative; others in the entry are left
/// alone) back into its working tree. Tracked content is written from its blob
/// (`git cat-file blob`); untracked files are moved back. An existing file is never
/// overwritten: an identical one counts as restored, a tracked file still exactly as the
/// discard left it is replaced, anything else is a conflict and stays in the trash.
/// Restored files leave the record; an entry left with nothing is removed.
///
/// Cancelling stops between files; what was restored until then leaves the record, and
/// the error is [`GitErrorKind::Cancelled`].
pub fn restore(
    git: &GitCli,
    entry: &TrashEntry,
    paths: &[BString],
    cancel: &CancelToken,
) -> Result<RestoreReport, TrashError> {
    // Into the repository where it is now, if it was moved (#19).
    let current = entry.current_root();
    let root = current
        .as_deref()
        .or(entry.root.as_deref())
        .ok_or(TrashError::UnknownRepository)?;
    let wanted: HashSet<&[u8]> = paths
        .iter()
        .map(|path| path.strip_suffix(b"/").unwrap_or(path))
        .collect();
    let chosen: Vec<&TrashFile> = entry
        .files
        .iter()
        .filter(|file| wanted.contains(&file.path[..]))
        .collect();

    // What the tracked paths hold now, hashed in one go.
    let present: Vec<&BString> = chosen
        .iter()
        .filter(|file| matches!(file.kind, TrashFileKind::Tracked { .. }))
        .map(|file| &file.path)
        .filter(|path| {
            relative_path(path).is_some_and(|relative| {
                std::fs::symlink_metadata(root.join(relative))
                    .is_ok_and(|meta| meta.is_file() || meta.is_symlink())
            })
        })
        .collect();
    let ids = gitweft_git::ops::hash_paths(git, root, &present, false, cancel)?;
    let current: HashMap<&BString, ObjectId> = present.into_iter().zip(ids).collect();

    let mut report = RestoreReport::default();
    let mut outcome = Ok(());
    for file in chosen {
        if cancel.is_cancelled() {
            outcome = Err(GitError::new(GitErrorKind::Cancelled, "restore", "cancelled").into());
            break;
        }
        let Some(relative) = relative_path(&file.path) else {
            report
                .failed
                .push((file.path.clone(), RestoreFailure::NotRelative));
            continue;
        };
        let target = root.join(relative);
        if let Some(blocker) = blocker_in(root, relative) {
            let blocker = BString::from(blocker.as_os_str().as_bytes());
            report.blocked.push((file.path.clone(), blocker));
            continue;
        }
        let result = match &file.kind {
            TrashFileKind::Tracked {
                blob,
                left,
                executable,
                symlink,
            } => restore_tracked(
                git,
                root,
                &target,
                *blob,
                *left,
                (*executable, *symlink),
                current.get(&file.path).copied(),
                cancel,
            ),
            TrashFileKind::Untracked => {
                restore_untracked(&entry.files_dir.join(relative), &target).map_err(Into::into)
            }
        };
        match result {
            Ok(Outcome::Restored) => report.restored.push(file.path.clone()),
            Ok(Outcome::Conflict) => report.conflicts.push(file.path.clone()),
            Err(err) if err.is_cancelled() => {
                outcome = Err(err);
                break;
            }
            Err(TrashError::Io(err))
                if err
                    .get_ref()
                    .is_some_and(|inner| inner.is::<GoneFromTrash>()) =>
            {
                report
                    .failed
                    .push((file.path.clone(), RestoreFailure::GoneFromTrash));
            }
            Err(err) => report
                .failed
                .push((file.path.clone(), RestoreFailure::Other(err.to_string()))),
        }
    }
    report.emptied = forget(git, entry, &report.restored)?;
    outcome.map(|()| report)
}

#[allow(clippy::too_many_arguments)]
fn restore_tracked(
    git: &GitCli,
    root: &Path,
    target: &Path,
    blob: ObjectId,
    left: Option<ObjectId>,
    (executable, symlink): (bool, bool),
    current: Option<ObjectId>,
    cancel: &CancelToken,
) -> Result<Outcome, TrashError> {
    // Whether what's there now (a file or a symlink) is what the discard left, to replace.
    let replace = match std::fs::symlink_metadata(target) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => false,
        Err(err) => return Err(err.into()),
        Ok(meta) if !(meta.is_file() || meta.is_symlink()) => return Ok(Outcome::Conflict),
        Ok(meta) => match current {
            // The very content, as the same kind of entry.
            Some(id) if id == blob && meta.is_symlink() == symlink => {
                return Ok(Outcome::Restored);
            }
            Some(id) if Some(id) == left => true,
            _ => return Ok(Outcome::Conflict),
        },
    };
    let content = git
        .command(root)
        .args(["cat-file", "blob"])
        .arg(blob.to_string())
        .read_only()
        .run(cancel)?
        .stdout;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if replace {
        // Removed, never written through: what the discard left may be a symlink.
        std::fs::remove_file(target)?;
    }
    if symlink {
        std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(&content), target)?;
        return Ok(Outcome::Restored);
    }
    std::fs::write(target, content)?;
    let mut permissions = std::fs::metadata(target)?.permissions();
    let mode = permissions.mode();
    let wanted = if executable {
        // Executable for whoever may read it, like git checks files out.
        mode | ((mode & 0o444) >> 2)
    } else {
        mode & !0o111
    };
    if wanted != mode {
        permissions.set_mode(wanted);
        std::fs::set_permissions(target, permissions)?;
    }
    Ok(Outcome::Restored)
}

fn restore_untracked(stored: &Path, target: &Path) -> io::Result<Outcome> {
    let stored_meta = std::fs::symlink_metadata(stored).map_err(|err| match err.kind() {
        io::ErrorKind::NotFound => io::Error::new(err.kind(), GoneFromTrash),
        _ => err,
    })?;
    match std::fs::symlink_metadata(target) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            move_path(stored, target)?;
            Ok(Outcome::Restored)
        }
        Err(err) => Err(err),
        Ok(meta) => {
            let identical = meta.is_file()
                && stored_meta.is_file()
                && meta.len() == stored_meta.len()
                && std::fs::read(target)? == std::fs::read(stored)?;
            if identical {
                std::fs::remove_file(stored)?;
                Ok(Outcome::Restored)
            } else {
                Ok(Outcome::Conflict)
            }
        }
    }
}

/// Takes the restored files out of the entry: from its record, and the folders they
/// leave empty. Returns whether the entry was emptied and removed (its ref with it).
fn forget(git: &GitCli, entry: &TrashEntry, restored: &[BString]) -> Result<bool, TrashError> {
    if restored.is_empty() {
        return Ok(false);
    }
    // Only the folders the restored files were in: an empty folder that is itself in
    // the trash (a cleaned `empty/`) stays.
    for path in restored {
        let Some(stored) = entry.stored_path(path) else {
            continue;
        };
        let mut folder = stored.parent();
        while let Some(dir) = folder {
            if !dir.starts_with(&entry.files_dir)
                || dir == entry.files_dir
                || std::fs::remove_dir(dir).is_err()
            {
                break;
            }
            folder = dir.parent();
        }
    }
    let remaining = if entry.legacy {
        std::fs::read_dir(&entry.dir)?.next().is_some()
    } else {
        let mut record = TrashRecord::read(&entry.dir)?;
        record.files.retain(|file| !restored.contains(&file.path));
        if !record.files.is_empty() {
            record.write(&entry.dir)?;
        }
        !record.files.is_empty()
    };
    if remaining {
        return Ok(false);
    }
    release(git, entry);
    std::fs::remove_dir_all(&entry.dir)?;
    Ok(true)
}
