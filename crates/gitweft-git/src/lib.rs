//! Git access for Gitweft.
//!
//! Reads that must be fast happen in-process (gix); every mutation and every network
//! operation runs through the user's own `git` binary via [`cli::GitCli`], so hooks,
//! credential helpers, ssh configuration and signing behave exactly as in a terminal.

pub mod activity;
pub mod askpass;
pub mod bisect;
pub mod branch;
pub mod cancel;
pub mod cli;
pub mod conflict_files;
pub mod display;
pub mod files;
pub mod flatpak;
pub mod lfs;
pub mod merge;
pub mod notes;
pub mod ops;
pub mod partial;
pub mod patch;
pub mod read;
pub mod rebase;
pub mod remote;
pub mod repair;
pub mod rewrite;
pub mod setup;
pub mod stash;
pub mod submodules;
pub mod tag;
pub mod tools;
pub mod trash;
pub mod undo;
pub mod worktrees;

pub use cancel::CancelToken;
pub use cli::{GitCli, GitError, GitErrorKind, GitOutput, GitVersion, Progress, StderrEvent};
/// Re-exported so dependents use exactly our gix version and feature set.
pub use gix;
pub use gix::ObjectId;

/// The path git and ssh run this program by when they call it back as a helper (the
/// sequence editor of an interactive rebase, askpass). It is `/proc/<pid>/exe` where there
/// is one (Linux): that reaches the running binary even after its file was replaced (a
/// package update, a rebuild), when [`std::env::current_exe`] names a path that is gone
/// ("… (deleted)"), and it is always the same version as the app it talks to.
pub fn helper_program() -> std::io::Result<std::path::PathBuf> {
    let running = std::path::PathBuf::from(format!("/proc/{}/exe", std::process::id()));
    if running.exists() {
        return Ok(running);
    }
    std::env::current_exe()
}

#[cfg(test)]
mod tests {
    #[test]
    fn helpers_run_the_running_binary() {
        use std::os::unix::fs::MetadataExt as _;
        let helper = std::fs::metadata(super::helper_program().unwrap()).unwrap();
        let exe = std::fs::metadata(std::env::current_exe().unwrap()).unwrap();
        assert_eq!((helper.dev(), helper.ino()), (exe.dev(), exe.ino()));
    }
}
