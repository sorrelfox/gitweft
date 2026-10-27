//! The on-disk record of what Gitweft moved to its trash, so a Trash page can list and
//! restore it, even after the app restarts.
//!
//! One trash entry is one folder `<trash root>/<ms>[-reset]/`: the index
//! [`INDEX_FILE`] and, under [`FILES_DIR`], the untracked files that were moved there
//! (at their repository-relative paths). The discarded content of tracked files lives in
//! the repository's object database (`git hash-object -w`); the index keeps the blob ids,
//! and a ref per entry ([`protect_ref`]: a tree of those blobs) keeps `git gc` from
//! pruning them while the entry exists.
//!
//! Writing records lives here, next to the operations that move files (discard, clean,
//! hard reset); listing, restoring and deleting entries is `gitweft_core::trash`.

use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

use gix::ObjectId;
use gix::bstr::BString;
use serde::{Deserialize, Serialize};

use crate::cancel::CancelToken;
use crate::cli::{GitCli, GitError};

/// The index of an entry, in the entry's folder. Moved files are under [`FILES_DIR`], so
/// no file can take its name.
pub const INDEX_FILE: &str = "gitweft-trash.json";
/// Where an entry keeps the untracked files it moved, at their repository-relative paths.
pub const FILES_DIR: &str = "files";
/// The version [`TrashRecord::write`] writes.
pub const RECORD_VERSION: u32 = 1;

/// What put the files in the trash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrashAction {
    /// Discarding changes in the staging panel.
    Discard,
    /// Cleaning untracked or ignored files.
    Clean,
    /// A hard reset, moving untracked files out of the target's way.
    Reset,
    /// Files moved out of a restore's way (a file where a folder is needed).
    Aside,
    /// Written by a newer version, or an entry from before records existed.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrashFileKind {
    /// A tracked file's discarded content, in the object database.
    Tracked {
        blob: ObjectId,
        /// What the discard left at the path (hashed the same way, not written), so a
        /// restore knows the file is unchanged since and may be overwritten. `None` when
        /// nothing was left there, or it isn't known.
        left: Option<ObjectId>,
        executable: bool,
        /// The discarded version was a symlink: `blob` is its target, as git stores a
        /// link, and a restore makes a symlink again.
        symlink: bool,
    },
    /// An untracked file or folder, moved to `files/<path>` in the entry.
    Untracked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashFile {
    /// Repository-relative, without a trailing `/`.
    pub path: BString,
    pub kind: TrashFileKind,
}

/// The index of one trash entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashRecord {
    pub version: u32,
    /// The working tree the files came from (absolute).
    pub root: PathBuf,
    /// When, in milliseconds since the epoch.
    pub time_ms: u64,
    pub action: TrashAction,
    pub files: Vec<TrashFile>,
}

impl TrashRecord {
    pub fn new(root: &Path, action: TrashAction, time_ms: u64) -> Self {
        Self {
            version: RECORD_VERSION,
            root: std::path::absolute(root).unwrap_or_else(|_| root.to_owned()),
            time_ms,
            action,
            files: Vec::new(),
        }
    }

    /// Reads `<dir>/gitweft-trash.json`.
    pub fn read(dir: &Path) -> io::Result<Self> {
        let bytes = std::fs::read(dir.join(INDEX_FILE))?;
        let wire: WireRecord = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        wire.try_into()
    }

    /// Writes `<dir>/gitweft-trash.json` (through a temporary file, so a crash never
    /// leaves half an index).
    pub fn write(&self, dir: &Path) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(&WireRecord::from(self)).map_err(io::Error::other)?;
        let temporary = dir.join(format!("{INDEX_FILE}.tmp"));
        std::fs::write(&temporary, json)?;
        std::fs::rename(&temporary, dir.join(INDEX_FILE))
    }
}

