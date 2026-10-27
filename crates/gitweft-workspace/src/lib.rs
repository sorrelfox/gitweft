//! Gitweft application shell.

// First: its `tn!` macro is used by the modules below.
#[macro_use]
mod plural;

mod blame_view;
mod code_keys;
mod code_scroll;
mod commit_columns;
mod compare_view;
mod conflict_view;
mod dangling_view;
mod details_view;
mod diff_decor;
mod diff_view;
mod file_text;
mod file_view;
mod find;
mod graph_view;
mod history_view;
mod keymap;
mod list_keys;
mod locale;
mod palette;
mod prompt;
mod rebase_view;
mod reflog_view;
mod session;
mod settings_ui;
mod settings_window;
mod shell;
mod split;
mod staging_view;
mod tab_notes;
mod toast;
mod trash_view;
mod tree_view;
#[cfg(test)]
mod ui_test;
mod util;
mod workspace;

use std::path::PathBuf;
use std::time::Instant;

use crate::toast::Toast;
use gitweft_core::ipc::{self, IpcServer};
use gitweft_core::session::WindowSize;
use gpui_kit::component::{Root, TitleBar};
use gpui_kit::*;
use rust_i18n::t;

use crate::session::SessionStore;
use crate::shell::Shell;

// User-visible strings live in /locales (CONTRIBUTING.md, "Text on screen"); `locale.rs`
// adds the pseudo-language.
rust_i18n::i18n!(
    "../../locales",
    fallback = "en",
    backend = crate::locale::PseudoLocale
);

pub(crate) const APP_NAME: &str = "Gitweft";

/// The main window's size when no session recorded one.
const DEFAULT_WINDOW_SIZE: (f32, f32) = (1280., 800.);

/// The main window and its shell, for the paths other `gitweft` processes send.
struct MainWindow {
    handle: WindowHandle<Root>,
    shell: WeakEntity<Shell>,
}

impl Global for MainWindow {}

/// Keeps the single-instance socket open; dropped (and the socket removed) on quit.
struct Instance(#[allow(dead_code)] IpcServer);

impl Global for Instance {}

/// What the command line prints, in the user's language. Usable before [`run`]: each
/// chooses the language first.
pub mod cli_text {
    use rust_i18n::t;

    pub fn usage() -> String {
        crate::locale::init();
        t!("cli.usage").into_owned()
    }

    pub fn unknown_option(flag: &str) -> String {
        crate::locale::init();
        t!("cli.unknown_option", flag = flag).into_owned()
    }

    /// A Gitweft is running but didn't answer ([`gitweft_core::ipc::is_no_answer`]).
    pub fn no_answer(error: &std::io::Error) -> String {
        crate::locale::init();
        t!("cli.no_answer", error = error.to_string()).into_owned()
    }
}

/// Starts the GUI and blocks until the last window closes. The window reopens the last
/// session's tabs (the "Reopen repositories" setting) and one tab per path given; with
/// `single_instance`, later `gitweft PATH` commands open their paths in it.
/// `started` is the process start, used by `GITWEFT_TRACE_STARTUP` (see
/// `Workspace::trace_startup`). `other_unanswered`: a Gitweft is running but didn't answer
/// the hand-off, which the new window says.
///
/// Returns whether the window opened: `false` makes the process exit with a failure.
pub fn run(
    paths: Vec<PathBuf>,
    single_instance: bool,
    started: Instant,
    other_unanswered: bool,
) -> bool {
    let opened = std::rc::Rc::new(std::cell::Cell::new(true));
    let result = opened.clone();
    gpui_kit::application()
        // Our icons first, then gpui-kit's (see `gitweft_ui::icons`).
        .with_assets(gitweft_ui::AppAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            // After the kit's init, which may set its own default language.
            locale::init();
            // Confirmations in our theme (GPUI has no system prompt on Linux).
            cx.set_prompt_builder(prompt::build);
            gitweft_ui::fonts::register(cx);
            // Before the first window, so it opens in the chosen theme without a flash.
            gitweft_ui::SettingsStore::init(cx);
            bind_keys(cx);
            // Last: the keymap as registered so far is the base the user's keymap applies to.
            keymap::KeymapStore::init(cx);

            let last = SessionStore::init(cx);
            let restore = gitweft_ui::SettingsStore::settings(cx)
                .general
                .restore_session;
            let (tabs, active) = session::startup_tabs(&last, restore, paths);
            if single_instance {
                listen(cx);
            }
            open_main_window(
                tabs,
                active,
                last.window,
                Some(started),
                other_unanswered,
                opened,
                cx,
            );
        });
    result.get()
}

