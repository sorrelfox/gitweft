//! The last session: the repositories open as tabs, which one was active, and the window's
//! size, so the next start can reopen them (the "Reopen repositories" setting).
//!
//! Kept in `$XDG_DATA_HOME/gitweft/session` as lines of text, because paths need not be
//! UTF-8 (JSON can't hold them):
//!
//! ```text
//! gitweft-session 1
//! window 1280 800 windowed
//! active 1
//! tab /home/me/project
//! tab /home/me/other
//! ```
//!
//! Reading is lenient: unknown or broken lines are skipped, so a file from a newer build
//! still gives its tabs. Callers read it once at start and write it off the UI thread.

use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::paths::data_dir;

const HEADER: &str = "gitweft-session";
const VERSION: u32 = 1;
/// More tabs than this aren't reopened.
pub const MAX_TABS: usize = 50;
/// Window sizes outside this range aren't restored.
const WINDOW_SIZE: (f32, f32) = (200.0, 20_000.0);

pub fn session_file() -> PathBuf {
    data_dir().join("session")
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Session {
    /// Each tab's repository (its working tree, or git dir when bare), in tab order.
    pub tabs: Vec<PathBuf>,
    /// The active tab, when it is one of `tabs`.
    pub active: Option<usize>,
    pub window: Option<WindowSize>,
}

/// The window's size (its size before maximizing, when maximized).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowSize {
    pub width: f32,
    pub height: f32,
    pub maximized: bool,
}

impl WindowSize {
    fn valid(self) -> bool {
        [self.width, self.height]
            .iter()
            .all(|value| value.is_finite() && (WINDOW_SIZE.0..=WINDOW_SIZE.1).contains(value))
    }
}

/// The session in `file`; empty when it's missing, unreadable or not a session file.
pub fn load(file: &Path) -> Session {
    std::fs::read(file).map_or_else(|_| Session::default(), |bytes| parse(&bytes))
}

fn parse(bytes: &[u8]) -> Session {
    let mut lines = bytes.split(|&byte| byte == b'\n');
    let header_ok = lines.next().is_some_and(|line| {
        std::str::from_utf8(line)
            .ok()
            .and_then(|line| line.strip_prefix(HEADER))
            .is_some_and(|rest| rest.starts_with(' '))
    });
    let mut session = Session::default();
    if !header_ok {
        return session;
    }
    let mut active = None;
    for line in lines {
        if let Some(path) = line.strip_prefix(b"tab ") {
            let path = PathBuf::from(std::ffi::OsStr::from_bytes(path));
            if path.is_absolute() && session.tabs.len() < MAX_TABS {
                session.tabs.push(path);
            }
            continue;
        }
        let Ok(line) = std::str::from_utf8(line) else {
            continue;
        };
        let mut words = line.split_whitespace();
        match words.next() {
            Some("active") => active = words.next().and_then(|n| n.parse::<usize>().ok()),
            Some("window") => {
                let width = words.next().and_then(|n| n.parse::<f32>().ok());
                let height = words.next().and_then(|n| n.parse::<f32>().ok());
                let maximized = words.next() == Some("maximized");
                if let (Some(width), Some(height)) = (width, height) {
                    let size = WindowSize {
                        width,
                        height,
                        maximized,
                    };
                    session.window = size.valid().then_some(size);
                }
            }
            _ => {}
        }
    }
    session.active = active.filter(|&ix| ix < session.tabs.len());
    session
}

