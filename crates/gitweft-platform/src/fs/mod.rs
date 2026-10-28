//! The parts of the file system that only one system has: permission bits and symlinks.
//!
//! On Unix a private folder is one nobody else may enter, and the executable bit is part of
//! what git records. Windows has neither: a folder under the user's own profile is private
//! by the access list it inherits, and Git for Windows works with `core.fileMode` off, so
//! the calls that set or read a mode have nothing to do there.

use std::fs::Metadata;
use std::io;
use std::path::Path;

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
