//! `locales/app.yml` against the code: every key the code reads exists, every key is read,
//! and counted strings have their plural forms (`tn!`, `src/plural.rs`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Plural form suffixes, as `plural::category` names them.
const FORMS: [&str; 4] = ["_one", "_few", "_many", "_other"];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Key → its English text.
fn locale() -> BTreeMap<String, String> {
    let text = std::fs::read_to_string(root().join("locales/app.yml")).unwrap();
    let mut keys = BTreeMap::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        if line.is_empty() || line.starts_with([' ', '#', '_']) {
            continue;
        }
        let key = line
            .strip_suffix(':')
            .unwrap_or_else(|| panic!("not a key line: {line}"));
        let value = lines
            .next()
            .and_then(|l| l.strip_prefix("  en: "))
            .unwrap_or_else(|| panic!("{key} has no `en:` line"));
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(value);
        assert!(
            keys.insert(key.to_owned(), value.to_owned()).is_none(),
            "{key} twice"
        );
    }
    keys
}

fn sources(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push((path.clone(), std::fs::read_to_string(&path).unwrap()));
        }
    }
}

/// The source files of every crate (tests included: a key only a test reads still counts).
fn all_sources() -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    for krate in std::fs::read_dir(root().join("crates")).unwrap() {
        let krate = krate.unwrap().path();
        for dir in ["src", "tests"] {
            if krate.join(dir).is_dir() {
                sources(&krate.join(dir), &mut out);
            }
        }
    }
    out
}

fn is_key(text: &str) -> bool {
    text.contains('.')
        && text.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
}

/// The string literals that open a `name!(` call, e.g. every `t!("…"` key.
fn macro_keys<'a>(text: &'a str, name: &str) -> Vec<&'a str> {
    let mut keys = Vec::new();
    let open = format!("{name}!(");
    let mut rest = text;
    while let Some(at) = rest.find(&open) {
        // Not the tail of a longer macro name (`format!(` ends in `t!(`).
        let before = rest[..at].chars().next_back();
        rest = &rest[at + open.len()..];
        if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
            continue;
        }
        let trimmed = rest.trim_start();
        if let Some(literal) = trimmed.strip_prefix('"')
            && let Some(end) = literal.find('"')
        {
            keys.push(&literal[..end]);
        }
    }
    keys
}

/// Every string literal that looks like a key (keys handed to helpers such as `heading`).
fn quoted_keys(text: &str) -> BTreeSet<&str> {
    text.split('"')
        .skip(1)
        .step_by(2)
        .filter(|s| is_key(s))
        .collect()
}

fn placeholders(text: &str) -> BTreeSet<&str> {
    text.split("%{")
        .skip(1)
        .filter_map(|s| s.split_once('}').map(|(name, _)| name))
        .collect()
}

fn plural_base(key: &str) -> Option<&str> {
    FORMS.iter().find_map(|form| key.strip_suffix(form))
}

