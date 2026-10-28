use std::ffi::{CStr, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use super::{absolute, base_dir, home};

/// macOS puts both an application's data and its settings in one place of its own; the
/// rest of Unix follows the XDG spec.
#[cfg(target_os = "macos")]
const DATA_FALLBACK: &str = "Library/Application Support";
#[cfg(not(target_os = "macos"))]
const DATA_FALLBACK: &str = ".local/share";
#[cfg(target_os = "macos")]
const CONFIG_FALLBACK: &str = "Library/Application Support";
#[cfg(not(target_os = "macos"))]
const CONFIG_FALLBACK: &str = ".config";

pub(super) fn data_home() -> PathBuf {
    base_dir(absolute("XDG_DATA_HOME"), home(), DATA_FALLBACK)
}

pub(super) fn config_home() -> PathBuf {
    base_dir(absolute("XDG_CONFIG_HOME"), home(), CONFIG_FALLBACK)
}

/// Without a runtime directory (macOS has none, and neither has a session started by hand),
/// a folder of this user's own in the temp dir.
pub(super) fn socket_dir() -> PathBuf {
    std::env::temp_dir().join(format!("gitweft-{}", own_uid()))
}

pub(super) fn user_part() -> String {
    own_uid().to_string()
}

/// This user's home directory as the password database has it (`getpwuid_r`): for a
/// process started without `HOME`.
pub(super) fn system_home() -> Option<PathBuf> {
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    // SAFETY: a zeroed passwd is a valid value for getpwuid_r to fill in.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: getpwuid_r writes into `entry` and `buf`, which outlive the call and whose
    // sizes are the ones given.
    let status = unsafe {
        libc::getpwuid_r(
            own_uid(),
            &mut entry,
            buf.as_mut_ptr(),
            buf.len(),
            &mut found,
        )
    };
    if status != 0 || found.is_null() || entry.pw_dir.is_null() {
        return None;
    }
    // SAFETY: pw_dir points at a NUL-terminated string inside `buf`, still alive.
    let dir = unsafe { CStr::from_ptr(entry.pw_dir) };
    let dir = OsStr::from_bytes(dir.to_bytes());
    (!dir.is_empty()).then(|| PathBuf::from(dir))
}

/// This process's user id.
pub(crate) fn own_uid() -> u32 {
    // SAFETY: getuid has no preconditions and never fails.
    unsafe { libc::getuid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_password_database_knows_this_users_home() {
        let home = system_home().expect("a passwd entry for the test user");
        assert!(home.is_absolute(), "{home:?}");
    }
}
