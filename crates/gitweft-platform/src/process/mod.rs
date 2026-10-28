//! Starting child processes, and stopping all of what they started.
//!
//! A git command is rarely one process: it runs ssh, hooks and credential helpers of its
//! own, and cancelling means ending the lot, or a helper that ignores the first signal
//! keeps the pipes open and the reader waiting. [`spawn_tree`] therefore puts the child
//! into a group of its own that [`ProcessTree::kill`] can end in one go: a process group on
//! Unix, a job object on Windows. The user's own programs, which outlive us, go through
//! [`spawn_detached`] instead.
//!
//! [`lock_holders`] looks at the processes already running, for a git that may still be
//! holding a lock file.

use std::path::{Path, PathBuf};
use std::process::ExitStatus;

mod holders;
pub use holders::{LockHolder, lock_holders};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{ProcessTree, spawn_detached, spawn_tree};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{ProcessTree, spawn_detached, spawn_tree};

/// Windows' default when `PATHEXT` is unset.
#[cfg(windows)]
const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

/// The code a finished process left. A process Unix killed with a signal reports 128 plus
/// the signal's number, the way a shell does.
pub fn exit_code(status: ExitStatus) -> Option<i32> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;

        status.code().or_else(|| status.signal().map(|s| 128 + s))
    }
    #[cfg(windows)]
    {
        status.code()
    }
}

/// `name` as found on `PATH`, the way the system itself would find it.
pub fn find_program(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| candidates(&dir, name))
        .find(|candidate| runnable(candidate))
}

#[cfg(unix)]
fn candidates(dir: &Path, name: &str) -> Vec<PathBuf> {
    vec![dir.join(name)]
}

/// Windows finds a program by its extension: `git` is `git.exe`, and `PATHEXT` lists the
/// extensions that count. A name that brought its own extension is taken as it is.
#[cfg(windows)]
fn candidates(dir: &Path, name: &str) -> Vec<PathBuf> {
    let named = dir.join(name);
    if named.extension().is_some() {
        return vec![named];
    }
    let pathext = std::env::var("PATHEXT").unwrap_or_default();
    let pathext = if pathext.trim().is_empty() {
        DEFAULT_PATHEXT
    } else {
        pathext.as_str()
    };
    pathext
        .split(';')
        .filter(|extension| !extension.is_empty())
        .map(|extension| dir.join(format!("{name}{extension}")))
        .collect()
}

fn runnable(path: &Path) -> bool {
    path.metadata().is_ok_and(|meta| {
        // Windows has no executable bit; being a file there is enough.
        meta.is_file() && (cfg!(windows) || crate::fs::is_executable(&meta))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_program_on_the_path_is_found() {
        let found = find_program(if cfg!(windows) { "cmd" } else { "sh" })
            .expect("a shell on the search path");
        assert!(found.is_absolute(), "{found:?}");
        assert!(found.is_file(), "{found:?}");
    }

    #[test]
    fn a_program_that_is_not_there_is_not_found() {
        assert_eq!(find_program("gitweft-no-such-program"), None);
    }
}
