//! App-wide settings, kept in `$XDG_CONFIG_HOME/gitweft/settings.json` (default
//! `~/.config/gitweft`). Per-repository state stays in the data dir (`repo_settings`).
//!
//! The file is forgiving in both directions:
//! - a missing, unreadable or corrupt file gives the defaults (a corrupt one is kept as
//!   `settings.json.bak` when the settings are first saved again);
//! - a field that's missing or has a value this build doesn't understand falls back to its
//!   default on its own, without losing the rest;
//! - keys this build doesn't know (written by a newer build) are kept when saving;
//! - a file written by a newer format version is read, but never overwritten.
//!
//! Values are clamped to what the UI can show. Loading is a small local read done once
//! before the first window; saving happens off the UI thread.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::paths::config_dir;

/// The format version this build writes.
pub const VERSION: u32 = 1;

pub const DEFAULT_UI_FONT: &str = "IBM Plex Sans";
pub const DEFAULT_MONO_FONT: &str = "IBM Plex Mono";
pub const UI_FONT_SIZE: (f32, f32, f32) = (11.0, 13.0, 16.0);
pub const MONO_FONT_SIZE: (f32, f32, f32) = (10.0, 12.0, 16.0);
pub const ZOOM: (f32, f32, f32) = (0.8, 1.0, 1.5);
/// Limits for remembered graph column widths, in pixels.
pub const WIDTH: (f32, f32) = (40.0, 1600.0);
/// Limits for the side panels' widths (design pixels): the app keeps them within the
/// window; this only keeps a hand-edited file sane (no maximum of their own).
pub const PANEL_WIDTH: (f32, f32) = (40.0, 20_000.0);
/// Limits for a section's share of the room it divides with another.
pub const SHARE: (f32, f32) = (0.1, 0.9);

/// `settings.json` in [`config_dir`].
pub fn settings_file() -> PathBuf {
    config_dir().join("settings.json")
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    #[serde(deserialize_with = "lenient")]
    pub appearance: Appearance,
    #[serde(deserialize_with = "lenient")]
    pub general: General,
    #[serde(deserialize_with = "lenient")]
    pub diff: DiffSettings,
    #[serde(deserialize_with = "lenient")]
    pub graph: GraphSettings,
    #[serde(deserialize_with = "lenient")]
    pub layout: LayoutSettings,
    #[serde(deserialize_with = "lenient")]
    pub tools: ToolSettings,
    /// Named identities for commits, applied to one repository at a time.
    #[serde(deserialize_with = "lenient", skip_serializing_if = "Vec::is_empty")]
    pub identity_profiles: Vec<IdentityProfile>,
}

/// A name, email and signing key kept under a label ("Work"), to set in a repository's
/// own config in one go. Nothing writes the global config.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IdentityProfile {
    #[serde(deserialize_with = "lenient")]
    pub label: String,
    #[serde(deserialize_with = "lenient")]
    pub name: String,
    #[serde(deserialize_with = "lenient")]
    pub email: String,
    /// `user.signingKey`, if the profile signs with a key of its own.
    #[serde(deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub signing_key: Option<String>,
}

/// The user's other programs (`external`); empty picks one. Commands may use `{path}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolSettings {
    #[serde(deserialize_with = "lenient")]
    pub terminal: String,
    #[serde(deserialize_with = "lenient")]
    pub file_manager: String,
    #[serde(deserialize_with = "lenient")]
    pub editor: String,
    /// A tool name for `git difftool --tool`; empty: git's `diff.guitool` / `diff.tool`.
    #[serde(deserialize_with = "lenient")]
    pub diff_tool: String,
    /// A tool name for `git mergetool --tool`; empty: git's `merge.guitool` / `merge.tool`.
    #[serde(deserialize_with = "lenient")]
    pub merge_tool: String,
}