/// Milliseconds since the epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Creates a new, empty entry folder: `<ms>` (`<ms>-reset` for a reset), with `-2`, `-3`…
/// appended when that name is taken (two discards in the same millisecond).
pub fn create_entry_dir(
    trash_root: &Path,
    action: TrashAction,
    time_ms: u64,
) -> io::Result<PathBuf> {
    std::fs::create_dir_all(trash_root)?;
    let base = match action {
        TrashAction::Reset => format!("{time_ms}-reset"),
        _ => format!("{time_ms}"),
    };
    for attempt in 1u32.. {
        let name = match attempt {
            1 => base.clone(),
            n => format!("{base}-{n}"),
        };
        let dir = trash_root.join(name);
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    unreachable!("some folder name is free")
}

/// Where the refs that keep trashed content from `git gc` live.
pub const PROTECT_PREFIX: &str = "refs/gitweft/trash/";

/// The ref that keeps the tracked content of the entry in `dir` from `git gc`: its
/// folder's name under [`PROTECT_PREFIX`] (`None` for a name that can't be a ref).
pub fn protect_ref(dir: &Path) -> Option<String> {
    let name = dir.file_name()?.to_str()?;
    let valid = !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    valid.then(|| format!("{PROTECT_PREFIX}{name}"))
}

/// Keeps `blobs` (an entry's tracked content) reachable while the entry exists: a tree of
/// them, each named by its id, under [`protect_ref`], so `git gc` never prunes them. Git
/// leaves such a ref alone everywhere else (`log --all` and `fsck` skip a tree). Blobs no
/// longer in the repository are left out; with none left, nothing is written.
pub fn protect(
    git: &GitCli,
    workdir: &Path,
    dir: &Path,
    blobs: &[ObjectId],
    cancel: &CancelToken,
) -> Result<(), GitError> {
    let Some(name) = protect_ref(dir) else {
        return Ok(());
    };
    let mut ids = blobs.to_vec();
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        return Ok(());
    }
    let asked: String = ids.iter().map(|id| format!("{id}\n")).collect();
    // A missing object prints `<id> missing`.
    let found = git
        .command(workdir)
        .args(["cat-file", "--batch-check=%(objectname) %(objecttype)"])
        .stdin(asked)
        .read_only()
        .run(cancel)?;
    let text = found.stdout_str();
    let present: Vec<&str> = text
        .lines()
        .filter_map(|line| line.strip_suffix(" blob"))
        .collect();
    if present.is_empty() {
        return Ok(());
    }
    let entries: String = present
        .iter()
        .map(|id| format!("100644 blob {id}\t{id}\n"))
        .collect();
    let tree = git
        .command(workdir)
        .arg("mktree")
        .stdin(entries)
        .run(cancel)?;
    let tree = tree.stdout_str().trim().to_owned();
    git.command(workdir)
        .args(["update-ref", name.as_str(), tree.as_str()])
        .run(cancel)
        .map(|_| ())
}

/// Whether the entry in `dir` has its [`protect_ref`].
pub fn is_protected(git: &GitCli, workdir: &Path, dir: &Path, cancel: &CancelToken) -> bool {
    let Some(name) = protect_ref(dir) else {
        return false;
    };
    git.command(workdir)
        .args(["rev-parse", "--verify", "--quiet", name.as_str()])
        .allow_exit_codes(&[1])
        .read_only()
        .run(cancel)
        .is_ok_and(|output| output.status == 0)
}

/// Lets `git gc` have the entry's tracked content again (the entry is gone): deletes its
/// [`protect_ref`]. A missing ref is nothing to delete.
pub fn unprotect(
    git: &GitCli,
    workdir: &Path,
    dir: &Path,
    cancel: &CancelToken,
) -> Result<(), GitError> {
    let Some(name) = protect_ref(dir) else {
        return Ok(());
    };
    git.command(workdir)
        .args(["update-ref", "-d", name.as_str()])
        .run(cancel)
        .map(|_| ())
}

/// Moves `from` to `to` (a file, symlink or whole folder), creating `to`'s parent
/// folders. Across file systems (the trash under `~/.local/share`, the repository on
/// another disk) it copies and then removes the original.
pub fn move_path(from: &Path, to: &Path) -> io::Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::rename(from, to) {
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
            copy_recursively(from, to)?;
            if std::fs::symlink_metadata(from)?.is_dir() {
                std::fs::remove_dir_all(from)
            } else {
                std::fs::remove_file(from)
            }
        }
        other => other,
    }
}

