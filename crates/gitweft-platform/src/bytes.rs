//! Names as git prints them, read back as the system's own strings.
//!
//! Git speaks in bytes: with `core.quotepath=off` a path comes out exactly as it is stored,
//! which on Unix can be any byte sequence at all, while Git for Windows prints UTF-8. The
//! other direction needs nothing from here: std's `OsStr::as_encoded_bytes` and
//! `OsString::into_encoded_bytes` already give the bytes on Unix, and UTF-8 for every name
//! Windows can hold.

use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// `bytes` as an OS string: borrowed where the system can take them as they are.
#[cfg(unix)]
pub fn os_str_from_bytes(bytes: &[u8]) -> Cow<'_, OsStr> {
    use std::os::unix::ffi::OsStrExt;

    Cow::Borrowed(OsStr::from_bytes(bytes))
}

/// `bytes` as an OS string, read as the UTF-8 that Git for Windows prints. An invalid
/// sequence is replaced rather than refused: a name we cannot read is still a name the
/// user should see.
#[cfg(windows)]
pub fn os_str_from_bytes(bytes: &[u8]) -> Cow<'_, OsStr> {
    match String::from_utf8_lossy(bytes) {
        Cow::Borrowed(text) => Cow::Borrowed(OsStr::new(text)),
        Cow::Owned(text) => Cow::Owned(OsString::from(text)),
    }
}

/// [`os_str_from_bytes`] as a path.
pub fn path_from_bytes(bytes: &[u8]) -> Cow<'_, Path> {
    match os_str_from_bytes(bytes) {
        Cow::Borrowed(name) => Cow::Borrowed(Path::new(name)),
        Cow::Owned(name) => Cow::Owned(PathBuf::from(name)),
    }
}

/// [`os_str_from_bytes`] taking the bytes over, without copying them where it can.
#[cfg(unix)]
pub fn os_string_from_vec(bytes: Vec<u8>) -> OsString {
    use std::os::unix::ffi::OsStringExt;

    OsString::from_vec(bytes)
}

/// [`os_str_from_bytes`] taking the bytes over.
#[cfg(windows)]
pub fn os_string_from_vec(bytes: Vec<u8>) -> OsString {
    match String::from_utf8(bytes) {
        Ok(text) => OsString::from(text),
        Err(err) => OsString::from(String::from_utf8_lossy(err.as_bytes()).into_owned()),
    }
}

/// [`os_string_from_vec`] as a path.
pub fn path_buf_from_vec(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(os_string_from_vec(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_names_round_trip() {
        let name = "café/αβγ.txt";
        assert_eq!(os_str_from_bytes(name.as_bytes()), OsStr::new(name));
        assert_eq!(path_from_bytes(name.as_bytes()), Path::new(name));
        assert_eq!(os_string_from_vec(name.into()), OsString::from(name));
        assert_eq!(path_buf_from_vec(name.into()), PathBuf::from(name));
    }

    /// Latin-1 in a file name: not UTF-8, but a perfectly good name on Unix, and it must
    /// come back byte for byte.
    #[cfg(unix)]
    #[test]
    fn a_name_that_is_not_utf8_survives() {
        let bytes = b"caf\xe9";
        let name = os_str_from_bytes(bytes);
        assert!(matches!(name, Cow::Borrowed(_)));
        assert_eq!(name.as_encoded_bytes(), bytes);
        assert_eq!(
            os_string_from_vec(bytes.to_vec()).into_encoded_bytes(),
            bytes
        );
        assert_eq!(path_buf_from_vec(bytes.to_vec()).as_os_str(), name);
    }
}