/// The built-in colors the user's overrides go on top of.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorPreset {
    /// The redesign's colors.
    #[default]
    Graphite,
    /// Black on white (white on black), text and lines at a high contrast.
    HighContrast,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeChoice {
    /// Follow the desktop's light/dark setting.
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Density {
    /// 32 px rows, as in the Graphite design.
    #[default]
    Comfortable,
    /// 28 px rows.
    Compact,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    #[serde(deserialize_with = "lenient")]
    pub theme: ThemeChoice,
    #[serde(deserialize_with = "lenient")]
    pub ui_font: String,
    #[serde(deserialize_with = "lenient")]
    pub mono_font: String,
    #[serde(deserialize_with = "lenient_number")]
    pub ui_font_size: f32,
    #[serde(deserialize_with = "lenient_number")]
    pub mono_font_size: f32,
    /// Scales the whole UI (1.0 = 100%).
    #[serde(deserialize_with = "lenient_number")]
    pub zoom: f32,
    #[serde(deserialize_with = "lenient")]
    pub density: Density,
    #[serde(deserialize_with = "lenient")]
    pub preset: ColorPreset,
    #[serde(deserialize_with = "lenient")]
    pub colors: ColorOverrides,
    /// Colors the user saved under a name (Settings → Colors), to come back to them.
    #[serde(deserialize_with = "lenient_map")]
    pub saved_colors: BTreeMap<String, SavedColors>,
}

/// A preset and the user's colors on top of it, saved under a name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedColors {
    #[serde(deserialize_with = "lenient")]
    pub preset: ColorPreset,
    #[serde(deserialize_with = "lenient")]
    pub colors: ColorOverrides,
}

/// At most this many saved colors are kept.
pub const MAX_SAVED_COLORS: usize = 50;

impl Appearance {
    /// Saves the preset and colors in use under `name` (trimmed), replacing colors saved
    /// under that name before. Returns `false` (and saves nothing) for an empty name, or
    /// a new name when [`MAX_SAVED_COLORS`] are saved already.
    pub fn save_colors(&mut self, name: &str) -> bool {
        let name = name.trim();
        if name.is_empty()
            || (self.saved_colors.len() >= MAX_SAVED_COLORS
                && !self.saved_colors.contains_key(name))
        {
            return false;
        }
        let saved = SavedColors {
            preset: self.preset,
            colors: self.colors.clone(),
        };
        self.saved_colors.insert(name.to_owned(), saved);
        true
    }

    /// Puts the colors saved under `name` in use; `false` when there are none.
    pub fn use_saved_colors(&mut self, name: &str) -> bool {
        let Some(saved) = self.saved_colors.get(name) else {
            return false;
        };
        self.preset = saved.preset;
        self.colors = saved.colors.clone();
        true
    }

    /// The name the colors in use are saved under, if they are (the first by name).
    pub fn saved_colors_in_use(&self) -> Option<&str> {
        self.saved_colors
            .iter()
            .find(|(_, saved)| saved.preset == self.preset && saved.colors == self.colors)
            .map(|(name, _)| name.as_str())
    }

    /// Whether putting saved colors in use would lose colors the user changed: some are
    /// changed, and they aren't saved under any name.
    pub fn has_unsaved_colors(&self) -> bool {
        !(self.colors.dark.is_empty() && self.colors.light.is_empty())
            && self.saved_colors_in_use().is_none()
    }
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::System,
            preset: ColorPreset::Graphite,
            ui_font: DEFAULT_UI_FONT.into(),
            mono_font: DEFAULT_MONO_FONT.into(),
            ui_font_size: UI_FONT_SIZE.1,
            mono_font_size: MONO_FONT_SIZE.1,
            zoom: ZOOM.1,
            density: Density::Comfortable,
            colors: ColorOverrides::default(),
            saved_colors: BTreeMap::new(),
        }
    }
}

/// The user's colors, by palette token (`"bg"`, `"lane.3"`, `"diff.add_bg"`), as hex
/// (`#rgb`, `#rrggbb` or `#rrggbbaa`), separately for the dark and the light palette.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ColorOverrides {
    #[serde(deserialize_with = "lenient")]
    pub dark: BTreeMap<String, String>,
    #[serde(deserialize_with = "lenient")]
    pub light: BTreeMap<String, String>,
}

impl ColorOverrides {
    pub fn for_mode(&self, dark: bool) -> &BTreeMap<String, String> {
        if dark { &self.dark } else { &self.light }
    }