/// Every view's key bindings, as the app registers them (the headless UI tests too).
pub(crate) fn bind_keys(cx: &mut gpui_kit::App) {
    graph_view::bind_keys(cx);
    diff_view::bind_keys(cx);
    conflict_view::bind_keys(cx);
    blame_view::bind_keys(cx);
    history_view::bind_keys(cx);
    file_view::bind_keys(cx);
    find::bind_keys(cx);
    tree_view::bind_keys(cx);
    reflog_view::bind_keys(cx);
    dangling_view::bind_keys(cx);
    trash_view::bind_keys(cx);
    compare_view::bind_keys(cx);
    rebase_view::bind_keys(cx);
    list_keys::bind_keys(cx);
    gitweft_ui::widgets::bind_keys(cx);
    code_keys::bind_keys(cx);
    workspace::bind_keys(cx);
    shell::bind_keys(cx);
    settings_window::bind_keys(cx);
}

/// Opens the main window with a tab per path (the start screen when there are none),
/// showing tab `active`, at `size` (the default size when `None`).
/// `other_unanswered`: say in the window that another Gitweft didn't answer. A window
/// that can't open is reported, `opened` set to false, and the app quits.
fn open_main_window(
    tabs: Vec<PathBuf>,
    active: usize,
    size: Option<WindowSize>,
    started: Option<Instant>,
    other_unanswered: bool,
    opened: std::rc::Rc<std::cell::Cell<bool>>,
    cx: &mut App,
) {
    let (width, height) = size.map_or(DEFAULT_WINDOW_SIZE, |size| (size.width, size.height));
    let bounds = Bounds::centered(None, gpui_kit::size(px(width), px(height)), cx);
    let window_bounds = if size.is_some_and(|size| size.maximized) {
        WindowBounds::Maximized(bounds)
    } else {
        WindowBounds::Windowed(bounds)
    };
    let options = WindowOptions {
        window_bounds: Some(window_bounds),
        window_min_size: Some(gpui_kit::size(px(720.), px(480.))), // unscaled: a window's
        app_id: Some("io.github.sorrelfox.Gitweft".into()),
        // We draw our own title bar; ask the compositor not to add another.
        window_decorations: Some(WindowDecorations::Client),
        titlebar: Some(TitlebarOptions {
            title: Some(APP_NAME.into()),
            ..TitleBar::title_bar_options()
        }),
        ..TitleBar::window_options()
    };

    cx.spawn(async move |cx| {
        let mut shell = None;
        let opening = cx.open_window(options, |window, cx| {
            gitweft_ui::apply(window, cx);
            let env = workspace::WorkspaceEnv::app();
            let view = cx.new(|cx| Shell::new(tabs, active, started, env, window, cx));
            shell = Some(view.downgrade());
            cx.new(|cx| Root::new(view, window, cx))
        });
        let handle = match opening {
            Ok(handle) => handle,
            Err(err) => {
                eprintln!("{}", t!("cli.window_failed", error = err.to_string()));
                opened.set(false);
                cx.update(|cx| cx.quit());
                return;
            }
        };
        if other_unanswered {
            let text = t!("cli.no_answer_notice").into_owned();
            cx.update(|cx| toast::push(Toast::warning(text), cx));
        }
        if let Some(shell) = shell {
            cx.update(|cx| cx.set_global(MainWindow { handle, shell }));
        }
    })
    .detach();
}

/// Listens for other `gitweft` processes (`gitweft_core::ipc`) and opens what they send.
fn listen(cx: &mut App) {
    let (server, requests) = match IpcServer::start(&ipc::socket_path()) {
        Ok(started) => started,
        Err(err) => {
            eprintln!("gitweft: `gitweft PATH` won't reach this window: {err}");
            return;
        }
    };
    cx.set_global(Instance(server));
    cx.on_app_quit(|cx| {
        if cx.has_global::<Instance>() {
            cx.remove_global::<Instance>();
        }
        async {}
    })
    .detach();
    cx.spawn(async move |cx| {
        while let Ok(paths) = requests.recv().await {
            cx.update(|cx| open_requested(paths, cx));
        }
    })
    .detach();
}

/// Opens `paths` in the main window (a tab that shows one already comes forward) and
/// brings the window forward. With the main window closed (the settings window kept the
/// app running), a new one opens.
fn open_requested(paths: Vec<PathBuf>, cx: &mut App) {
    if let Some(main) = cx.try_global::<MainWindow>() {
        let (handle, shell) = (main.handle, main.shell.clone());
        let requested = paths.clone();
        let opened = handle.update(cx, |_, window, cx| {
            let opened = shell
                .update(cx, |shell, cx| shell.open_paths(requested, window, cx))
                .is_ok();
            if opened {
                window.activate_window();
            }
            opened
        });
        if matches!(opened, Ok(true)) {
            return;
        }
    }
    // Reopened later: its failure to open has no exit status to set.
    let opened = std::rc::Rc::new(std::cell::Cell::new(true));
    open_main_window(paths, 0, None, None, false, opened, cx);
}
