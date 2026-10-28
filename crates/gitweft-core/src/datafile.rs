//! Data files Gitweft rewrites whole (review sessions, per-repository settings), and what a
//! save may do with the file already there. A file this build can't read (not JSON, not an
//! object, not UTF-8) is kept as `<name>.bak` before it's replaced; one written by a newer
//! version of Gitweft is left alone. So a failed read never turns into lost data.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

/// What [`write_json`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Saved {
    /// Written: there was no file, or one this build can read.
    Written,
    /// Written, after the unreadable file that was there was moved to this path.
    SetAside(PathBuf),
    /// Not written: the file there comes from a newer version of Gitweft, and stays.
    NewerKept,
}

/// What's at `path`, for a format whose version (the top-level `"version"`, 1 when
/// missing) this build writes as `version`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Existing {
    Missing,
    Readable,
    Newer,
    /// Not a JSON object (or not UTF-8): this build can't read it.
    Unreadable,
}

/// Looks at the file at `path` without changing it.
pub fn existing(path: &Path, version: u64) -> io::Result<Existing> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Existing::Missing),
        Err(err) => return Err(err),
    };
    Ok(match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(object)) => {
            let found = object.get("version").and_then(Value::as_u64).unwrap_or(1);
            if found > version {
                Existing::Newer
            } else {
                Existing::Readable
            }
        }
        _ => Existing::Unreadable,
    })
}

/// Where an unreadable `path` is kept: `<path>.bak`, or `<path>.bak.2`, `.bak.3`… when
/// that's taken (an earlier one is never replaced).
pub fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".bak");
    let first = PathBuf::from(&name);
    if !first.exists() {
        return first;
    }
    (2u32..)
        .map(|n| {
            let mut numbered = name.clone();
            numbered.push(format!(".{n}"));
            PathBuf::from(numbered)
        })
        .find(|candidate| !candidate.exists())
        .expect("some name is free")
}

/// Writes `text` (the JSON of format `version`) to `path`, after [`existing`] says what's
/// there: a newer version's file is left alone ([`Saved::NewerKept`]), an unreadable one is
/// moved aside first ([`Saved::SetAside`]). Writes a temporary file and renames it, so a
/// crash never leaves half a file.
pub fn write_json(path: &Path, version: u64, text: &str) -> io::Result<Saved> {
    // A linked file is read, set aside and replaced where it really is.
    let path = &save_target(path)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let saved = match existing(path, version)? {
        Existing::Newer => return Ok(Saved::NewerKept),
        Existing::Unreadable => {
            let backup = backup_path(path);
            std::fs::rename(path, &backup)?;
            Saved::SetAside(backup)
        }
        Existing::Missing | Existing::Readable => Saved::Written,
    };
    atomic_write(path, text.as_bytes())?;
    Ok(saved)
}

/// The file a save of `path` replaces: `path` itself, or, when it is a symbolic link (a
/// settings file a dotfile manager such as stow or chezmoi links in), the file the link
/// finally points at, so the link stays a link. A link to a missing file gives that
/// file's path, and the save creates it.
pub fn save_target(path: &Path) -> io::Result<PathBuf> {
    let mut current = path.to_path_buf();
    // Linux gives up after 40 links too (ELOOP).
    for _ in 0..40 {
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = std::fs::read_link(&current)?;
                current = match current.parent() {
                    Some(parent) if target.is_relative() => parent.join(target),
                    _ => target,
                };
            }
            Ok(_) => return Ok(current),
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(current),
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::other(format!(
        "{}: too many levels of symbolic links",
        path.display()
    )))
}

/// Replaces the file at `path` with `bytes` so that no crash or power loss leaves half a
/// file or an empty one: the bytes go to a temporary file beside it, which is synced to
/// disk before it is renamed over the old one, and then the folder is synced so the rename
/// is kept too. A symbolic link is followed ([`save_target`]) and stays a link. The folder
/// is created if needed.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let target = save_target(path)?;
    let dir = target.parent().filter(|dir| !dir.as_os_str().is_empty());
    if let Some(dir) = dir {
        std::fs::create_dir_all(dir)?;
    }
    let temporary = temp_path(&target);
    let written = std::fs::File::create(&temporary)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            file.sync_all()
        })
        .and_then(|()| std::fs::rename(&temporary, &target));
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written?;
    if let Some(dir) = dir {
        // Not every filesystem can sync a folder; the file's own data is on disk already.
        let _ = std::fs::File::open(dir).and_then(|dir| dir.sync_all());
    }
    Ok(())
}

/// A temporary name beside `path` to write it under: `<path>.tmp.<pid>.<n>`, different for
/// every save, in this process and in any other, so two savers never share one.
pub fn temp_path(path: &Path) -> PathBuf {
    static SAVES: AtomicU64 = AtomicU64::new(0);
    let n = SAVES.fetch_add(1, Ordering::Relaxed);
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".tmp.{}.{n}", std::process::id()));
    PathBuf::from(name)
}

