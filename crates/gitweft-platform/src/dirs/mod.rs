//! Where Gitweft keeps its own files: local only, and each system's usual place for them.
//! The callers add their own `gitweft` folder below [`data_home`] and [`config_home`].

use std::path::PathBuf;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as imp;
#[cfg(unix)]
pub(crate) use unix::own_uid;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as imp;

/// This user's home directory: `HOME` when it is set and not empty, otherwise what the
/// system itself knows about them (a process can be started without `HOME`).
pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(imp::system_home)
}

/// Where per-repository state, the undo logs and the trash live: `$XDG_DATA_HOME`, which is
/// honoured on every system because tests and users with several profiles set it, else the
/// system's own place for an application's data.
pub fn data_home() -> PathBuf {
    imp::data_home()
}

/// Where the settings live: `$XDG_CONFIG_HOME`, else the system's own place for them.
pub fn config_home() -> PathBuf {
    imp::config_home()
}

/// The folder for Gitweft's own sockets, which only this user may enter (it is not created
/// here; see [`crate::fs::create_private_dir`]). On a session that has one, the runtime
/// directory, which the system empties when the session ends.
pub fn socket_dir() -> PathBuf {
    match absolute("XDG_RUNTIME_DIR") {
        Some(runtime) => runtime.join("gitweft"),
        None => imp::socket_dir(),
    }
}

/// `variable`'s value if it is an absolute path. A relative one is ignored, as the XDG spec
/// says, and so is a path the system handed us that we could not act on anyway.
fn absolute(variable: &str) -> Option<PathBuf> {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// A base directory: `value` if it's an absolute path, otherwise `fallback` under `home`.
/// With no home at all, a folder of this user's in the temp dir rather than the temp dir
/// itself, which every user shares.
fn base_dir(value: Option<PathBuf>, home: Option<PathBuf>, fallback: &str) -> PathBuf {
    value.filter(|path| path.is_absolute()).unwrap_or_else(|| {
        home.unwrap_or_else(|| {
            std::env::temp_dir().join(format!("gitweft-home-{}", imp::user_part()))
        })
        .join(fallback)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_directories() {
        let home = || Some(PathBuf::from("/home/u"));
        assert_eq!(
            base_dir(Some("/cfg".into()), home(), ".config"),
            PathBuf::from("/cfg")
        );
        assert_eq!(
            base_dir(None, home(), ".config"),
            PathBuf::from("/home/u/.config")
        );
        // A relative value is ignored, as the XDG spec says.
        assert_eq!(
            base_dir(Some("cfg".into()), home(), ".config"),
            PathBuf::from("/home/u/.config")
        );
        // No home at all: this user's own folder, not the shared temp dir.
        assert_eq!(
            base_dir(None, None, ".config"),
            std::env::temp_dir()
                .join(format!("gitweft-home-{}", imp::user_part()))
                .join(".config")
        );
    }

    #[test]
    fn the_directories_are_absolute() {
        for dir in [data_home(), config_home(), socket_dir()] {
            assert!(dir.is_absolute(), "{dir:?}");
        }
    }

    #[test]
    fn the_user_part_is_a_file_name() {
        let part = imp::user_part();
        assert!(!part.is_empty());
        assert!(
            part.chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.')),
            "{part}"
        );
    }
}
