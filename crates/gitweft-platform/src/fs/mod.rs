//! The parts of the file system that only one system has: permission bits, symlinks and
//! the form of a resolved path.
//!
//! On Unix a private folder is one nobody else may enter, and the executable bit is part of
//! what git records. Windows has neither: a folder under the user's own profile is private
//! by the access list it inherits, and Git for Windows works with `core.fileMode` off, so
//! the calls that set or read a mode have nothing to do there.

use std::fs::Metadata;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as imp;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as imp;

/// Creates `dir` and its parents, private to this user.
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    imp::create_private_dir(dir)
}

/// Makes `path` readable and writable by this user alone.
pub fn make_private(path: &Path) -> io::Result<()> {
    imp::make_private(path)
}

/// Why a folder that is to hold one of our sockets isn't this user's alone, in words that
/// go into the message the caller builds, or `None` when it is.
pub fn private_dir_problem(meta: &Metadata) -> Option<&'static str> {
    imp::private_dir_problem(meta)
}

/// Whether a file may be run, which is what git records as the executable bit.
pub fn is_executable(meta: &Metadata) -> bool {
    imp::is_executable(meta)
}

/// Sets or clears `path`'s executable bit.
pub fn set_executable(path: &Path, executable: bool) -> io::Result<()> {
    imp::set_executable(path, executable)
}

/// Makes `link` point at `target`, which may be a path that doesn't exist (yet).
pub fn symlink(target: &Path, link: &Path) -> io::Result<()> {
    imp::symlink(target, link)
}

/// `path` made absolute, with `.`, `..` and links resolved, like `std::fs::canonicalize`,
/// but in the form git takes: on Windows `C:\…` rather than std's `\\?\C:\…`, which git
/// can't work in and which never equals the `C:/…` git prints.
pub fn canonicalize(path: &Path) -> io::Result<PathBuf> {
    imp::canonicalize(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resolved_path_is_one_git_can_take() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("a")).unwrap();
        let resolved = canonicalize(&dir.path().join("a").join("..").join("a")).unwrap();
        assert!(resolved.is_absolute(), "{resolved:?}");
        assert_eq!(resolved, canonicalize(&dir.path().join("a")).unwrap());
        assert_eq!(resolved.file_name(), Some("a".as_ref()));
        assert!(
            !resolved
                .as_os_str()
                .as_encoded_bytes()
                .starts_with(br"\\?\"),
            "{resolved:?}"
        );
        assert!(canonicalize(&dir.path().join("missing")).is_err());
    }
}
