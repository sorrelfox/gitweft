use std::fs::Metadata;
use std::io;
use std::path::Path;

use windows_sys::Win32::Foundation::ERROR_PRIVILEGE_NOT_HELD;

/// A folder below the user's profile is theirs alone by the access list it inherits, so
/// there is nothing to set.
pub(super) fn create_private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)
}

pub(super) fn make_private(_path: &Path) -> io::Result<()> {
    Ok(())
}

pub(super) fn private_dir_problem(_meta: &Metadata) -> Option<&'static str> {
    None
}

/// Windows has no executable bit, and Git for Windows works with `core.fileMode` off.
pub(super) fn is_executable(_meta: &Metadata) -> bool {
    false
}

pub(super) fn set_executable(_path: &Path, _executable: bool) -> io::Result<()> {
    Ok(())
}

pub(super) fn symlink(target: &Path, link: &Path) -> io::Result<()> {
    match std::os::windows::fs::symlink_file(target, link) {
        Ok(()) => Ok(()),
        // Making a symlink needs a privilege an ordinary account doesn't have unless
        // developer mode is on. Without it, write what git itself writes with
        // `core.symlinks` off: a plain file holding the target's path.
        Err(err)
            if err.raw_os_error() == Some(ERROR_PRIVILEGE_NOT_HELD as i32)
                || err.kind() == io::ErrorKind::PermissionDenied =>
        {
            std::fs::write(link, target.as_os_str().as_encoded_bytes())
        }
        Err(err) => Err(err),
    }
}