fn copy_recursively(from: &Path, to: &Path) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(from)?;
    if meta.is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(from)?, to)
    } else if meta.is_dir() {
        std::fs::create_dir(to)?;
        for child in std::fs::read_dir(from)? {
            let child = child?;
            copy_recursively(&child.path(), &to.join(child.file_name()))?;
        }
        std::fs::set_permissions(to, meta.permissions())
    } else {
        std::fs::copy(from, to).map(|_| ())
    }
}

/// `path` as a relative filesystem path, if it is one that stays inside the folder it is
/// joined to (no `..`, not absolute). A trailing `/` (a folder from `git clean -n`) is
/// dropped.
pub fn relative_path(path: &[u8]) -> Option<&Path> {
    let trimmed = path.strip_suffix(b"/").unwrap_or(path);
    let relative = Path::new(OsStr::from_bytes(trimmed));
    let normal = relative
        .components()
        .all(|component| matches!(component, Component::Normal(_) | Component::CurDir));
    (normal && !trimmed.is_empty()).then_some(relative)
}

// The JSON form. Paths are bytes: text when they are UTF-8, `{"hex": "…"}` otherwise.

#[derive(Serialize, Deserialize)]
struct WireRecord {
    version: u32,
    root: WireBytes,
    time_ms: u64,
    action: TrashAction,
    files: Vec<WireFile>,
}

#[derive(Serialize, Deserialize)]
struct WireFile {
    path: WireBytes,
    #[serde(flatten)]
    kind: WireKind,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum WireKind {
    Tracked {
        blob: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        left: Option<String>,
        #[serde(default)]
        executable: bool,
        // Written only when set, so records of plain files are as before.
        #[serde(default, skip_serializing_if = "is_false")]
        symlink: bool,
    },
    Untracked,
}

fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum WireBytes {
    Text(String),
    Hex { hex: String },
}

impl WireBytes {
    fn new(bytes: &[u8]) -> Self {
        match std::str::from_utf8(bytes) {
            Ok(text) => Self::Text(text.to_owned()),
            Err(_) => Self::Hex {
                hex: bytes.iter().map(|b| format!("{b:02x}")).collect(),
            },
        }
    }

    fn into_bytes(self) -> io::Result<Vec<u8>> {
        match self {
            Self::Text(text) => Ok(text.into_bytes()),
            Self::Hex { hex } => {
                let digits = hex.as_bytes();
                if digits.len() % 2 != 0 {
                    return Err(invalid("odd hex length"));
                }
                digits
                    .chunks(2)
                    .map(|pair| {
                        std::str::from_utf8(pair)
                            .ok()
                            .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                            .ok_or_else(|| invalid("bad hex digit"))
                    })
                    .collect()
            }
        }
    }
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("trash index: {what}"))
}

fn object_id(hex: &str) -> io::Result<ObjectId> {
    ObjectId::from_hex(hex.as_bytes()).map_err(|_| invalid("bad object id"))
}

impl From<&TrashRecord> for WireRecord {
    fn from(record: &TrashRecord) -> Self {
        Self {
            version: record.version,
            root: WireBytes::new(record.root.as_os_str().as_bytes()),
            time_ms: record.time_ms,
            action: record.action,
            files: record
                .files
                .iter()
                .map(|file| WireFile {
                    path: WireBytes::new(&file.path),
                    kind: match &file.kind {
                        TrashFileKind::Tracked {
                            blob,
                            left,
                            executable,
                            symlink,
                        } => WireKind::Tracked {
                            blob: blob.to_string(),
                            left: left.map(|id| id.to_string()),
                            executable: *executable,
                            symlink: *symlink,
                        },
                        TrashFileKind::Untracked => WireKind::Untracked,
                    },
                })
                .collect(),
        }
    }
}

impl TryFrom<WireRecord> for TrashRecord {
    type Error = io::Error;

