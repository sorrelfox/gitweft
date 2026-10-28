//! The git processes that may be holding a lock file, which tells a lock a crashed git left
//! behind from one that is still in use.
//!
//! Each system shows a different part of this. Linux and macOS tell this user the working
//! directory of each of their own processes, so there a holder is a git working inside the
//! repository. Windows keeps a process's working directory in that process's own memory,
//! but its Restart Manager names the processes that have a given file open, so there a
//! holder is a git that has the lock file itself open. Either way a git is a program named
//! `git` or `git-…`, and only the processes this user may look at are found.

use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as imp;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as imp;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as imp;

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod imp {
    use super::LockHolder;
    use std::path::{Path, PathBuf};

    pub(super) fn lock_holders(_lock: &Path, _dirs: &[PathBuf]) -> Vec<LockHolder> {
        Vec::new()
    }
}

/// A git process that may be the one holding a lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockHolder {
    pub pid: u32,
    /// Its command line, arguments separated by spaces. Windows doesn't show another
    /// process's arguments, so there it is the path of the program.
    pub command: String,
}

/// The git processes that may be holding the lock file `lock`, by pid: on Linux and macOS
/// those of this user whose working directory lies inside one of `dirs` (pass them
/// canonicalized: the working tree and the git dirs), on Windows those that have `lock`
/// open. Never fails: whatever can't be read is skipped, and on other systems nothing is
/// found.
pub fn lock_holders(lock: &Path, dirs: &[PathBuf]) -> Vec<LockHolder> {
    let mut holders = imp::lock_holders(lock, dirs);
    holders.sort_by_key(|holder| holder.pid);
    holders
}

/// Whether a program's name (Linux's `comm`, the file name without its extension
/// elsewhere) is git's own or one of its helpers'.
#[cfg_attr(
    not(any(target_os = "linux", target_os = "macos", windows)),
    expect(dead_code, reason = "no processes to look at on this system")
)]
fn is_git(name: &str) -> bool {
    name == "git" || name.starts_with("git-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_and_its_helpers_are_git() {
        assert!(is_git("git"));
        assert!(is_git("git-remote-http"));
        assert!(!is_git("gitk"));
        assert!(!is_git("sleep"));
        assert!(!is_git(""));
    }

    #[test]
    fn nothing_holds_a_lock_in_an_empty_folder() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = crate::fs::canonicalize(dir.path()).unwrap();
        let lock = canonical.join("index.lock");
        std::fs::write(&lock, "").unwrap();
        assert_eq!(lock_holders(&lock, &[canonical]), []);
    }
}