/// Runs `f` holding an exclusive lock on `<path>.lock` (beside `path`, made if needed), so
/// that writers of `path` take turns, in this process and in others; the lock goes when
/// `f` returns (or the process ends). For read-modify-write saves that mustn't lose another
/// writer's change.
pub fn with_lock<T>(path: &Path, f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(PathBuf::from(name))?;
    lock.lock()?;
    f()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn a_readable_or_missing_file_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/a.json");
        assert_eq!(
            write_json(&path, 1, r#"{"version":1,"n":1}"#).unwrap(),
            Saved::Written
        );
        assert_eq!(
            write_json(&path, 1, r#"{"version":1,"n":2}"#).unwrap(),
            Saved::Written
        );
        assert_eq!(read(&path), r#"{"version":1,"n":2}"#);
        // No version counts as 1; an older one is ours to replace.
        std::fs::write(&path, r#"{"n":0}"#).unwrap();
        assert_eq!(
            write_json(&path, 2, r#"{"version":2}"#).unwrap(),
            Saved::Written
        );
        assert!(!dir.path().join("sub/a.json.tmp").exists());
    }

    #[test]
    fn a_newer_versions_file_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.json");
        std::fs::write(&path, r#"{"version":9,"future":true}"#).unwrap();
        assert_eq!(existing(&path, 1).unwrap(), Existing::Newer);
        assert_eq!(
            write_json(&path, 1, r#"{"version":1}"#).unwrap(),
            Saved::NewerKept
        );
        assert_eq!(read(&path), r#"{"version":9,"future":true}"#);
    }

    #[test]
    fn an_unreadable_file_is_kept_aside_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.json");
        let cases: [&[u8]; 4] = [
            b"{ not json",
            b"[1, 2]",
            b"\"text\"",
            b"{\"n\": \"caf\xe9\"}",
        ];
        for (n, bytes) in cases.iter().enumerate() {
            std::fs::write(&path, bytes).unwrap();
            assert_eq!(
                existing(&path, 1).unwrap(),
                Existing::Unreadable,
                "case {n}"
            );
            let saved = write_json(&path, 1, r#"{"version":1}"#).unwrap();
            let backup = match n {
                0 => dir.path().join("a.json.bak"),
                n => dir.path().join(format!("a.json.bak.{}", n + 1)),
            };
            assert_eq!(saved, Saved::SetAside(backup.clone()), "case {n}");
            // Every earlier backup is still there, byte for byte.
            assert_eq!(std::fs::read(&backup).unwrap(), *bytes, "case {n}");
            assert_eq!(read(&path), r#"{"version":1}"#);
        }
    }

    #[test]
    fn temporary_names_differ_and_stay_beside_the_file() {
        let path = Path::new("/d/settings.json");
        let (a, b) = (temp_path(path), temp_path(path));
        assert_ne!(a, b);
        assert_eq!(a.parent(), path.parent());
        assert!(
            a.to_string_lossy().starts_with("/d/settings.json.tmp."),
            "{}",
            a.display()
        );
    }

    #[test]
    fn writers_holding_the_lock_take_turns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("count");
        std::fs::write(&path, "0").unwrap();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for _ in 0..5 {
                        with_lock(&path, || {
                            let n: u32 = std::fs::read_to_string(&path)?.parse().unwrap();
                            std::thread::yield_now();
                            std::fs::write(&path, (n + 1).to_string())
                        })
                        .unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(read(&path), "40");
    }

    #[test]
    fn a_missing_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            existing(&dir.path().join("none.json"), 1).unwrap(),
            Existing::Missing
        );
    }

    /// The folder's names other than `keep`: a save leaves no temporary file behind.
    #[cfg(unix)]
    fn leftovers(dir: &Path, keep: &[&str]) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| !keep.contains(&name.as_str()))
            .collect();
        names.sort();
        names
    }

    #[cfg(unix)]
    #[test]
    fn a_save_through_a_link_keeps_the_link() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let dotfiles = dir.path().join("dotfiles");
        std::fs::create_dir(&dotfiles).unwrap();
        std::fs::write(dotfiles.join("settings.json"), "old").unwrap();
        // A relative link, as stow makes, and an absolute one to the relative one.
        symlink("dotfiles/settings.json", dir.path().join("relative.json")).unwrap();
        symlink(
            dir.path().join("relative.json"),
            dir.path().join("absolute.json"),
        )
        .unwrap();

        atomic_write(&dir.path().join("absolute.json"), b"new").unwrap();

        for link in ["relative.json", "absolute.json"] {
            let meta = std::fs::symlink_metadata(dir.path().join(link)).unwrap();
            assert!(meta.file_type().is_symlink(), "{link} is still a link");
        }
        assert_eq!(read(&dotfiles.join("settings.json")), "new");
        assert_eq!(
            leftovers(&dotfiles, &["settings.json"]),
            Vec::<String>::new()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_a_missing_file_creates_that_file() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        symlink("elsewhere/keymap.json", dir.path().join("keymap.json")).unwrap();
        atomic_write(&dir.path().join("keymap.json"), b"{}").unwrap();
        assert_eq!(read(&dir.path().join("elsewhere/keymap.json")), "{}");
        let meta = std::fs::symlink_metadata(dir.path().join("keymap.json")).unwrap();
        assert!(meta.file_type().is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_linked_file_is_set_aside_where_it_is() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.json"), "not json").unwrap();
        symlink("real.json", dir.path().join("link.json")).unwrap();

        let saved = write_json(&dir.path().join("link.json"), 1, r#"{"version":1}"#).unwrap();

        assert_eq!(saved, Saved::SetAside(dir.path().join("real.json.bak")));
        assert_eq!(read(&dir.path().join("real.json.bak")), "not json");
        assert_eq!(read(&dir.path().join("link.json")), r#"{"version":1}"#);
        let meta = std::fs::symlink_metadata(dir.path().join("link.json")).unwrap();
        assert!(meta.file_type().is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_loop_is_an_error_not_a_hang() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        symlink("b", dir.path().join("a")).unwrap();
        symlink("a", dir.path().join("b")).unwrap();
        assert!(atomic_write(&dir.path().join("a"), b"x").is_err());
        assert_eq!(leftovers(dir.path(), &["a", "b"]), Vec::<String>::new());
    }
}
