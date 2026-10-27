//! The recently opened repositories (start screen), newest first. Stored in
//! `$XDG_DATA_HOME/gitweft/recent-repositories`, one absolute path per line, as raw
//! bytes (paths need not be UTF-8). Callers run these in a job: they do file I/O.

use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::paths::data_dir;

/// How many repositories the list keeps.
pub const LIMIT: usize = 20;

pub fn recent_file() -> PathBuf {
    data_dir().join("recent-repositories")
}

/// The list; empty if the file is missing or unreadable.
pub fn load(file: &Path) -> Vec<PathBuf> {
    let Ok(bytes) = std::fs::read(file) else {
        return Vec::new();
    };
    bytes
        .split(|&byte| byte == b'\n')
        .map(|line| PathBuf::from(std::ffi::OsStr::from_bytes(line)))
        .filter(|path| path.is_absolute())
        .take(LIMIT)
        .collect()
}

/// Moves `repo` to the front (adding it if new) and returns the new list. A read-modify-write
/// under a lock (`<file>.lock`), so two Gitweft instances opening repositories at once both
/// land in the list (#7).
pub fn remember(file: &Path, repo: &Path) -> io::Result<Vec<PathBuf>> {
    // A newline can't be stored in a line-based file; such a path just isn't listed.
    if !repo.is_absolute() || repo.as_os_str().as_bytes().contains(&b'\n') {
        return Ok(load(file));
    }
    crate::datafile::with_lock(file, || {
        let mut list = load(file);
        list.retain(|path| path != repo);
        list.insert(0, repo.to_owned());
        list.truncate(LIMIT);
        save(file, &list)?;
        Ok(list)
    })
}

/// Removes `repo` and returns the new list (under the same lock as [`remember`]).
pub fn forget(file: &Path, repo: &Path) -> io::Result<Vec<PathBuf>> {
    crate::datafile::with_lock(file, || {
        let mut list = load(file);
        list.retain(|path| path != repo);
        save(file, &list)?;
        Ok(list)
    })
}

/// Writes a temporary file and renames it, so a crash never leaves half a list.
fn save(file: &Path, list: &[PathBuf]) -> io::Result<()> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut bytes = Vec::new();
    for path in list {
        bytes.extend_from_slice(path.as_os_str().as_bytes());
        bytes.push(b'\n');
    }
    crate::datafile::atomic_write(file, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writers at once (threads here, instances in real life) each land in the list (#7).
    #[test]
    fn writers_at_once_all_land() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("recent");
        let writers = LIMIT.min(8);
        let threads: Vec<_> = (0..writers)
            .map(|n| {
                let file = file.clone();
                std::thread::spawn(move || {
                    remember(&file, &PathBuf::from(format!("/repo/{n}"))).unwrap();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(load(&file).len(), writers);
    }

    #[test]
    fn newest_first_without_duplicates_and_capped() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sub/recent");
        assert!(load(&file).is_empty());

        remember(&file, Path::new("/a")).unwrap();
        remember(&file, Path::new("/b")).unwrap();
        let list = remember(&file, Path::new("/a")).unwrap();
        assert_eq!(list, [PathBuf::from("/a"), PathBuf::from("/b")]);
        assert_eq!(load(&file), list);

        // Relative paths and paths with a newline are never stored.
        remember(&file, Path::new("relative")).unwrap();
        remember(&file, Path::new("/with\nnewline")).unwrap();
        assert_eq!(load(&file).len(), 2);

        for n in 0..30 {
            remember(&file, &PathBuf::from(format!("/r{n}"))).unwrap();
        }
        let list = load(&file);
        assert_eq!(list.len(), LIMIT);
        assert_eq!(list[0], PathBuf::from("/r29"));

        let list = forget(&file, Path::new("/r29")).unwrap();
        assert_eq!(list[0], PathBuf::from("/r28"));
        assert_eq!(load(&file), list);
    }

    #[test]
    fn non_utf8_paths_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("recent");
        let odd = PathBuf::from(std::ffi::OsStr::from_bytes(b"/repos/caf\xe9"));
        remember(&file, &odd).unwrap();
        assert_eq!(load(&file), [odd]);
    }
}