#[test]
fn every_key_read_exists_and_every_key_is_read() {
    let keys = locale();
    let sources = all_sources();
    let mut problems = Vec::new();
    let mut read = BTreeSet::new();
    let mut counted = BTreeSet::new();
    for (path, text) in &sources {
        let file = path
            .strip_prefix(root())
            .unwrap_or(path)
            .display()
            .to_string();
        for key in macro_keys(text, "t").into_iter().filter(|k| is_key(k)) {
            if !keys.contains_key(key) {
                problems.push(format!("{file}: t!(\"{key}\") isn't in app.yml"));
            }
            if plural_base(key).is_some() {
                problems.push(format!("{file}: t!(\"{key}\") is a plural form: use tn!"));
            }
        }
        for base in macro_keys(text, "tn") {
            for form in ["_one", "_other"] {
                if !keys.contains_key(&format!("{base}{form}")) {
                    problems.push(format!("{file}: tn!(\"{base}\") has no {base}{form}"));
                }
            }
            if keys.contains_key(base) {
                problems.push(format!(
                    "{file}: tn!(\"{base}\"), but {base} is a key of its own"
                ));
            }
            counted.insert(base.to_owned());
        }
        read.extend(quoted_keys(text).into_iter().map(str::to_owned));
    }
    for key in keys.keys() {
        let used =
            read.contains(key) || plural_base(key).is_some_and(|base| counted.contains(base));
        if !used {
            problems.push(format!("app.yml: {key} isn't read anywhere"));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn plural_forms_come_in_sets_with_the_same_placeholders() {
    let keys = locale();
    let mut problems = Vec::new();
    for (key, value) in &keys {
        let Some(base) = plural_base(key) else {
            continue;
        };
        let other = format!("{base}_other");
        let Some(other_value) = keys.get(&other) else {
            problems.push(format!("{key} has no {other}"));
            continue;
        };
        // "one" may leave the number out in English ("1 file" reads "%{count} file" or "a file").
        let mut mine = placeholders(value);
        let mut theirs = placeholders(other_value);
        mine.remove("count");
        theirs.remove("count");
        if mine != theirs {
            problems.push(format!("{key} and {other} take different placeholders"));
        }
        if !placeholders(other_value).contains("count") {
            problems.push(format!("{other} doesn't show %{{count}}"));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// `format!` with a `t!` or `tn!` among its arguments glues a translated text to other text in
/// code, which a translation can't reorder or reword: a message is one key with placeholders.
/// Where the key only names something that isn't text for people (never so far), the
/// call gets `// i18n: not text` on the line above.
///
/// A translation first put in a variable and then glued (`format!("{by} {text}")`) isn't
/// seen: that needs reading, not a scan.
#[test]
fn no_translated_text_is_glued_in_format() {
    let mut glued = Vec::new();
    for (path, text) in all_sources() {
        if path.components().any(|part| part.as_os_str() == "tests") {
            continue;
        }
        for (start, _) in text.match_indices("format!(") {
            let args = macro_args(&text[start + "format!(".len()..]);
            let translated = calls(args, "t") || calls(args, "tn");
            let before = text[..start].lines().next_back().unwrap_or("");
            let above = text[..start].lines().rev().nth(1).unwrap_or("");
            let exempt =
                above.contains("// i18n: not text") || before.contains("// i18n: not text");
            if translated && !exempt {
                let line = text[..start].matches('\n').count() + 1;
                glued.push(format!("{}:{line}", path.display()));
            }
        }
    }
    assert!(
        glued.is_empty(),
        "translated text glued in format!: {glued:#?}"
    );
}

/// Whether `text` calls the macro `name` (`name!(`), not one whose name only ends so
/// (`print!(` isn't `t!(`). The needle is built here so this file reads no key itself.
fn calls(text: &str, name: &str) -> bool {
    let needle = format!("{name}{}", "!(");
    text.match_indices(&needle).any(|(at, _)| {
        !text[..at]
            .chars()
            .next_back()
            .is_some_and(|ch| ch.is_alphanumeric() || ch == '_')
    })
}

/// The text of a macro call's arguments, up to its closing parenthesis (strings skipped).
fn macro_args(rest: &str) -> &str {
    let (mut depth, mut in_string, mut escaped) = (1, false, false);
    for (at, ch) in rest.char_indices() {
        if in_string {
            match ch {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return &rest[..at];
                }
            }
            _ => {}
        }
    }
    rest
}

/// The languages besides English and the plural forms each uses (as `plural::category`
/// picks them; `scripts/i18n/langs.py` has the same table).
const TRANSLATIONS: [(&str, &[&str]); 12] = [
    ("de", &["_one", "_other"]),
    ("fr", &["_one", "_other"]),
    ("es", &["_one", "_other"]),
    ("pt-BR", &["_one", "_other"]),
    ("it", &["_one", "_other"]),
    ("pl", &["_one", "_few", "_many"]),
    ("ru", &["_one", "_few", "_many"]),
    ("uk", &["_one", "_few", "_many"]),
    ("zh-CN", &["_other"]),
    ("ja", &["_other"]),
    ("ko", &["_other"]),
    ("tr", &["_one", "_other"]),
];

/// A translation file (`scripts/i18n/build.py`'s format): `key: "JSON text"` lines.
fn translation(path: &Path) -> BTreeMap<String, String> {
    let text = std::fs::read_to_string(path).unwrap();
    let mut keys = BTreeMap::new();
    for line in text.lines() {
        if line.is_empty() || line.starts_with(['#', '_']) {
            continue;
        }
        let (key, value) = line
            .split_once(": ")
            .unwrap_or_else(|| panic!("{}: not a key line: {line}", path.display()));
        let value: String = serde_json::from_str(value)
            .unwrap_or_else(|err| panic!("{}: {key}: {err}", path.display()));
        assert!(
            keys.insert(key.to_owned(), value).is_none(),
            "{}: {key} twice",
            path.display()
        );
    }
    keys
}

/// Each translation has every key of app.yml, a counted string in exactly the forms its
/// language uses, and the English text's placeholders (a form may leave out %{count}).
#[test]
fn every_translation_has_every_key_and_placeholder() {
    let english = locale();
    let mut problems = Vec::new();
    for entry in std::fs::read_dir(root().join("locales")).unwrap() {
        let path = entry.unwrap().path();
        let Some(lang) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if lang == "app" {
            continue;
        }
        let Some((_, forms)) = TRANSLATIONS.iter().find(|(name, _)| *name == lang) else {
            problems.push(format!(
                "locales/{lang}.yml: not a language in TRANSLATIONS"
            ));
            continue;
        };
        let translated = translation(&path);
        let mut wanted: BTreeMap<String, BTreeSet<&str>> = BTreeMap::new();
        for (key, text) in &english {
            match plural_base(key) {
                None => {
                    wanted.insert(key.clone(), placeholders(text));
                }
                Some(base) => {
                    for form in *forms {
                        wanted
                            .entry(format!("{base}{form}"))
                            .or_default()
                            .extend(placeholders(text));
                    }
                }
            }
        }
        for (key, allowed) in &wanted {
            let Some(text) = translated.get(key) else {
                problems.push(format!("locales/{lang}.yml: {key} is missing"));
                continue;
            };
            let have = placeholders(text);
            let must: BTreeSet<&str> = if plural_base(key).is_some() {
                allowed.iter().copied().filter(|p| *p != "count").collect()
            } else {
                allowed.clone()
            };
            if !must.is_subset(&have) || !have.is_subset(allowed) {
                problems.push(format!(
                    "locales/{lang}.yml: {key} has {have:?}, English {allowed:?}"
                ));
            }
        }
        for key in translated.keys() {
            if !wanted.contains_key(key) {
                problems.push(format!("locales/{lang}.yml: {key} isn't a key for {lang}"));
            }
        }
    }
    for (lang, _) in TRANSLATIONS {
        if !root().join(format!("locales/{lang}.yml")).is_file() {
            problems.push(format!("locales/{lang}.yml is missing"));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
