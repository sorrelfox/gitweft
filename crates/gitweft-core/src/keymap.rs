//! The user's key bindings, kept in `$XDG_CONFIG_HOME/gitweft/keymap.json` next to
//! `settings.json`. Only overrides are stored: an action that isn't listed keeps the
//! bindings the app gives it.
//!
//! ```json
//! {"version": 1, "bindings": {"workspace::Fetch": ["ctrl-shift-f"], "shell::NewTab": []}}
//! ```
//!
//! The file is forgiving in the same ways as the settings file:
//! - a missing or unreadable file gives no overrides; a corrupt one (not JSON, or not an
//!   object) gives none either, and is kept as `keymap.json.bak` when the bindings are
//!   first saved again;
//! - one entry that doesn't fit is skipped without losing the rest (see
//!   [`KeymapFile::open`]);
//! - top-level keys this build doesn't know (written by a newer build) are kept when saving;
//! - a file written by a newer format version is read, but never overwritten.
//!
//! Keystrokes aren't checked here: the GUI parses them and ignores the ones it can't.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::paths::config_dir;

/// The format version this build writes.
pub const KEYMAP_VERSION: u32 = 1;

/// The user's key bindings, per action (its GPUI name, e.g. "workspace::Fetch"): the
/// keystroke sequences ("ctrl-shift-f", "g g") that replace the action's default bindings.
/// An empty list unbinds it. Actions not listed keep their defaults.
pub type Overrides = BTreeMap<String, Vec<String>>;

/// `$XDG_CONFIG_HOME/gitweft/keymap.json` (next to settings.json).
pub fn keymap_path() -> PathBuf {
    config_dir().join("keymap.json")
}

/// The keymap file, with what's needed to save it back faithfully: the JSON as read (for
/// keys this build doesn't know), whether it may be written, and whether a corrupt file
/// has to be moved aside first.
#[derive(Debug)]
pub struct KeymapFile {
    path: PathBuf,
    raw: Map<String, Value>,
    writable: bool,
    corrupt: bool,
}

impl KeymapFile {
    /// Never fails: missing or unreadable → no overrides; corrupt JSON → none, and the
    /// first save renames it to `keymap.json.bak`; a newer version is read (lenient) but
    /// save returns Ok(false) without writing.
    ///
    /// Each entry of `bindings` is read on its own: a list keeps its strings (trimmed,
    /// empty ones and non-strings dropped), a single string is a one-item list, `null`
    /// unbinds the action, and anything else skips the entry. So does an action name
    /// that's empty or has no `::`. A `bindings` that isn't an object gives no overrides.
    pub fn open(path: PathBuf) -> (Self, Overrides) {
        let mut file = KeymapFile {
            path,
            raw: Map::new(),
            writable: true,
            corrupt: false,
        };
        let text = match std::fs::read(&file.path) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => text,
                // Not UTF-8 is as corrupt as bad JSON: kept as the backup on the next save.
                Err(_) => {
                    file.corrupt = true;
                    return (file, Overrides::new());
                }
            },
            // Missing is the normal first run; unreadable (permissions, a folder) is
            // treated the same, and saving will report the real problem.
            Err(_) => return (file, Overrides::new()),
        };
        let raw = match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(raw)) => raw,
            _ => {
                file.corrupt = true;
                return (file, Overrides::new());
            }
        };
        let version = raw.get("version").and_then(Value::as_u64).unwrap_or(1);
        file.writable = version <= u64::from(KEYMAP_VERSION);
        let overrides = raw.get("bindings").map(parse_bindings).unwrap_or_default();
        file.raw = raw;
        (file, overrides)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `false` when the file was written by a newer version of Gitweft: it's read, but
    /// saving leaves it alone.
    pub fn writable(&self) -> bool {
        self.writable
    }

    /// Writes the overrides (temp file + rename, creating the folder), keeping unknown
    /// top-level keys; Ok(false) when the file is from a newer version (left untouched).
    pub fn save(&mut self, overrides: &Overrides) -> io::Result<bool> {
        if !self.writable {
            return Ok(false);
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if self.corrupt {
            // Keep what the user had, in case it was hand-edited and mostly fine (never
            // over an earlier backup: `.bak.2`, `.bak.3`…).
            // Where the file really is: a linked one stays linked (#8).
            let target = crate::datafile::save_target(&self.path)?;
            std::fs::rename(&target, crate::datafile::backup_path(&target))?;
            self.corrupt = false;
        }
        // The whole map is replaced, so a removed override is really gone.
        let bindings = serde_json::to_value(overrides).map_err(io::Error::other)?;
        self.raw
            .insert("version".into(), Value::from(KEYMAP_VERSION));
        self.raw.insert("bindings".into(), bindings);
        let text = serde_json::to_string_pretty(&self.raw).map_err(io::Error::other)?;
        // Two instances saving at once each write their own temporary file (#7).
        crate::datafile::atomic_write(&self.path, (text + "\n").as_bytes())?;
        Ok(true)
    }
}