    pub fn for_mode_mut(&mut self, dark: bool) -> &mut BTreeMap<String, String> {
        if dark {
            &mut self.dark
        } else {
            &mut self.light
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DateFormat {
    /// `2026-08-29 12:27`
    #[default]
    Full,
    /// `08-29 12:27`
    Short,
    /// `2 hours ago`
    Relative,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    #[serde(deserialize_with = "lenient")]
    pub date_format: DateFormat,
    /// Reopen the last session's repositories on start (`session`).
    #[serde(deserialize_with = "lenient_true")]
    pub restore_session: bool,
    /// Trash entries older than this many days are deleted; 0, the default, keeps
    /// them until the user empties the trash.
    #[serde(deserialize_with = "lenient", skip_serializing_if = "is_zero")]
    pub trash_max_age_days: u32,
    /// The oldest trash entries are deleted while the trash is larger than this many MB;
    /// 0, the default, sets no limit.
    #[serde(deserialize_with = "lenient", skip_serializing_if = "is_zero")]
    pub trash_max_size_mb: u32,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

impl Default for General {
    fn default() -> Self {
        Self {
            date_format: DateFormat::Full,
            restore_session: true,
            trash_max_age_days: 0,
            trash_max_size_mb: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffLayout {
    #[default]
    Unified,
    Split,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiffSettings {
    #[serde(deserialize_with = "lenient")]
    pub layout: DiffLayout,
    #[serde(deserialize_with = "lenient")]
    pub ignore_whitespace: bool,
    /// Long lines wrap in the diff, file and blame views (else they scroll sideways).
    #[serde(deserialize_with = "lenient")]
    pub wrap_lines: bool,
    /// Moved lines look like any added and removed lines, without their own tint.
    /// Off by default: the tint is shown.
    #[serde(deserialize_with = "lenient")]
    pub plain_moved_lines: bool,
    /// Columns between tab stops in the code views; `None` is the default, 4. Read it
    /// with [`DiffSettings::tab_width`].
    #[serde(deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub tab_width: Option<u8>,
}

impl DiffSettings {
    /// The tab stops' distance, within [`TAB_WIDTH`].
    pub fn tab_width(&self) -> usize {
        let (min, default, max) = TAB_WIDTH;
        usize::from(self.tab_width.unwrap_or(default).clamp(min, max))
    }
}

/// The tab width's range and default: (min, default, max).
pub const TAB_WIDTH: (u8, u8, u8) = (1, 4, 16);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GraphSettings {
    #[serde(deserialize_with = "lenient_true")]
    pub show_author: bool,
    #[serde(deserialize_with = "lenient_true")]
    pub show_date: bool,
    #[serde(deserialize_with = "lenient_true")]
    pub show_sha: bool,
    /// Widths the user dragged the resizable columns to; `None` is the default width.
    #[serde(deserialize_with = "lenient")]
    pub refs_width: Option<f32>,
    #[serde(deserialize_with = "lenient")]
    pub author_width: Option<f32>,
    #[serde(deserialize_with = "lenient")]
    pub date_width: Option<f32>,
}

impl Default for GraphSettings {
    fn default() -> Self {
        Self {
            show_author: true,
            show_date: true,
            show_sha: true,
            refs_width: None,
            author_width: None,
            date_width: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LayoutSettings {
    /// Panel widths the user dragged to; `None` is the default width.
    #[serde(deserialize_with = "lenient")]
    pub sidebar_width: Option<f32>,
    #[serde(deserialize_with = "lenient")]
    pub right_panel_width: Option<f32>,
    /// The right panel's sections as the user dragged them; `None` is the default.
    /// Unstaged's share of the working changes' two file lists.
    #[serde(deserialize_with = "lenient")]
    pub staging_split: Option<f32>,
    /// The commit box's height, in design pixels.
    #[serde(deserialize_with = "lenient")]
    pub commit_box_height: Option<f32>,
    /// The height of a commit's message and facts above its files, in design pixels.
    #[serde(deserialize_with = "lenient")]
    pub details_top_height: Option<f32>,
    /// The file preview's share of the details panel.
    #[serde(deserialize_with = "lenient")]
    pub preview_share: Option<f32>,
    /// Sidebar sections opened in the bottom block (`"tags"`, `"stashes"`, …).
    #[serde(deserialize_with = "lenient")]
    pub sidebar_expanded: BTreeSet<String>,
}

impl AppSettings {
    /// These settings with every value in range: font sizes and zoom clamped, an empty
    /// font name back to the default, colors that aren't hex dropped, widths limited.
    pub fn clamped(mut self) -> Self {
        let appearance = &mut self.appearance;
        appearance.ui_font_size = clamp(appearance.ui_font_size, UI_FONT_SIZE);
        appearance.mono_font_size = clamp(appearance.mono_font_size, MONO_FONT_SIZE);
        appearance.zoom = clamp(appearance.zoom, ZOOM);
        if appearance.ui_font.trim().is_empty() {
            appearance.ui_font = DEFAULT_UI_FONT.into();
        }
        if appearance.mono_font.trim().is_empty() {
            appearance.mono_font = DEFAULT_MONO_FONT.into();
        }
        let saved = std::mem::take(&mut appearance.saved_colors);
        appearance.saved_colors = saved
            .into_iter()
            .filter_map(|(name, saved)| {
                let name = name.trim().to_owned();
                (!name.is_empty()).then_some((name, saved))
            })
            .take(MAX_SAVED_COLORS)
            .collect();
        let overrides = std::iter::once(&mut appearance.colors).chain(
            appearance
                .saved_colors
                .values_mut()
                .map(|saved| &mut saved.colors),
        );
        for colors in overrides {
            colors.dark.retain(|_, value| is_hex_color(value));
            colors.light.retain(|_, value| is_hex_color(value));
        }
        let graph = &mut self.graph;
        let columns = [
            &mut graph.refs_width,
            &mut graph.author_width,
            &mut graph.date_width,
        ];
        let layout = &mut self.layout;
        for share in [&mut layout.staging_split, &mut layout.preview_share] {
            *share = share
                .filter(|value| value.is_finite())
                .map(|value| value.clamp(SHARE.0, SHARE.1));
        }
        let panels = [
            &mut layout.sidebar_width,
            &mut layout.right_panel_width,
            &mut layout.commit_box_height,
            &mut layout.details_top_height,
        ];
        let widths = columns.into_iter().map(|width| (width, WIDTH));
        for (width, (min, max)) in
            widths.chain(panels.into_iter().map(|width| (width, PANEL_WIDTH)))
        {
            *width = width
                .filter(|value| value.is_finite())
                .map(|value| value.clamp(min, max));
        }
        self
    }
}

fn clamp(value: f32, (min, default, max): (f32, f32, f32)) -> f32 {
    if value.is_finite() {
        value.clamp(min, max)
    } else {
        default
    }
}

/// Whether `text` is `#rgb`, `#rrggbb` or `#rrggbbaa`.
pub fn is_hex_color(text: &str) -> bool {
    text.strip_prefix('#').is_some_and(|digits| {
        matches!(digits.len(), 3 | 6 | 8) && digits.chars().all(|c| c.is_ascii_hexdigit())
    })
}

/// Deserializes a field, falling back to its default when the value doesn't fit (an
/// unknown theme name, a string where a number belongs), so one bad value doesn't cost
/// the whole file.
fn lenient<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned + Default,
{
    let value = Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

/// A map whose entries are read one by one: an entry that doesn't fit is left out, the
/// others stay (a plain `lenient` would drop them all).
fn lenient_map<'de, D, T>(deserializer: D) -> Result<BTreeMap<String, T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let Value::Object(entries) = Value::deserialize(deserializer)? else {
        return Ok(BTreeMap::new());
    };
    Ok(entries
        .into_iter()
        .filter_map(|(key, value)| Some((key, serde_json::from_value(value).ok()?)))
        .collect())
}

/// A number that doesn't fit becomes NaN, which [`AppSettings::clamped`] turns into the
/// field's default (a plain `lenient` would give 0, which clamps to the minimum).
fn lenient_number<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f32, D::Error> {
    let value = Value::deserialize(deserializer)?;
    Ok(value.as_f64().map_or(f32::NAN, |number| number as f32))
}

/// A switch that is on by default (a column shown) stays on when its value doesn't fit.
fn lenient_true<'de, D: Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    let value = Value::deserialize(deserializer)?;
    Ok(value.as_bool().unwrap_or(true))
}

/// The settings file, with what's needed to save it back faithfully: the JSON as read
/// (for keys this build doesn't know), whether it may be written, and whether a corrupt
/// file has to be moved aside first.
#[derive(Debug)]
pub struct SettingsFile {
    path: PathBuf,
    raw: Map<String, Value>,
    writable: bool,
    corrupt: bool,
}

impl SettingsFile {
    /// Reads `path`. Never fails: see the module docs for what each problem gives.
    pub fn open(path: &Path) -> (Self, AppSettings) {
        let mut file = SettingsFile {
            path: path.to_path_buf(),
            raw: Map::new(),
            writable: true,
            corrupt: false,
        };
        let text = match std::fs::read(path) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => text,
                // Not UTF-8 is as corrupt as bad JSON: kept as the backup on the next save.
                Err(_) => {
                    file.corrupt = true;
                    return (file, AppSettings::default());
                }
            },
            // Missing is the normal first run; unreadable (permissions, a folder) is
            // treated the same, and saving will report the real problem.
            Err(_) => return (file, AppSettings::default()),
        };
        let raw = match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(raw)) => raw,
            _ => {
                file.corrupt = true;
                return (file, AppSettings::default());
            }
        };
        let version = raw.get("version").and_then(Value::as_u64).unwrap_or(1);
        file.writable = version <= u64::from(VERSION);
        let settings = serde_json::from_value::<AppSettings>(Value::Object(raw.clone()))
            .unwrap_or_default()
            .clamped();
        file.raw = raw;
        (file, settings)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `false` when the file was written by a newer version of Gitweft: it's read, but
    /// saving leaves it alone.
    pub fn writable(&self) -> bool {
        self.writable
    }

    /// Writes `settings`, keeping keys this build doesn't know. Returns `Ok(false)`
    /// without writing when the file isn't [`writable`](Self::writable). Writes a
    /// temporary file and renames it, so a crash never leaves half a file.
    pub fn save(&mut self, settings: &AppSettings) -> io::Result<bool> {
        if !self.writable {
            return Ok(false);
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if self.corrupt {
            // Keep what the user had, in case it was hand-edited and mostly fine (never
            // over an earlier backup: `.bak.2`, `.bak.3`…).
            // Where the file really is: a linked one stays linked.
            let target = crate::datafile::save_target(&self.path)?;
            std::fs::rename(&target, crate::datafile::backup_path(&target))?;
            self.corrupt = false;
        }
        let Value::Object(fresh) = serde_json::to_value(settings).map_err(io::Error::other)? else {
            unreachable!("settings serialize to an object");
        };
        merge_sections(&mut self.raw, fresh);
        self.raw.insert("version".into(), Value::from(VERSION));
        let text = serde_json::to_string_pretty(&self.raw).map_err(io::Error::other)?;
        // Two instances saving at once each write their own temporary file.
        crate::datafile::atomic_write(&self.path, (text + "\n").as_bytes())?;
        Ok(true)
    }
}

/// Puts `fresh` into `raw` two levels deep: sections keep keys this build doesn't know,
/// while every value this build writes (including whole maps such as the color
/// overrides, so a removed override is really gone) replaces the old one.
fn merge_sections(raw: &mut Map<String, Value>, fresh: Map<String, Value>) {
    for (key, value) in fresh {
        match (raw.get_mut(&key), value) {
            (Some(Value::Object(old)), Value::Object(section)) => {
                for (field, field_value) in section {
                    old.insert(field, field_value);
                }
            }
            (_, value) => {
                raw.insert(key, value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn a_missing_file_gives_defaults_and_saving_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gitweft/settings.json");
        let (mut file, settings) = SettingsFile::open(&path);
        assert_eq!(settings, AppSettings::default());
        assert!(file.writable());

        let mut changed = settings.clone();
        changed.appearance.theme = ThemeChoice::Dark;
        changed.appearance.density = Density::Compact;
        changed.appearance.zoom = 1.25;
        changed
            .appearance
            .colors
            .dark
            .insert("acc".into(), "#ff8800".into());
        changed.general.date_format = DateFormat::Relative;
        changed.general.restore_session = false;
        changed.diff.layout = DiffLayout::Split;
        changed.graph.show_sha = false;
        changed.graph.author_width = Some(180.0);
        changed.layout.sidebar_expanded.insert("tags".into());
        changed.tools.terminal = "kitty".into();
        changed.tools.editor = "code -g {path}".into();
        changed.tools.diff_tool = "meld".into();
        assert!(file.save(&changed).unwrap());
        assert!(!path.with_extension("json.tmp").exists());

        let (_, reread) = SettingsFile::open(&path);
        assert_eq!(reread, changed);
        assert_eq!(read(&path)["version"], VERSION);
    }

    /// A settings file a dotfile manager links in stays a link after a save.
    #[test]
    fn a_linked_settings_file_stays_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles/gitweft/settings.json");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "{\"version\": 1}").unwrap();
        let path = dir.path().join("config/gitweft/settings.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, &path).unwrap();

        let (mut file, mut settings) = SettingsFile::open(&path);
        settings.appearance.theme = ThemeChoice::Dark;
        assert!(file.save(&settings).unwrap());

        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let (_, reread) = SettingsFile::open(&real);
        assert_eq!(reread.appearance.theme, ThemeChoice::Dark);
    }

    #[test]
    fn missing_and_unfit_fields_default_one_by_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"version": 1, "appearance": {"theme": "sepia", "zoom": "big", "density": "compact"},
                "diff": {"ignore_whitespace": true}, "graph": 7, "general": {"restore_session": "yes"}}"#,
        )
        .unwrap();
        let (_, settings) = SettingsFile::open(&path);
        assert!(settings.general.restore_session);
        assert_eq!(settings.appearance.theme, ThemeChoice::System);
        assert_eq!(settings.appearance.zoom, 1.0);
        assert_eq!(settings.appearance.density, Density::Compact);
        assert_eq!(settings.appearance.ui_font, DEFAULT_UI_FONT);
        assert!(settings.diff.ignore_whitespace);
        assert_eq!(settings.diff.layout, DiffLayout::Unified);
        // Settings older than the plain moved-lines option keep the tint.
        assert!(!settings.diff.plain_moved_lines);
        assert_eq!(settings.graph, GraphSettings::default());
    }

    /// The Tab width: 4 when unset or unreadable, kept within 1–16.
    #[test]
    fn identity_profiles_round_trip_and_default_to_none() {
        let settings: AppSettings = serde_json::from_str("{}").unwrap();
        assert!(settings.identity_profiles.is_empty());
        assert!(
            !serde_json::to_string(&settings)
                .unwrap()
                .contains("identity_profiles")
        );
        let text = r#"{"identity_profiles": [
            {"label": "Work", "name": "Ada", "email": "ada@work.example", "signing_key": "ABC"},
            {"label": "Home", "name": "Ada", "email": "ada@home.example"}
        ]}"#;
        let settings: AppSettings = serde_json::from_str(text).unwrap();
        assert_eq!(settings.identity_profiles.len(), 2);
        assert_eq!(
            settings.identity_profiles[0].signing_key.as_deref(),
            Some("ABC")
        );
        assert_eq!(settings.identity_profiles[1].signing_key, None);
        let again: AppSettings =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(again, settings);
    }

    #[test]
    fn the_tab_width_defaults_and_stays_in_range() {
        let with = |json: &str| -> usize {
            let diff: DiffSettings = serde_json::from_str(json).unwrap();
            diff.tab_width()
        };
        assert_eq!(with("{}"), 4);
        assert_eq!(with(r#"{"tab_width": 8}"#), 8);
        assert_eq!(with(r#"{"tab_width": 0}"#), 1);
        assert_eq!(with(r#"{"tab_width": 200}"#), 16);
        assert_eq!(with(r#"{"tab_width": "wide"}"#), 4);
        // Unset isn't written, so older versions read the file as before.
        let json = serde_json::to_string(&DiffSettings::default()).unwrap();
        assert!(!json.contains("tab_width"), "{json}");
    }

    #[test]
    fn unknown_keys_survive_a_save_and_removed_colors_are_gone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r##"{"version": 1, "future": {"x": 1},
                "appearance": {"sparkles": true, "colors": {"dark": {"acc": "#112233", "bg": "#000"}}}}"##,
        )
        .unwrap();
        let (mut file, mut settings) = SettingsFile::open(&path);
        assert_eq!(settings.appearance.colors.dark.len(), 2);
        settings.appearance.colors.dark.remove("bg");
        file.save(&settings).unwrap();

        let saved = read(&path);
        assert_eq!(saved["future"]["x"], 1);
        assert_eq!(saved["appearance"]["sparkles"], true);
        assert_eq!(saved["appearance"]["colors"]["dark"]["acc"], "#112233");
        assert!(saved["appearance"]["colors"]["dark"].get("bg").is_none());
    }

    #[test]
    fn a_newer_version_is_read_but_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let text = r#"{"version": 99, "appearance": {"theme": "light"}}"#;
        std::fs::write(&path, text).unwrap();
        let (mut file, settings) = SettingsFile::open(&path);
        assert_eq!(settings.appearance.theme, ThemeChoice::Light);
        assert!(!file.writable());
        assert!(!file.save(&AppSettings::default()).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn a_corrupt_file_gives_defaults_and_is_kept_as_a_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{ \"appearance\": ").unwrap();
        let (mut file, settings) = SettingsFile::open(&path);
        assert_eq!(settings, AppSettings::default());
        assert!(file.save(&settings).unwrap());
        assert_eq!(
            std::fs::read_to_string(path.with_extension("json.bak")).unwrap(),
            "{ \"appearance\": "
        );
        assert_eq!(read(&path)["version"], VERSION);
        // Only the first save moves it aside.
        assert!(file.save(&settings).unwrap());
        assert!(path.with_extension("json.bak").exists());
    }

    /// Not UTF-8 is as corrupt as bad JSON (it used to count as missing and was replaced
    /// without a backup), and a later backup never replaces an earlier one.
    #[test]
    fn a_file_that_isnt_utf8_is_kept_as_a_backup_and_backups_pile_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let bytes: &[u8] = b"{\"appearance\": {\"ui_font\": \"Caf\xe9 Sans\"}}";
        std::fs::write(&path, bytes).unwrap();
        let (mut file, settings) = SettingsFile::open(&path);
        assert_eq!(settings, AppSettings::default());
        assert!(file.save(&settings).unwrap());
        assert_eq!(
            std::fs::read(path.with_extension("json.bak")).unwrap(),
            bytes
        );

        std::fs::write(&path, "{ broken").unwrap();
        let (mut file, settings) = SettingsFile::open(&path);
        assert!(file.save(&settings).unwrap());
        assert_eq!(
            std::fs::read(path.with_extension("json.bak")).unwrap(),
            bytes
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("settings.json.bak.2")).unwrap(),
            "{ broken"
        );
    }

    #[test]
    fn values_are_clamped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r##"{"appearance": {"ui_font_size": 3, "mono_font_size": 40, "zoom": 9,
                 "ui_font": "  ", "colors": {"light": {"acc": "#12345", "bg": "#abcdef80", "fg": "red"}}},
                "layout": {"sidebar_width": 5, "right_panel_width": 99999,
                           "staging_split": 2, "preview_share": -1, "commit_box_height": 300}}"##,
        )
        .unwrap();
        let (_, settings) = SettingsFile::open(&path);
        let appearance = &settings.appearance;
        assert_eq!(appearance.ui_font_size, UI_FONT_SIZE.0);
        assert_eq!(appearance.mono_font_size, MONO_FONT_SIZE.2);
        assert_eq!(appearance.zoom, ZOOM.2);
        assert_eq!(appearance.ui_font, DEFAULT_UI_FONT);
        assert_eq!(
            appearance.colors.light.keys().collect::<Vec<_>>(),
            vec!["bg"]
        );
        assert_eq!(settings.layout.sidebar_width, Some(WIDTH.0));
        assert_eq!(
            settings.layout.right_panel_width,
            Some(99999.0f32.min(PANEL_WIDTH.1))
        );
        assert_eq!(settings.layout.staging_split, Some(SHARE.1));
        assert_eq!(settings.layout.preview_share, Some(SHARE.0));
        assert_eq!(settings.layout.commit_box_height, Some(300.));
        assert_eq!(settings.layout.details_top_height, None);
    }

    #[test]
    fn colors_are_saved_used_and_recognised() {
        let mut appearance = Appearance::default();
        assert!(!appearance.has_unsaved_colors());
        appearance
            .colors
            .dark
            .insert("acc".into(), "#ff8800".into());
        assert!(appearance.has_unsaved_colors());
        assert!(!appearance.save_colors("   "));
        assert!(appearance.save_colors("  Warm  "));
        assert_eq!(appearance.saved_colors_in_use(), Some("Warm"));
        assert!(!appearance.has_unsaved_colors());

        appearance.preset = ColorPreset::HighContrast;
        appearance.colors.light.insert("bg".into(), "#fff".into());
        assert_eq!(appearance.saved_colors_in_use(), None);
        assert!(appearance.save_colors("Bright"));

        assert!(appearance.use_saved_colors("Warm"));
        assert_eq!(appearance.preset, ColorPreset::Graphite);
        assert!(appearance.colors.light.is_empty());
        assert_eq!(appearance.saved_colors_in_use(), Some("Warm"));
        assert!(!appearance.use_saved_colors("Missing"));
        assert_eq!(appearance.saved_colors_in_use(), Some("Warm"));

        // The same name replaces what it held.
        appearance.colors.dark.insert("bg".into(), "#000".into());
        assert!(appearance.save_colors("Warm"));
        assert_eq!(appearance.saved_colors.len(), 2);
        assert_eq!(appearance.saved_colors["Warm"].colors.dark.len(), 2);
    }

    #[test]
    fn saved_colors_are_capped_but_can_still_be_replaced() {
        let mut appearance = Appearance::default();
        for n in 0..MAX_SAVED_COLORS {
            assert!(appearance.save_colors(&format!("c{n:02}")));
        }
        assert!(!appearance.save_colors("one more"));
        assert!(appearance.save_colors("c07"));
        assert_eq!(appearance.saved_colors.len(), MAX_SAVED_COLORS);
    }

    #[test]
    fn saved_colors_round_trip_and_bad_entries_drop_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r##"{"appearance": {"saved_colors": {
                "Warm": {"preset": "high_contrast", "colors": {"dark": {"acc": "#f80", "bg": "red"}}},
                "Odd": 7,
                "  ": {"preset": "graphite"},
                "Unknown preset": {"preset": "sepia", "colors": {"light": {"fg": "#123456"}}}}}}"##,
        )
        .unwrap();
        let (mut file, settings) = SettingsFile::open(&path);
        let saved = &settings.appearance.saved_colors;
        assert_eq!(
            saved.keys().collect::<Vec<_>>(),
            vec!["Unknown preset", "Warm"]
        );
        assert_eq!(saved["Warm"].preset, ColorPreset::HighContrast);
        assert_eq!(
            saved["Warm"].colors.dark.keys().collect::<Vec<_>>(),
            vec!["acc"]
        );
        assert_eq!(saved["Unknown preset"].preset, ColorPreset::Graphite);

        let mut changed = settings.clone();
        changed.appearance.saved_colors.remove("Warm");
        file.save(&changed).unwrap();
        let (_, reread) = SettingsFile::open(&path);
        assert_eq!(reread, changed);
        assert!(
            read(&path)["appearance"]["saved_colors"]
                .get("Warm")
                .is_none()
        );
    }

    #[test]
    fn hex_colors() {
        for good in ["#abc", "#A1B2C3", "#a1b2c3d4"] {
            assert!(is_hex_color(good), "{good}");
        }
        for bad in ["abc", "#ab", "#abcd", "#abcdefg", "#ggg", "", "#"] {
            assert!(!is_hex_color(bad), "{bad}");
        }
    }

    #[test]
    fn the_file_is_in_the_config_dir() {
        assert_eq!(settings_file().parent().unwrap(), config_dir());
        assert!(config_dir().ends_with("gitweft"));
    }
}
