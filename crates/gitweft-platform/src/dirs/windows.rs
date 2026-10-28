use std::path::PathBuf;

use super::{absolute, base_dir, home};

pub(super) fn data_home() -> PathBuf {
    app_data("XDG_DATA_HOME", "LOCALAPPDATA", r"AppData\Local")
}

pub(super) fn config_home() -> PathBuf {
    app_data("XDG_CONFIG_HOME", "APPDATA", r"AppData\Roaming")
}

/// Windows has no runtime directory that is emptied for us, so the sockets live under the
/// data dir; what a crashed Gitweft leaves there is cleaned up when the next one starts.
pub(super) fn socket_dir() -> PathBuf {
    data_home().join("gitweft").join("run")
}

/// A name for this user that is safe in a file name.
pub(super) fn user_part() -> String {
    let name: String = std::env::var("USERNAME")
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    if name.is_empty() {
        "user".to_owned()
    } else {
        name
    }
}

pub(super) fn system_home() -> Option<PathBuf> {
    absolute("USERPROFILE")
}

/// `$XDG_…` first, so a test or a second profile can move the folder as it can on Unix,
/// then Windows' own place for it, then the place that one usually is.
fn app_data(xdg: &str, variable: &str, fallback: &str) -> PathBuf {
    let value = absolute(xdg).or_else(|| absolute(variable));
    base_dir(value, home(), fallback)
}
