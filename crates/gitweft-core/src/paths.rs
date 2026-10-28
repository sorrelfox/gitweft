//! Where Gitweft keeps its own files, local only: the XDG base directories on Linux, the
//! system's own folders elsewhere (see [`gitweft_platform::dirs`]).

use std::path::{Path, PathBuf};

use gitweft_platform::dirs;

/// `$XDG_DATA_HOME/gitweft` (on Linux by default `~/.local/share/gitweft`).
pub fn data_dir() -> PathBuf {
    #[cfg(feature = "test-support")]
    if let Some(dir) = TEST_DATA_DIR.get() {
        return dir.clone();
    }
    dirs::data_home().join("gitweft")
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

/// `$XDG_CONFIG_HOME/gitweft` (on Linux by default `~/.config/gitweft`): the app-wide
/// settings.
pub fn config_dir() -> PathBuf {
    dirs::config_home().join("gitweft")
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
    let hash = fnv1a(root.as_os_str().as_encoded_bytes());
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

/// `path`, written the Unix way (`/a/b`), as an absolute path on this system: Windows
/// wants a drive in front of it.
#[cfg(test)]
pub(crate) fn test_path(path: &str) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(format!("C:{path}"))
    } else {
        PathBuf::from(path)
    }
}
