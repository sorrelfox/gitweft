//! The fonts and date format in force, for code that has no `App` at hand: paint closures,
//! row builders, formatting helpers. [`apply`](crate::apply) sets them; anything reads them.

use std::sync::RwLock;
use std::sync::atomic::{AtomicU8, Ordering};

use gitweft_core::settings::DateFormat;
use gpui_kit::SharedString;

use crate::fonts::{MONO_FONT, UI_FONT};

static FONTS: RwLock<Option<(SharedString, SharedString)>> = RwLock::new(None);
static DATE_FORMAT: AtomicU8 = AtomicU8::new(0);

pub(crate) fn set(ui: SharedString, mono: SharedString, date_format: DateFormat) {
    *FONTS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((ui, mono));
    let code = match date_format {
        DateFormat::Full => 0,
        DateFormat::Short => 1,
        DateFormat::Relative => 2,
    };
    DATE_FORMAT.store(code, Ordering::Relaxed);
}

fn fonts() -> Option<(SharedString, SharedString)> {
    FONTS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// The interface font family (already checked to exist).
pub fn ui_font() -> SharedString {
    fonts().map_or(UI_FONT.into(), |(ui, _)| ui)
}

/// The code font family: SHAs, dates, paths, code (already checked to exist).
pub fn mono_font() -> SharedString {
    fonts().map_or(MONO_FONT.into(), |(_, mono)| mono)
}

pub fn date_format() -> DateFormat {
    match DATE_FORMAT.load(Ordering::Relaxed) {
        1 => DateFormat::Short,
        2 => DateFormat::Relative,
        _ => DateFormat::Full,
    }
}