    fn try_from(wire: WireRecord) -> io::Result<Self> {
        let root = PathBuf::from(std::ffi::OsString::from_vec(wire.root.into_bytes()?));
        let files = wire
            .files
            .into_iter()
            .map(|file| {
                let kind = match file.kind {
                    WireKind::Tracked {
                        blob,
                        left,
                        executable,
                        symlink,
                    } => TrashFileKind::Tracked {
                        blob: object_id(&blob)?,
                        left: left.as_deref().map(object_id).transpose()?,
                        executable,
                        symlink,
                    },
                    WireKind::Untracked => TrashFileKind::Untracked,
                };
                Ok(TrashFile {
                    path: file.path.into_bytes()?.into(),
                    kind,
                })
            })
            .collect::<io::Result<_>>()?;
        Ok(Self {
            version: wire.version,
            root,
            time_ms: wire.time_ms,
            action: wire.action,
            files,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_non_utf8_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = PathBuf::from(std::ffi::OsString::from_vec(b"/r/caf\xe9".to_vec()));
        let blob = ObjectId::from_hex(b"0123456789abcdef0123456789abcdef01234567").unwrap();
        let mut record = TrashRecord::new(&root, TrashAction::Clean, 42);
        record.files = vec![
            TrashFile {
                path: BString::from(&b"d/\xff.txt"[..]),
                kind: TrashFileKind::Tracked {
                    blob,
                    left: None,
                    executable: true,
                    symlink: false,
                },
            },
            TrashFile {
                path: "link".into(),
                kind: TrashFileKind::Tracked {
                    blob,
                    left: Some(blob),
                    executable: false,
                    symlink: true,
                },
            },
            TrashFile {
                path: "plain.txt".into(),
                kind: TrashFileKind::Untracked,
            },
        ];
        record.write(dir.path()).unwrap();
        assert_eq!(TrashRecord::read(dir.path()).unwrap(), record);
        let text = std::fs::read_to_string(dir.path().join(INDEX_FILE)).unwrap();
        assert!(text.contains("\"hex\": \"642fff2e747874\""), "{text}");
        assert!(text.contains("\"path\": \"plain.txt\""), "{text}");
        // Only the link says so: a plain file's record is as before.
        assert_eq!(text.matches("\"symlink\": true").count(), 1, "{text}");
        assert!(!text.contains("\"symlink\": false"), "{text}");
    }

    #[test]
    fn records_from_before_symlinks_read_as_files() {
        let dir = tempfile::tempdir().unwrap();
        let blob = "0123456789abcdef0123456789abcdef01234567";
        let json = format!(
            r#"{{"version":1,"root":"/r","time_ms":1,"action":"discard",
                "files":[{{"path":"a","kind":"tracked","blob":"{blob}","executable":true}}]}}"#
        );
        std::fs::write(dir.path().join(INDEX_FILE), json).unwrap();
        let record = TrashRecord::read(dir.path()).unwrap();
        assert!(matches!(
            record.files[0].kind,
            TrashFileKind::Tracked {
                executable: true,
                symlink: false,
                ..
            }
        ));
    }

    #[test]
    fn unknown_actions_and_bad_paths() {
        let wire: WireRecord = serde_json::from_str(
            r#"{"version":9,"root":"/r","time_ms":1,"action":"shred","files":[]}"#,
        )
        .unwrap();
        assert_eq!(wire.action, TrashAction::Unknown);
        assert_eq!(relative_path(b"a/b/"), Some(Path::new("a/b")));
        assert_eq!(relative_path(b"../x"), None);
        assert_eq!(relative_path(b"/etc/passwd"), None);
        assert_eq!(relative_path(b""), None);
    }

    #[test]
    fn entry_folders_never_collide() {
        let root = tempfile::tempdir().unwrap();
        let a = create_entry_dir(root.path(), TrashAction::Discard, 7).unwrap();
        let b = create_entry_dir(root.path(), TrashAction::Discard, 7).unwrap();
        let c = create_entry_dir(root.path(), TrashAction::Reset, 7).unwrap();
        assert_eq!(a.file_name().unwrap(), "7");
        assert_eq!(b.file_name().unwrap(), "7-2");
        assert_eq!(c.file_name().unwrap(), "7-reset");
    }
}
