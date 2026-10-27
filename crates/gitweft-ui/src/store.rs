//! The settings as GPUI globals, and applying them to the window.
//!
//! - [`SettingsStore`]: the settings. Change them only through [`SettingsStore::update`];
//!   views that depend on them `observe_global::<SettingsStore>`.
//! - [`Look`]: what they resolve to for the window (palette, metrics, fonts), set by
//!   [`apply`]. Views read it through [`LookExt`]: `cx.palette()`, `cx.metrics()`.
//!
//! Saving is kept out of both globals, so a save never looks like a settings change.

use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gitweft_core::settings::{AppSettings, SettingsFile, ThemeChoice, settings_file};
use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::*;

use crate::fonts::{self, MONO_FONT, UI_FONT};
use crate::kit_theme::{Fonts, kit_config};
use crate::metrics::Metrics;
use crate::palette::Palette;

/// Settings changes are written this long after the last one, so dragging a slider or
/// typing a size writes once.
const SAVE_DELAY: Duration = Duration::from_millis(400);

pub struct SettingsStore {
    settings: AppSettings,
}

impl Global for SettingsStore {}

/// The settings file and the pending save. Not observed by anyone.
struct Persistence {
    file: Arc<Mutex<SettingsFile>>,
    dirty: bool,
    /// Each save's number, counting up: the background writes can finish out of order.
    generation: u64,
    /// The newest save written so far: an older snapshot never replaces it (#7).
    written: Arc<AtomicU64>,
    save_task: Option<Task<()>>,
    /// Families installed on this system, read once at start.
    installed_fonts: Vec<String>,
    _quit: Option<Subscription>,
}

impl Global for Persistence {}

/// What the settings resolve to for the window.
#[derive(Clone)]
pub struct Look {
    pub palette: Palette,
    pub metrics: Metrics,
    pub fonts: Rc<Fonts>,
}

impl Global for Look {}

impl SettingsStore {
    /// Reads the settings file and installs the globals. Call once, after the bundled
    /// fonts are registered and before the first window opens.
    pub fn init(cx: &mut App) {
        Self::init_with_file(cx, &settings_file());
    }

    /// [`Self::init`] with the settings in `path` (tests, #51).
    pub fn init_with_file(cx: &mut App, path: &Path) {
        let (file, settings) = SettingsFile::open(path);
        if !file.writable() {
            eprintln!(
                "gitweft: {} was written by a newer Gitweft; changes won't be saved to it",
                file.path().display()
            );
        }
        let installed_fonts = cx.text_system().all_font_names();
        let look = Look::resolve(&settings, false, &installed_fonts);
        cx.set_global(Persistence {
            file: Arc::new(Mutex::new(file)),
            dirty: false,
            generation: 0,
            written: Arc::new(AtomicU64::new(0)),
            save_task: None,
            installed_fonts,
            _quit: None,
        });
        cx.set_global(SettingsStore { settings });
        cx.set_global(look);
        // A change made less than SAVE_DELAY before quitting is still written.
        let quit = cx.on_app_quit(|cx| {
            Self::save_now(cx, true);
            async {}
        });
        cx.global_mut::<Persistence>()._quit = Some(quit);
    }

    pub fn settings(cx: &App) -> &AppSettings {
        &cx.global::<SettingsStore>().settings
    }

    /// Font families installed on this system (read once at start), for the font choices.
    pub fn installed_fonts(cx: &App) -> &[String] {
        &cx.global::<Persistence>().installed_fonts
    }

    /// Changes the settings. Values are clamped; observers run only if something changed,
    /// and the file is written shortly after.
    pub fn update(cx: &mut App, change: impl FnOnce(&mut AppSettings)) {
        let mut next = cx.global::<SettingsStore>().settings.clone();
        change(&mut next);
        let next = next.clamped();
        if next == cx.global::<SettingsStore>().settings {
            return;
        }
        cx.global_mut::<SettingsStore>().settings = next;
        let task = cx.spawn(async move |cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            cx.update(|cx| Self::save_now(cx, false));
        });
        let persistence = cx.global_mut::<Persistence>();
        persistence.dirty = true;
        persistence.save_task = Some(task);
    }

    /// Writes the settings if they changed since the last write: on a background thread,
    /// or right here when quitting.
    fn save_now(cx: &mut App, blocking: bool) {
        let persistence = cx.global_mut::<Persistence>();
        if !persistence.dirty {
            return;
        }
        persistence.dirty = false;
        persistence.generation += 1;
        let generation = persistence.generation;
        let written = persistence.written.clone();
        let file = persistence.file.clone();
        let snapshot = cx.global::<SettingsStore>().settings.clone();
        let write = move || {
            let mut file = file.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            // A newer snapshot got the file first: this older one mustn't replace it.
            if written.load(Ordering::SeqCst) > generation {
                return;
            }
            written.store(generation, Ordering::SeqCst);
            if let Err(err) = file.save(&snapshot) {
                eprintln!(
                    "gitweft: couldn't save the settings to {}: {err}",
                    file.path().display()
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

impl Look {
    fn resolve(settings: &AppSettings, dark: bool, installed: &[String]) -> Self {
        let appearance = &settings.appearance;
        Self {
            palette: Palette::resolve_preset(
                appearance.preset,
                dark,
                appearance.colors.for_mode(dark),
            ),
            metrics: Metrics::new(appearance),
            fonts: Rc::new(Fonts {
                ui: fonts::resolve_family(&appearance.ui_font, UI_FONT, installed),
                mono: fonts::resolve_family(&appearance.mono_font, MONO_FONT, installed),
            }),
        }
    }
}

/// Whether the settings make `window` dark: the desktop's choice for System.
pub fn is_dark(settings: &AppSettings, window: &Window) -> bool {
    match settings.appearance.theme {
        ThemeChoice::System => ThemeMode::from(window.appearance()).is_dark(),
        ThemeChoice::Light => false,
        ThemeChoice::Dark => true,
    }
}

/// Applies the settings to `window` and the kit theme: resolves the palette, metrics and
/// fonts, rebuilds the kit theme from them, and redraws every window. Call it when the
/// window opens, when the settings change, and when the desktop switches light/dark.
pub fn apply(window: &mut Window, cx: &mut App) {
    let settings = cx.global::<SettingsStore>().settings.clone();
    let dark = is_dark(&settings, window);
    let look = Look::resolve(&settings, dark, &cx.global::<Persistence>().installed_fonts);
    let config = Rc::new(kit_config(&look.palette, &look.fonts, &look.metrics));
    {
        let theme = Theme::global_mut(cx);
        if dark {
            theme.dark_theme = config;
        } else {
            theme.light_theme = config;
        }
    }
    let mode = if dark {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    };
    Theme::change(mode, Some(window), cx);
    // Top-right toasts covered the operation banner's Continue/Abort (and hovering one
    // keeps it open). The lower sidebar is the least important place to cover.
    Theme::global_mut(cx).notification.placement = Anchor::BottomLeft;
    crate::current::set(
        look.fonts.ui.clone(),
        look.fonts.mono.clone(),
        settings.general.date_format,
    );
    cx.set_global(look);
    cx.refresh_windows();
}

/// `cx.palette()`, `cx.metrics()`, `cx.look()`: the resolved settings, for views.
pub trait LookExt {
    fn look(&self) -> &Look;

    fn palette(&self) -> &Palette {
        &self.look().palette
    }

    fn metrics(&self) -> &Metrics {
        &self.look().metrics
    }
}

impl LookExt for App {
    fn look(&self) -> &Look {
        self.global::<Look>()
    }
}
