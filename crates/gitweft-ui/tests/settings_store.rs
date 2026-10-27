//! `SettingsStore` in a GPUI test context: the debounced save, a
//! change that changes nothing, the blocking save on quit, and a file from a newer version
//! left alone. The settings file is the test's own (`init_with_file`), never the user's.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use gitweft_core::settings::{SettingsFile, ThemeChoice};
use gitweft_ui::SettingsStore;
use gpui_kit::TestAppContext;

/// A test context with the store's globals over `dir/settings.json`; returns both.
fn store(dir: &Path) -> (TestAppContext, PathBuf) {
    let path = dir.join("settings.json");
    let cx = TestAppContext::single();
    cx.update(|cx| SettingsStore::init_with_file(cx, &path));
    (cx, path)
}

fn set_theme(cx: &TestAppContext, theme: ThemeChoice) {
    cx.update(|cx| SettingsStore::update(cx, |settings| settings.appearance.theme = theme));
}

/// Lets `ms` milliseconds pass on the test clock and runs what became due.
fn wait(cx: &TestAppContext, ms: u64) {
    cx.executor().advance_clock(Duration::from_millis(ms));
    cx.run_until_parked();
}

fn saved_theme(path: &Path) -> ThemeChoice {
    SettingsFile::open(path).1.appearance.theme
}

#[test]
fn a_change_is_saved_once_things_are_quiet() {
    let dir = tempfile::tempdir().unwrap();
    let (cx, path) = store(dir.path());

    set_theme(&cx, ThemeChoice::Light);
    wait(&cx, 200);
    assert!(!path.exists(), "not yet: the save waits 400 ms");
    set_theme(&cx, ThemeChoice::Dark);
    wait(&cx, 300);
    assert!(!path.exists(), "each change starts the wait again");
    wait(&cx, 200);
    assert_eq!(saved_theme(&path), ThemeChoice::Dark);
}

#[test]
fn a_change_that_changes_nothing_notifies_no_one_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (cx, path) = store(dir.path());
    let notified = Rc::new(Cell::new(0));
    let seen = notified.clone();
    let _watch =
        cx.update(|cx| cx.observe_global::<SettingsStore>(move |_| seen.set(seen.get() + 1)));

    let theme = cx.read(|cx| SettingsStore::settings(cx).appearance.theme);
    set_theme(&cx, theme);
    wait(&cx, 1_000);
    assert_eq!(notified.get(), 0);
    assert!(!path.exists());

    let other = if theme == ThemeChoice::Dark {
        ThemeChoice::Light
    } else {
        ThemeChoice::Dark
    };
    set_theme(&cx, other);
    cx.run_until_parked();
    assert_eq!(notified.get(), 1, "a real change is observed");
}

#[test]
fn a_change_just_before_quitting_is_still_saved() {
    let dir = tempfile::tempdir().unwrap();
    let (cx, path) = store(dir.path());
    set_theme(&cx, ThemeChoice::Dark);
    cx.quit();
    assert_eq!(saved_theme(&path), ThemeChoice::Dark);
}

#[test]
fn a_file_from_a_newer_version_is_read_but_never_written() {
    let dir = tempfile::tempdir().unwrap();
    let text = r#"{"version": 99, "appearance": {"theme": "light"}}"#;
    std::fs::write(dir.path().join("settings.json"), text).unwrap();
    let (cx, path) = store(dir.path());
    let theme = cx.read(|cx| SettingsStore::settings(cx).appearance.theme);
    assert_eq!(theme, ThemeChoice::Light, "it's still read");

    set_theme(&cx, ThemeChoice::Dark);
    wait(&cx, 1_000);
    cx.quit();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        text,
        "never written"
    );
}
