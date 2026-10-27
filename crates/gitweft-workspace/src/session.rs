//! Session restore: the shell records its tabs and the window's size whenever they change;
//! they're written shortly after (and when quitting) and read back at the next start.

use std::path::PathBuf;
use std::time::Duration;

use gitweft_core::session::{self, Session};
use gpui_kit::*;

/// Written this long after the last change, so a window resize writes once.
const SAVE_DELAY: Duration = Duration::from_millis(400);

pub(crate) struct SessionStore {
    file: PathBuf,
    /// What the shell recorded last (at start: what the file holds).
    recorded: Session,
    dirty: bool,
    _save_task: Option<Task<()>>,
}

impl Global for SessionStore {}

impl SessionStore {
    /// Reads the last session and installs the store. Returns what was read.
    pub(crate) fn init(cx: &mut App) -> Session {
        let file = session::session_file();
        let last = session::load(&file);
        cx.set_global(SessionStore {
            file,
            recorded: last.clone(),
            dirty: false,
            _save_task: None,
        });
        // A change made less than SAVE_DELAY before quitting is still written.
        cx.on_app_quit(|cx| {
            Self::save_now(cx, true);
            async {}
        })
        .detach();
        last
    }

    /// The shell's tabs and window as they are now; written shortly if they changed.
    pub(crate) fn record(cx: &mut App, session: Session) {
        let Some(store) = cx.try_global::<SessionStore>() else {
            return;
        };
        if store.recorded == session {
            return;
        }
        let task = cx.spawn(async move |cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            cx.update(|cx| Self::save_now(cx, false));
        });
        let store = cx.global_mut::<SessionStore>();
        store.recorded = session;
        store.dirty = true;
        store._save_task = Some(task);
    }

    /// Writes the recorded session if it changed since the last write: on a background
    /// thread, or right here when quitting.
    fn save_now(cx: &mut App, blocking: bool) {
        let store = cx.global_mut::<SessionStore>();
        if !store.dirty {
            return;
        }
        store.dirty = false;
        let (file, snapshot) = (store.file.clone(), store.recorded.clone());
        let write = move || {
            if let Err(err) = session::save(&file, &snapshot) {
                eprintln!(
                    "gitweft: couldn't save the session to {}: {err}",
                    file.display()
                );
            }
        };
        if blocking {
            write();
        } else {
            cx.background_executor()
                .spawn(async move { write() })
                .detach();
        }
    }
}

/// The tabs to open at start and the one to show: the last session's tabs (when `restore`),
/// then each path given on the command line that isn't among them. The first given path is
/// shown; without one, the session's active tab.
pub(crate) fn startup_tabs(
    last: &Session,
    restore: bool,
    given: Vec<PathBuf>,
) -> (Vec<PathBuf>, usize) {
    let (mut tabs, active) = if restore {
        (last.tabs.clone(), last.active.unwrap_or(0))
    } else {
        (Vec::new(), 0)
    };
    let mut first_given = None;
    for path in given {
        let ix = match tabs.iter().position(|tab| *tab == path) {
            Some(ix) => ix,
            None => {
                tabs.push(path);
                tabs.len() - 1
            }
        };
        first_given.get_or_insert(ix);
    }
    (tabs, first_given.unwrap_or(active))
}

#[cfg(test)]
mod tests {
    // Not `super::*`: gpui_kit's glob brings a `test` macro of its own.
    use std::path::PathBuf;

    use gitweft_core::session::Session;

    use super::startup_tabs;

    fn paths(list: &[&str]) -> Vec<PathBuf> {
        list.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn startup_tabs_restore_then_add_the_given_paths() {
        let last = Session {
            tabs: paths(&["/a", "/b", "/c"]),
            active: Some(1),
            window: None,
        };
        // Nothing given: the session as it was.
        assert_eq!(
            startup_tabs(&last, true, Vec::new()),
            (paths(&["/a", "/b", "/c"]), 1)
        );
        // A new path is added and shown; one already restored isn't opened twice (a
        // trailing slash is the same path).
        assert_eq!(
            startup_tabs(&last, true, paths(&["/d", "/a"])),
            (paths(&["/a", "/b", "/c", "/d"]), 3)
        );
        assert_eq!(
            startup_tabs(&last, true, paths(&["/c/"])),
            (paths(&["/a", "/b", "/c"]), 2)
        );
        // Not restoring: only what was given (none: a start screen).
        assert_eq!(
            startup_tabs(&last, false, paths(&["/d", "/e", "/d"])),
            (paths(&["/d", "/e"]), 0)
        );
        assert_eq!(startup_tabs(&last, false, Vec::new()), (Vec::new(), 0));
        // No active tab recorded: the first.
        let last = Session {
            active: None,
            ..last
        };
        assert_eq!(startup_tabs(&last, true, Vec::new()).1, 0);
    }
}
