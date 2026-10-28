use std::ffi::c_int;
use std::path::{Path, PathBuf};

use libc::pid_t;

use super::{LockHolder, is_git};

/// Looks through libproc: a process whose name is git's, whose working directory can be
/// read (only this user's can) and lies inside a dir.
pub(super) fn lock_holders(_lock: &Path, dirs: &[PathBuf]) -> Vec<LockHolder> {
    all_pids()
        .into_iter()
        .filter_map(|pid| {
            if !is_git(&name(pid)?) {
                return None;
            }
            // Another user's process: its working directory can't be read.
            let cwd = cwd(pid)?;
            if !dirs.iter().any(|dir| cwd.starts_with(dir)) {
                return None;
            }
            let command = command_line(pid)
                .or_else(|| program(pid))
                .unwrap_or_default();
            Some(LockHolder {
                pid: u32::try_from(pid).ok()?,
                command,
            })
        })
        .collect()
}

/// Every process's pid but the kernel's.
fn all_pids() -> Vec<pid_t> {
    // SAFETY: a null buffer only asks how many processes there are.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let Ok(count) = usize::try_from(count) else {
        return Vec::new();
    };
    // Room to spare for the processes started meanwhile.
    let mut pids: Vec<pid_t> = vec![0; count + count / 4 + 16];
    let Ok(size) = c_int::try_from(size_of_val(pids.as_slice())) else {
        return Vec::new();
    };
    // SAFETY: the buffer is `size` bytes of pids; the call returns how many it wrote.
    let found = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), size) };
    let Ok(found) = usize::try_from(found) else {
        return Vec::new();
    };
    pids.truncate(found);
    pids.retain(|pid| *pid > 0);
    pids
}

/// The process's name, which is its program's file name.
fn name(pid: pid_t) -> Option<String> {
    let mut buf = [0u8; 64];
    // SAFETY: the buffer is as long as the size we pass; the call returns the length of the
    // name it wrote, and zero when there is none.
    let len = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    let name = buf.get(..usize::try_from(len).ok()?)?;
    (!name.is_empty()).then(|| String::from_utf8_lossy(name).into_owned())
}

fn cwd(pid: pid_t) -> Option<PathBuf> {
    // SAFETY: the struct is plain integers and arrays of them, for which zero is valid.
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = c_int::try_from(size_of::<libc::proc_vnodepathinfo>()).ok()?;
    // SAFETY: `info` is a proc_vnodepathinfo, the struct this flavor fills, and `size` is
    // its size. The call returns how many bytes it wrote, and fails for another user's
    // process.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            (&raw mut info).cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    let path: Vec<u8> = info
        .pvi_cdir
        .vip_path
        .as_flattened()
        .iter()
        .take_while(|c| **c != 0)
        // The C chars of the path, as the bytes they are.
        .map(|c| *c as u8)
        .collect();
    (!path.is_empty()).then(|| crate::bytes::path_buf_from_vec(path))
}

/// The arguments the process was started with, from `kern.procargs2`.
fn command_line(pid: pid_t) -> Option<String> {
    let mut argmax: c_int = 0;
    let mut size = size_of::<c_int>();
    let mut name = [libc::CTL_KERN, libc::KERN_ARGMAX];
    // SAFETY: the name has the two entries we pass, and the value, a c_int, goes into
    // `argmax`, whose size is `size`. Nothing is set.
    let read = unsafe {
        libc::sysctl(
            name.as_mut_ptr(),
            2,
            (&raw mut argmax).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if read != 0 {
        return None;
    }
    let mut buf = vec![0u8; usize::try_from(argmax).ok()?];
    let mut size = buf.len();
    let mut name = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    // SAFETY: the name has the three entries we pass, and the value goes into `buf`, whose
    // length is `size`; the call sets `size` to the length it wrote. Nothing is set.
    let read = unsafe {
        libc::sysctl(
            name.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if read != 0 {
        return None;
    }
    buf.truncate(size);
    arguments(&buf).filter(|command| !command.is_empty())
}

/// The arguments in a `kern.procargs2` value: the argument count as a native int, the path
/// the program was started from, NULs up to an aligned boundary, then that many
/// NUL-terminated arguments, and the environment after them.
fn arguments(procargs: &[u8]) -> Option<String> {
    let (argc, rest) = procargs.split_first_chunk::<4>()?;
    let argc = usize::try_from(i32::from_ne_bytes(*argc)).ok()?;
    let rest = &rest[rest.iter().position(|b| *b == 0)?..];
    let rest = &rest[rest.iter().position(|b| *b != 0)?..];
    let arguments: Vec<_> = rest
        .split(|b| *b == 0)
        .take(argc)
        .map(String::from_utf8_lossy)
        .collect();
    Some(arguments.join(" ").trim().to_owned())
}

/// The path of the process's program.
fn program(pid: pid_t) -> Option<String> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the buffer is as long as the size we pass; the call returns the length of the
    // path it wrote, and zero on failure.
    let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    let path = buf.get(..usize::try_from(len).ok()?)?;
    (!path.is_empty()).then(|| String::from_utf8_lossy(path).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_follow_the_program_path() {
        let mut procargs = 2i32.to_ne_bytes().to_vec();
        procargs.extend_from_slice(b"/usr/bin/git\0\0\0\0git\0status\0HOME=/Users/a\0");
        assert_eq!(arguments(&procargs).as_deref(), Some("git status"));

        let mut no_arguments = 0i32.to_ne_bytes().to_vec();
        no_arguments.extend_from_slice(b"/usr/bin/git\0\0PATH=/usr/bin\0");
        assert_eq!(arguments(&no_arguments).as_deref(), Some(""));

        assert_eq!(arguments(b"\x02\0"), None);
    }

    #[test]
    fn this_process_can_be_read() {
        let pid = pid_t::try_from(std::process::id()).unwrap();
        assert!(all_pids().contains(&pid));
        let here = crate::fs::canonicalize(&std::env::current_dir().unwrap()).unwrap();
        assert_eq!(cwd(pid), Some(here));
        let exe = std::env::current_exe().unwrap();
        let exe_name = exe.file_name().unwrap().to_str().unwrap();
        assert!(exe_name.starts_with(&name(pid).unwrap()), "{exe_name}");
        assert!(command_line(pid).is_some());
        assert!(program(pid).is_some());
    }
}