/// The overrides in a `bindings` value, by the rules in [`KeymapFile::open`].
fn parse_bindings(bindings: &Value) -> Overrides {
    let Value::Object(entries) = bindings else {
        return Overrides::new();
    };
    entries
        .iter()
        .filter(|(action, _)| !action.is_empty() && action.contains("::"))
        .filter_map(|(action, value)| {
            let keystrokes = match value {
                Value::Null => Vec::new(),
                Value::String(keystroke) => keystrokes([keystroke.as_str()]),
                Value::Array(items) => keystrokes(items.iter().filter_map(Value::as_str)),
                _ => return None,
            };
            Some((action.clone(), keystrokes))
        })
        .collect()
}

/// `items` trimmed, without the empty ones.
fn keystrokes<'a>(items: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    items
        .into_iter()
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn overrides(entries: &[(&str, &[&str])]) -> Overrides {
        entries
            .iter()
            .map(|(action, keys)| {
                let keys = keys.iter().map(|key| key.to_string()).collect();
                (action.to_string(), keys)
            })
            .collect()
    }

    #[test]
    fn a_missing_file_gives_no_overrides_and_saving_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gitweft/keymap.json");
        let (mut file, read_back) = KeymapFile::open(path.clone());
        assert!(read_back.is_empty());
        assert!(file.writable());
        assert_eq!(file.path(), path);

        let changed = overrides(&[
            ("workspace::Fetch", &["ctrl-shift-f", "g f"]),
            ("shell::NewTab", &[]),
        ]);
        assert!(file.save(&changed).unwrap());
        assert!(!path.with_extension("json.tmp").exists());

        let (_, reread) = KeymapFile::open(path.clone());
        assert_eq!(reread, changed);
        let saved = read(&path);
        assert_eq!(saved["version"], KEYMAP_VERSION);
        // An unbound action is written as an empty list, not dropped.
        assert_eq!(saved["bindings"]["shell::NewTab"], serde_json::json!([]));
    }

    #[test]
    fn a_removed_override_is_gone_after_a_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keymap.json");
        std::fs::write(
            &path,
            r#"{"version": 1, "bindings": {"a::B": ["ctrl-b"], "a::C": ["ctrl-c"]}}"#,
        )
        .unwrap();
        let (mut file, mut read_back) = KeymapFile::open(path.clone());
        read_back.remove("a::C");
        file.save(&read_back).unwrap();
        let saved = read(&path);
        assert_eq!(saved["bindings"]["a::B"], serde_json::json!(["ctrl-b"]));
        assert!(saved["bindings"].get("a::C").is_none());
    }

    #[test]
    fn entries_are_read_leniently_one_by_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keymap.json");
        std::fs::write(
            &path,
            r#"{"version": 1, "bindings": {
                "a::List": [" ctrl-a ", 3, "", "  ", null, "g g"],
                "a::Single": " alt-s ",
                "a::Null": null,
                "a::Empty": [],
                "a::Number": 7,
                "a::Object": {"key": "ctrl-o"},
                "NoSeparator": ["ctrl-n"],
                "": ["ctrl-e"]
            }}"#,
        )
        .unwrap();
        let (_, read_back) = KeymapFile::open(path);
        assert_eq!(
            read_back,
            overrides(&[
                ("a::List", &["ctrl-a", "g g"]),
                ("a::Single", &["alt-s"]),
                ("a::Null", &[]),
                ("a::Empty", &[]),
            ])
        );
    }

    #[test]
    fn bindings_that_arent_an_object_give_no_overrides_but_arent_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keymap.json");
        std::fs::write(&path, r#"{"version": 1, "bindings": ["ctrl-a"]}"#).unwrap();
        let (mut file, read_back) = KeymapFile::open(path.clone());
        assert!(read_back.is_empty());
        assert!(file.save(&read_back).unwrap());
        assert!(!path.with_extension("json.bak").exists());
    }

    #[test]
    fn unknown_top_level_keys_survive_a_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keymap.json");
        std::fs::write(
            &path,
            r#"{"version": 1, "future": {"x": 1}, "bindings": {"a::B": "ctrl-b"}}"#,
        )
        .unwrap();
        let (mut file, read_back) = KeymapFile::open(path.clone());
        file.save(&read_back).unwrap();
        let saved = read(&path);
        assert_eq!(saved["future"]["x"], 1);
        assert_eq!(saved["bindings"]["a::B"], serde_json::json!(["ctrl-b"]));
    }

    #[test]
    fn a_newer_version_is_read_but_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keymap.json");
        let text = r#"{"version": 99, "bindings": {"a::B": ["ctrl-b"], "a::C": {"new": 1}}}"#;
        std::fs::write(&path, text).unwrap();
        let (mut file, read_back) = KeymapFile::open(path.clone());
        assert_eq!(read_back, overrides(&[("a::B", &["ctrl-b"])]));
        assert!(!file.writable());
        assert!(!file.save(&Overrides::new()).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn a_corrupt_file_gives_no_overrides_and_is_kept_as_a_backup() {
        for text in ["{ \"bindings\": ", "[1, 2]"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("keymap.json");
            std::fs::write(&path, text).unwrap();
            let (mut file, read_back) = KeymapFile::open(path.clone());
            assert!(read_back.is_empty());
            let changed = overrides(&[("a::B", &["ctrl-b"])]);
            assert!(file.save(&changed).unwrap());
            assert_eq!(
                std::fs::read_to_string(path.with_extension("json.bak")).unwrap(),
                text
            );
            assert_eq!(read(&path)["version"], KEYMAP_VERSION);
            // Only the first save moves it aside.
            assert!(file.save(&changed).unwrap());
            assert_eq!(
                std::fs::read_to_string(path.with_extension("json.bak")).unwrap(),
                text
            );
        }
    }

    /// Not UTF-8 is as corrupt as bad JSON (it used to count as missing and was replaced
    /// without a backup), and a later backup never replaces an earlier one (#4).
    #[test]
    fn a_file_that_isnt_utf8_is_kept_as_a_backup_and_backups_pile_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keymap.json");
        let bytes: &[u8] = b"{\"version\": 1, \"bindings\": {\"a::B\": [\"caf\xe9\"]}}";
        std::fs::write(&path, bytes).unwrap();
        let (mut file, read_back) = KeymapFile::open(path.clone());
        assert!(read_back.is_empty());
        assert!(file.save(&overrides(&[("a::B", &["ctrl-b"])])).unwrap());
        assert_eq!(
            std::fs::read(path.with_extension("json.bak")).unwrap(),
            bytes
        );

        std::fs::write(&path, "{ broken").unwrap();
        let (mut file, _) = KeymapFile::open(path.clone());
        assert!(file.save(&Overrides::new()).unwrap());
        assert_eq!(
            std::fs::read(path.with_extension("json.bak")).unwrap(),
            bytes
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("keymap.json.bak.2")).unwrap(),
            "{ broken"
        );
    }

    #[test]
    fn the_file_is_next_to_the_settings() {
        assert_eq!(keymap_path().parent().unwrap(), config_dir());
        assert_eq!(
            keymap_path().parent(),
            crate::settings::settings_file().parent()
        );
        assert!(keymap_path().ends_with("gitweft/keymap.json"));
    }
}
