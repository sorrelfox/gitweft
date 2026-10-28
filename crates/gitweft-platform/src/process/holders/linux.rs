use std::path::{Path, PathBuf};

use super::{LockHolder, is_git};

/// Looks through /proc: a process whose `comm` is git's, whose `cwd` link can be read (only
/// this user's can) and lies inside a dir.
pub(super) fn lock_holders(_lock: &Path, dirs: &[PathBuf]) -> Vec<LockHolder> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            let proc = entry.path();
            let comm = std::fs::read_to_string(proc.join("comm")).ok()?;
            if !is_git(comm.trim_end_matches('\n')) {
                return None;
            }
            // Another user's process: the link can't be read.
            let cwd = std::fs::read_link(proc.join("cwd")).ok()?;
            if !dirs.iter().any(|dir| cwd.starts_with(dir)) {
                return None;
            }
            let cmdline = std::fs::read(proc.join("cmdline")).unwrap_or_default();
            let command = String::from_utf8_lossy(&cmdline)
                .replace('\0', " ")
                .trim()
                .to_owned();
            Some(LockHolder { pid, command })
        })
        .collect()
}