/// Writes `session` to `file` (a temporary file, then a rename). Tabs whose path can't be
/// stored in a line (relative, or holding a newline) are left out.
pub fn save(file: &Path, session: &Session) -> io::Result<()> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut bytes = format!("{HEADER} {VERSION}\n").into_bytes();
    if let Some(size) = session.window.filter(|size| size.valid()) {
        let state = if size.maximized {
            "maximized"
        } else {
            "windowed"
        };
        bytes.extend(format!("window {} {} {state}\n", size.width, size.height).bytes());
    }
    let mut active = None;
    let mut stored = 0;
    for (ix, path) in session.tabs.iter().enumerate() {
        let raw = path.as_os_str().as_bytes();
        if !path.is_absolute() || raw.contains(&b'\n') || stored == MAX_TABS {
            continue;
        }
        if session.active == Some(ix) {
            active = Some(stored);
        }
        bytes.extend_from_slice(b"tab ");
        bytes.extend_from_slice(raw);
        bytes.push(b'\n');
        stored += 1;
    }
    if let Some(active) = active {
        bytes.extend(format!("active {active}\n").bytes());
    }
    // The last window's tabs win (by design), but two instances saving at once each write
    // their own temporary file (#7).
    crate::datafile::atomic_write(file, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(tabs: &[&str], active: Option<usize>, window: Option<WindowSize>) -> Session {
        Session {
            tabs: tabs.iter().map(PathBuf::from).collect(),
            active,
            window,
        }
    }

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sub/session");
        assert_eq!(load(&file), Session::default());

        let size = WindowSize {
            width: 1456.0,
            height: 912.5,
            maximized: true,
        };
        let written = session(&["/a", "/b c"], Some(1), Some(size));
        save(&file, &written).unwrap();
        assert!(!file.with_extension("tmp").exists());
        assert_eq!(load(&file), written);

        // No active tab (a start screen was active) and no window size.
        let written = session(&["/a"], None, None);
        save(&file, &written).unwrap();
        assert_eq!(load(&file), written);
    }

    #[test]
    fn non_utf8_paths_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("session");
        let odd = PathBuf::from(std::ffi::OsStr::from_bytes(b"/repos/caf\xe9"));
        let written = Session {
            tabs: vec![odd],
            active: Some(0),
            window: None,
        };
        save(&file, &written).unwrap();
        assert_eq!(load(&file), written);
    }

    #[test]
    fn unstorable_tabs_are_skipped_and_the_active_one_follows() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("session");
        save(
            &file,
            &session(&["relative", "/with\nnewline", "/a", "/b"], Some(3), None),
        )
        .unwrap();
        assert_eq!(load(&file), session(&["/a", "/b"], Some(1), None));
        // The active tab itself unstorable: none is active.
        save(&file, &session(&["/a", "relative"], Some(1), None)).unwrap();
        assert_eq!(load(&file), session(&["/a"], None, None));
    }

    #[test]
    fn reading_is_lenient() {
        // Not a session file at all.
        assert_eq!(parse(b"tab /a\n"), Session::default());
        assert_eq!(parse(b""), Session::default());
        // A newer version: known lines are read, others skipped.
        let text = b"gitweft-session 7\nsparkles on\ntab /a\ntab rel\nactive 9\nwindow wide 3\n\xff\xfe\ntab /b\n";
        assert_eq!(parse(text), session(&["/a", "/b"], None, None));
        // Sizes out of range are dropped, the rest kept.
        let text = b"gitweft-session 1\nwindow 50 800 windowed\ntab /a\nactive 0\n";
        assert_eq!(parse(text), session(&["/a"], Some(0), None));
        let text = b"gitweft-session 1\nwindow 900 NaN windowed\n";
        assert_eq!(parse(text).window, None);
        let text = b"gitweft-session 1\nwindow 900 700\n";
        assert_eq!(
            parse(text).window,
            Some(WindowSize {
                width: 900.0,
                height: 700.0,
                maximized: false
            })
        );
    }

    #[test]
    fn tabs_are_capped() {
        let mut text = b"gitweft-session 1\n".to_vec();
        for n in 0..MAX_TABS + 10 {
            text.extend(format!("tab /r{n}\n").bytes());
        }
        let parsed = parse(&text);
        assert_eq!(parsed.tabs.len(), MAX_TABS);
        assert_eq!(parsed.tabs[0], PathBuf::from("/r0"));
    }
}
