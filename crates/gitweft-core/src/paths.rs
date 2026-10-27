//! Where Gitweft keeps its own files (XDG base directories, local only).

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// `$XDG_DATA_HOME/gitweft` (default `~/.local/share/gitweft`).
pub fn data_dir() -> PathBuf {
    #[cfg(feature = "test-support")]
    if let Some(dir) = TEST_DATA_DIR.get() {
        return dir.clone();
    }
    xdg("XDG_DATA_HOME", ".local/share").join("gitweft")
}

#[cfg(feature = "test-support")]
static TEST_DATA_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Tests that run the workspace: [`data_dir`] (undo logs, repository settings, the trash)
/// is `dir` from now on, for the whole process, instead of the user's. The first call wins;
/// repositories in different sandboxes still get folders of their own.
#[cfg(feature = "test-support")]
pub fn use_data_dir_for_tests(dir: PathBuf) {
    let _ = TEST_DATA_DIR.set(dir);
}

/// `$XDG_CONFIG_HOME/gitweft` (default `~/.config/gitweft`): the app-wide settings.
pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("gitweft")
}

/// Where discarded untracked files are moved instead of being deleted.
pub fn trash_dir() -> PathBuf {
    data_dir().join("trash")
}

/// Our files about the repository at `root` (its working tree, or git dir when bare):
/// `<data dir>/repos/<its gitweft.id>` (see [`crate::repo_id`]), or before it has
/// one, `<data dir>/repos/<folder name>-<hash of the path>`.
pub fn repo_data_dir(root: &Path) -> PathBuf {
    repo_data_dir_in(&data_dir(), root)
}

/// [`repo_data_dir`] under the data dir `data` (tests pass their own).
pub fn repo_data_dir_in(data: &Path, root: &Path) -> PathBuf {
    match crate::repo_id::repo_key(root) {
        Some(key) => data.join("repos").join(key),
        None => legacy_repo_data_dir_in(data, root),
    }
}

/// The folder named after `root`'s path, used before the repository had an id (and moved
/// to the id's folder when it gets one).
pub fn legacy_repo_data_dir_in(data: &Path, root: &Path) -> PathBuf {
    let name = file_name_part(&root.file_name().unwrap_or_default().to_string_lossy());
    let hash = fnv1a(root.as_os_str().as_bytes());
    data.join("repos").join(format!("{name}-{hash:016x}"))
}

/// `text` with anything but letters, digits, `-`, `_` and `.` replaced by `_`, for
/// file names that stay readable.
pub fn file_name_part(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// 64-bit FNV-1a: stable across Rust releases (std's hasher isn't), which a file name
/// needs.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// This process's user id.
pub fn own_uid() -> u32 {
    // SAFETY: getuid has no preconditions and never fails.
    unsafe { libc::getuid() }
}

fn xdg(variable: &str, fallback: &str) -> PathBuf {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .or_else(passwd_home);
    xdg_from(std::env::var_os(variable), home, fallback)
}

/// This user's home directory as the password database has it (`getpwuid_r`): for a
/// process started without `HOME`.
fn passwd_home() -> Option<OsString> {
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
    let dir = unsafe { std::ffi::CStr::from_ptr(entry.pw_dir) };
    let dir = std::ffi::OsStr::from_bytes(dir.to_bytes());
    (!dir.is_empty()).then(|| dir.to_owned())
}

/// An XDG base directory: `value` if it's an absolute path (the spec says to ignore
/// relative ones), otherwise `fallback` under `home`. With no home at all (neither `HOME`
/// nor a passwd entry), a folder of this user's in the temp dir rather than the temp dir
/// itself, which every user shares.
fn xdg_from(value: Option<OsString>, home: Option<OsString>, fallback: &str) -> PathBuf {
    value
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| {
            home.map(PathBuf::from)
                .unwrap_or_else(|| std::env::temp_dir().join(format!("gitweft-home-{}", own_uid())))
                .join(fallback)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_directories() {
        let home = || Some(OsString::from("/home/u"));
        assert_eq!(
            xdg_from(Some("/cfg".into()), home(), ".config"),
            PathBuf::from("/cfg")
        );
        assert_eq!(
            xdg_from(None, home(), ".config"),
            PathBuf::from("/home/u/.config")
        );
        // A relative value is ignored, as the XDG spec says.
        assert_eq!(
            xdg_from(Some("cfg".into()), home(), ".config"),
            PathBuf::from("/home/u/.config")
        );
        // No home at all: this user's own folder, not the shared temp dir.
        assert_eq!(
            xdg_from(None, None, ".config"),
            std::env::temp_dir()
                .join(format!("gitweft-home-{}", own_uid()))
                .join(".config")
        );
    }

    #[test]
    fn the_password_database_knows_this_users_home() {
        let home = passwd_home().expect("a passwd entry for the test user");
        assert!(Path::new(&home).is_absolute(), "{home:?}");
    }
}
