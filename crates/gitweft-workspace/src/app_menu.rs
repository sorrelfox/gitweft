//! The menu bar. Only macOS shows it; GPUI keeps it on the other systems, where the
//! window has its own title bar and the commands are in the palette.
//!
//! Its items run the app's own commands, so each shows the key the user has for it: the
//! menus are built again whenever the keymap changes (`keymap::apply`).

use gpui_kit::component::input;
use gpui_kit::*;
use rust_i18n::t;

use crate::settings_window::OpenSettings;
use crate::{APP_NAME, graph_view, shell, workspace};

actions!(app, [Quit, Hide, HideOthers, ShowAll, Minimize, Zoom]);

pub(crate) fn bind_keys(cx: &mut App) {
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    cx.on_action(|_: &Minimize, cx| with_active_window(cx, Window::minimize_window));
    cx.on_action(|_: &Zoom, cx| with_active_window(cx, Window::zoom_window));
    // The keys every Mac app has. Elsewhere the desktop has its own for these.
    if cfg!(target_os = "macos") {
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-h", Hide, None),
            KeyBinding::new("alt-cmd-h", HideOthers, None),
            KeyBinding::new("cmd-m", Minimize, None),
        ]);
    }
}

fn with_active_window(cx: &mut App, f: impl FnOnce(&Window)) {
    if let Some(window) = cx.active_window() {
        window.update(cx, |_, window, _| f(window)).ok();
    }
}

/// Sets the menu bar, with the keys bound now.
pub(crate) fn set(cx: &mut App) {
    cx.set_menus(menus());
}

fn menus() -> Vec<Menu> {
    let item = |key: &str, action: &dyn Action| MenuItem::Action {
        name: t!(key).into_owned().into(),
        action: action.boxed_clone(),
        os_action: None,
        checked: false,
        disabled: false,
    };
    // Text editing: the system's own Edit items, run in the focused text field.
    let edit = |key: &str, action: &dyn Action, os_action: OsAction| MenuItem::Action {
        name: t!(key).into_owned().into(),
        action: action.boxed_clone(),
        os_action: Some(os_action),
        checked: false,
        disabled: false,
    };
    vec![
        Menu::new(APP_NAME).items([
            item("palette.settings", &OpenSettings),
            MenuItem::separator(),
            MenuItem::os_submenu(t!("menu.services").into_owned(), SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action(t!("menu.hide", app = APP_NAME).into_owned(), Hide),
            item("menu.hide_others", &HideOthers),
            item("menu.show_all", &ShowAll),
            MenuItem::separator(),
            MenuItem::action(t!("menu.quit", app = APP_NAME).into_owned(), Quit),
        ]),
        Menu::new(t!("menu.file").into_owned()).items([
            item("start.open_repository", &shell::OpenRepository),
            MenuItem::separator(),
            item("palette.tab_new", &shell::NewTab),
            item("palette.tab_close", &shell::CloseTab),
        ]),
        Menu::new(t!("menu.edit").into_owned()).items([
            edit("menu.undo", &input::Undo, OsAction::Undo),
            edit("menu.redo", &input::Redo, OsAction::Redo),
            MenuItem::separator(),
            edit("menu.cut", &input::Cut, OsAction::Cut),
            edit("menu.copy", &input::Copy, OsAction::Copy),
            edit("menu.paste", &input::Paste, OsAction::Paste),
            edit("menu.select_all", &input::SelectAll, OsAction::SelectAll),
            MenuItem::separator(),
            item("menu.undo_operation", &workspace::UndoOperation),
            item("menu.redo_operation", &workspace::RedoOperation),
        ]),
        Menu::new(t!("menu.view").into_owned()).items([
            item("menu.command_palette", &shell::OpenCommandPalette),
            item("palette.search", &graph_view::FocusSearch),
            MenuItem::separator(),
            item("keyboard.action.next_panel", &workspace::FocusNextPanel),
            item(
                "keyboard.action.previous_panel",
                &workspace::FocusPreviousPanel,
            ),
            MenuItem::separator(),
            item("trash.show", &workspace::ShowTrash),
            item("dangling.show", &workspace::ShowDanglingCommits),
            item("lfs.locks_menu", &workspace::ShowLfsLocks),
        ]),
        // GPUI gives a menu named "Window" (in English) the system's list of windows.
        Menu::new(t!("menu.window").into_owned()).items([
            item("menu.minimize", &Minimize),
            item("menu.zoom", &Zoom),
            MenuItem::separator(),
            item("palette.tab_next", &shell::NextTab),
            item("palette.tab_previous", &shell::PreviousTab),
        ]),
    ]
}

#[cfg(test)]
mod tests {
    use std::prelude::v1::test;

    use super::*;

    fn names(items: &[OwnedMenuItem]) -> Vec<String> {
        items
            .iter()
            .map(|item| match item {
                OwnedMenuItem::Separator => "-".to_owned(),
                OwnedMenuItem::Submenu(menu) => menu.name.to_string(),
                OwnedMenuItem::SystemMenu(menu) => menu.name.to_string(),
                OwnedMenuItem::Action { name, .. } => name.clone(),
            })
            .collect()
    }

    #[test]
    fn the_menu_bar_has_the_app_menu_then_file_edit_view_and_window() {
        let cx = TestAppContext::single();
        cx.update(set);
        let menus = cx.update(|cx| cx.get_menus()).expect("menus are kept");
        let titles: Vec<&str> = menus.iter().map(|menu| menu.name.as_ref()).collect();
        assert_eq!(titles, [APP_NAME, "File", "Edit", "View", "Window"]);
        assert_eq!(
            names(&menus[0].items),
            [
                "Settings…",
                "-",
                "Services",
                "-",
                "Hide Gitweft",
                "Hide others",
                "Show all",
                "-",
                "Quit Gitweft",
            ]
        );
        // The text-editing items are the system's own, the last two the app's undo.
        let system: Vec<bool> = menus[2]
            .items
            .iter()
            .filter_map(|item| match item {
                OwnedMenuItem::Action { os_action, .. } => Some(os_action.is_some()),
                _ => None,
            })
            .collect();
        assert_eq!(system, [true, true, true, true, true, true, false, false]);
    }
}
