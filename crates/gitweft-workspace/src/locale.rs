//! The UI language, chosen once at start: `GITWEFT_LOCALE`, else the system's (gettext's
//! order: `LANGUAGE`, then the first of `LC_ALL`, `LC_MESSAGES` and `LANG`), among the
//! languages in `locales/` (English only so far).
//!
//! `GITWEFT_LOCALE=en-XA` is a pseudo-language for checking the UI: every translated text
//! shows accented and in «guillemets». English without them doesn't come from `app.yml`, and
//! a sentence split over two pairs was glued together from pieces.

use std::borrow::Cow;

/// The pseudo-language's name (a private-use region, as other pseudo-locales use).
pub(crate) const PSEUDO: &str = "en-XA";

/// Chooses the language; call before the first window opens.
pub(crate) fn init() {
    let available: Vec<String> = crate::_rust_i18n_available_locales()
        .iter()
        .map(|locale| locale.to_string())
        .collect();
    let var = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    let chosen = match var("GITWEFT_LOCALE") {
        Some(wanted) => pick(&wanted, &available, true),
        None => system_locales(var("LANGUAGE"), ["LC_ALL", "LC_MESSAGES", "LANG"].map(var))
            .iter()
            .find_map(|wanted| pick(wanted, &available, false)),
    };
    if let Some(locale) = chosen {
        rust_i18n::set_locale(&locale);
    }
}

/// The locale names the system asks for, most wanted first. gettext ignores `LANGUAGE`
/// (a `:` list) when the locale is C or POSIX.
fn system_locales(language: Option<String>, locale: [Option<String>; 3]) -> Vec<String> {
    let locale = locale.into_iter().flatten().next();
    let plain = locale
        .as_deref()
        .is_none_or(|name| matches!(base_name(name).as_str(), "C" | "POSIX"));
    let mut names = Vec::new();
    if !plain && let Some(language) = language {
        names.extend(
            language
                .split(':')
                .filter(|n| !n.is_empty())
                .map(str::to_owned),
        );
    }
    names.extend(locale);
    names
}

/// "de_DE.UTF-8@euro" → "de-DE".
fn base_name(name: &str) -> String {
    name.split(['.', '@'])
        .next()
        .unwrap_or_default()
        .replace('_', "-")
}

/// The language of `available` that the locale name `wanted` asks for: the same region, else
/// the same language, else another region of it (pt_PT gets pt-BR; not for Chinese,
/// whose regions are written in different scripts). The pseudo-language only when asked
/// for by name (`explicit`).
fn pick(wanted: &str, available: &[String], explicit: bool) -> Option<String> {
    let name = base_name(wanted);
    if name.is_empty() || name == "C" || name == "POSIX" {
        return None;
    }
    let language = name.split('-').next().unwrap_or_default().to_owned();
    let usable = |locale: &&String| explicit || locale.as_str() != PSEUDO;
    [name, language.clone()]
        .into_iter()
        .find_map(|candidate| {
            available
                .iter()
                .find(|locale| locale.eq_ignore_ascii_case(&candidate))
                .filter(usable)
                .cloned()
        })
        .or_else(|| {
            let prefix = format!("{}-", language.to_ascii_lowercase());
            available
                .iter()
                .filter(|_| language != "zh")
                .find(|locale| locale.to_ascii_lowercase().starts_with(&prefix))
                .filter(usable)
                .cloned()
        })
}

/// The pseudo-language's translations: the English text, accented and in guillemets, with
/// its `%{…}` placeholders untouched.
pub(crate) struct PseudoLocale;

impl rust_i18n::Backend for PseudoLocale {
    fn available_locales(&self) -> Vec<Cow<'_, str>> {
        vec![PSEUDO.into()]
    }

    fn translate(&self, locale: &str, key: &str) -> Option<Cow<'_, str>> {
        if locale != PSEUDO {
            return None;
        }
        crate::_rust_i18n_try_translate("en", key).map(|english| pseudo(&english).into())
    }

    fn messages_for_locale(&self, _: &str) -> Option<Vec<(Cow<'_, str>, Cow<'_, str>)>> {
        None
    }
}

fn pseudo(text: &str) -> String {
    let mut out = String::from("«");
    let mut rest = text;
    while let Some(at) = rest.find("%{") {
        out.extend(rest[..at].chars().map(accent));
        let end = rest[at..]
            .find('}')
            .map_or(rest.len(), |close| at + close + 1);
        out.push_str(&rest[at..end]);
        rest = &rest[end..];
    }
    out.extend(rest.chars().map(accent));
    out.push('»');
    out
}

fn accent(c: char) -> char {
    match c {
        'a' => 'á',
        'e' => 'é',
        'i' => 'í',
        'o' => 'ó',
        'u' => 'ú',
        'A' => 'Á',
        'E' => 'É',
        'I' => 'Í',
        'O' => 'Ó',
        'U' => 'Ú',
        _ => c,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn available() -> Vec<String> {
        ["de", "en", "en-XA", "pt-BR"].map(str::to_owned).to_vec()
    }

    #[test]
    fn picks_the_region_then_the_language() {
        assert_eq!(
            pick("de_AT.UTF-8", &available(), false).as_deref(),
            Some("de")
        );
        assert_eq!(
            pick("pt_BR.UTF-8", &available(), false).as_deref(),
            Some("pt-BR")
        );
        // Another region of the language rather than English, but not across Chinese
        // scripts.
        assert_eq!(pick("pt_PT", &available(), false).as_deref(), Some("pt-BR"));
        let chinese = ["en".to_owned(), "zh-CN".to_owned()];
        assert_eq!(pick("zh_TW", &chinese, false), None);
        assert_eq!(
            pick("zh_CN.UTF-8", &chinese, false).as_deref(),
            Some("zh-CN")
        );
        assert_eq!(
            pick("en_US.UTF-8@x", &available(), false).as_deref(),
            Some("en")
        );
        assert_eq!(pick("C.UTF-8", &available(), false), None);
        assert_eq!(pick("POSIX", &available(), false), None);
    }

    #[test]
    fn the_pseudo_language_only_by_name() {
        assert_eq!(pick("en-XA", &available(), true).as_deref(), Some("en-XA"));
        assert_eq!(
            pick("en_XA.UTF-8", &available(), false).as_deref(),
            Some("en")
        );
    }

    #[test]
    fn system_order_is_gettexts() {
        let some = |s: &str| Some(s.to_owned());
        assert_eq!(
            system_locales(some("ru:en"), [None, some("de_DE.UTF-8"), some("fr_FR")]),
            ["ru", "en", "de_DE.UTF-8"]
        );
        assert_eq!(
            system_locales(some("ru"), [some("C.UTF-8"), None, some("de_DE")]),
            ["C.UTF-8"]
        );
        assert!(system_locales(some("ru"), [None, None, None]).is_empty());
    }

    #[test]
    fn pseudo_text_keeps_placeholders() {
        assert_eq!(
            pseudo("Reset %{branch} to %{sha}"),
            "«Rését %{branch} tó %{sha}»"
        );
        assert_eq!(pseudo("Push"), "«Push»".replace('u', "ú"));
        assert_eq!(pseudo("broken %{x"), "«brókén %{x»");
    }
}
