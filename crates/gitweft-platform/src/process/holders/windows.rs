use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS, FALSE};
use windows_sys::Win32::System::RestartManager::{
    CCH_RM_SESSION_KEY, RM_PROCESS_INFO, RmEndSession, RmGetList, RmRegisterResources,
    RmStartSession,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};

use super::{LockHolder, is_git};

/// The longest path Windows has, in UTF-16 units.
const MAX_PATH_LEN: u32 = 32_768;

/// How often the list of processes is asked for again when it grew meanwhile.
const LIST_TRIES: usize = 5;

/// Asks the Restart Manager which processes have `lock` open, and keeps those whose program
/// is git's. `dirs` goes unused: another process's working directory can't be read here.
pub(super) fn lock_holders(lock: &Path, _dirs: &[PathBuf]) -> Vec<LockHolder> {
    using_file(lock)
        .into_iter()
        .filter_map(|pid| {
            let program = program(pid)?;
            let name = program.file_stem()?.to_string_lossy().to_ascii_lowercase();
            is_git(&name).then(|| LockHolder {
                pid,
                command: program.to_string_lossy().into_owned(),
            })
        })
        .collect()
}

/// A Restart Manager session, ended on drop.
struct Session(u32);

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: the handle came from RmStartSession and is ended once, here.
        unsafe { RmEndSession(self.0) };
    }
}

/// The pids of the processes that have `path` open.
fn using_file(path: &Path) -> Vec<u32> {
    let Ok(path) = std::path::absolute(path) else {
        return Vec::new();
    };
    let path: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut handle = 0;
    let mut key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
    // SAFETY: `key` has room for the session key and its terminating NUL, as the call
    // requires; the handle goes into a local.
    if unsafe { RmStartSession(&mut handle, 0, key.as_mut_ptr()) } != ERROR_SUCCESS {
        return Vec::new();
    }
    let session = Session(handle);
    let files = [path.as_ptr()];
    // SAFETY: one file name, a NUL-terminated wide string that outlives the call, and no
    // applications or services.
    let registered = unsafe {
        RmRegisterResources(
            session.0,
            1,
            files.as_ptr(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
        )
    };
    if registered != ERROR_SUCCESS {
        return Vec::new();
    }
    let mut processes: Vec<RM_PROCESS_INFO> = Vec::new();
    for _ in 0..LIST_TRIES {
        let mut needed = 0;
        let mut count = u32::try_from(processes.len()).unwrap_or(u32::MAX);
        let mut reasons = 0;
        let buffer = if processes.is_empty() {
            std::ptr::null_mut()
        } else {
            processes.as_mut_ptr()
        };
        // SAFETY: `buffer` has room for `count` entries, or is null with a count of zero,
        // which only asks how many there are; the counts and the reasons go into locals.
        let listed = unsafe { RmGetList(session.0, &mut needed, &mut count, buffer, &mut reasons) };
        match listed {
            ERROR_SUCCESS => {
                processes.truncate(count as usize);
                return processes
                    .iter()
                    .map(|process| process.Process.dwProcessId)
                    .collect();
            }
            // More processes than room: again, with room for them all.
            ERROR_MORE_DATA => {
                // SAFETY: the struct is plain integers and arrays of them, for which zero
                // is a valid value.
                let empty: RM_PROCESS_INFO = unsafe { std::mem::zeroed() };
                processes = vec![empty; needed as usize];
            }
            _ => return Vec::new(),
        }
    }
    Vec::new()
}

/// The path of the process's program, if this user may look at the process.
fn program(pid: u32) -> Option<PathBuf> {
    // SAFETY: plain call; the handle it returns is checked below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) };
    if process.is_null() {
        return None;
    }
    // SAFETY: `process` is a fresh handle that nothing else owns.
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    let mut path = vec![0u16; MAX_PATH_LEN as usize];
    let mut len = MAX_PATH_LEN;
    // SAFETY: the handle is open with the access the call needs, and `path` has room for
    // `len` units; the call sets `len` to the length it wrote.
    let found = unsafe {
        QueryFullProcessImageNameW(
            process.as_raw_handle(),
            PROCESS_NAME_WIN32,
            path.as_mut_ptr(),
            &mut len,
        )
    };
    if found == FALSE {
        return None;
    }
    path.truncate(len as usize);
    Some(PathBuf::from(OsString::from_wide(&path)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_with_the_file_open_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("index.lock");
        let file = std::fs::File::create(&lock).unwrap();
        assert!(using_file(&lock).contains(&std::process::id()));
        // This test isn't git.
        assert_eq!(lock_holders(&lock, &[]), []);
        drop(file);
        assert!(!using_file(&lock).contains(&std::process::id()));
    }

    #[test]
    fn the_program_of_this_process() {
        let program = program(std::process::id()).unwrap();
        assert_eq!(
            program.file_name(),
            std::env::current_exe().unwrap().file_name()
        );
    }
}
