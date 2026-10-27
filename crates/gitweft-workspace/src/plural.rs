//! Plural forms. A counted string has one key per form (`history.count_one`,
//! `history.count_other`, and `_few` / `_many` in languages that have them); `tn!` picks the
//! form for the current locale's rules. A translation that lacks a form falls back to `_other`.
//!
//! Only grammar goes through here. Where the text differs because there is one thing to name
//! ("Discard changes to a.txt?" against "Discard changes to 3 files?"), the code chooses
//! between two keys itself: Russian's "one" form is also used for 21, 31 and so on.

/// `t!` for a counted string: `tn!("history.count", count = n)` reads the form of
/// `history.count` for `n`, with `count` (and any other arguments) as placeholders.
/// `count = n => shown` picks the form by `n` but shows `shown` (e.g. "74,123").
macro_rules! tn {
    ($base:literal, count = $count:expr => $shown:expr $(, $name:ident = $value:expr)* $(,)?) => {{
        let key = $crate::plural::key($base, $count as u64);
        let count = $shown;
        rust_i18n::t!(key.as_str(), count = count $(, $name = $value)*)
    }};
    ($base:literal, count = $count:expr $(, $name:ident = $value:expr)* $(,)?) => {{
        let count = $count;
        let key = $crate::plural::key($base, count as u64);
        rust_i18n::t!(key.as_str(), count = count $(, $name = $value)*)
    }};
}

/// The key of `base`'s form for `n` in the current locale.
pub(crate) fn key(base: &str, n: u64) -> String {
    let locale = rust_i18n::locale();
    let form = format!("{base}_{}", category(&locale, n));
    if form.ends_with("_other") || crate::_rust_i18n_try_translate(&locale, &form).is_some() {
        form
    } else {
        format!("{base}_other")
    }
}

/// The CLDR plural category of the whole number `n` in `locale`: "one", "few", "many" or
/// "other". Languages without rules here use English's (one for 1, other for the rest).
pub(crate) fn category(locale: &str, n: u64) -> &'static str {
    let language = locale.split(['-', '_']).next().unwrap_or(locale);
    match language {
        // East Slavic: one = 1, 21, 31 … (not 11); few = 2–4, 22–24 … (not 12–14); many = rest.
        "ru" | "uk" | "be" => match (n % 10, n % 100) {
            (1, tens) if tens != 11 => "one",
            (2..=4, tens) if !(12..=14).contains(&tens) => "few",
            _ => "many",
        },
        // Polish: one = 1 only; few = 2–4, 22–24 … (not 12–14); many = the rest.
        "pl" => match (n % 10, n % 100) {
            _ if n == 1 => "one",
            (2..=4, tens) if !(12..=14).contains(&tens) => "few",
            _ => "many",
        },
        // French and Brazilian Portuguese count 0 as one too.
        "fr" | "pt" => {
            if n <= 1 {
                "one"
            } else {
                "other"
            }
        }
        // No plural in the grammar.
        "ja" | "ko" | "zh" => "other",
        _ if n == 1 => "one",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::category;

    #[test]
    fn english_has_one_and_other() {
        assert_eq!(category("en", 0), "other");
        assert_eq!(category("en", 1), "one");
        assert_eq!(category("en-GB", 2), "other");
        assert_eq!(category("de", 21), "other");
    }

    #[test]
    fn russian_has_one_few_many() {
        let forms: Vec<_> = [1, 2, 4, 5, 11, 12, 14, 21, 22, 25, 101, 111, 0]
            .into_iter()
            .map(|n| category("ru", n))
            .collect();
        assert_eq!(
            forms,
            [
                "one", "few", "few", "many", "many", "many", "many", "one", "few", "many", "one",
                "many", "many"
            ]
        );
    }

    #[test]
    fn polish_french_and_the_languages_without_plurals() {
        let forms = |locale| -> Vec<_> {
            [0, 1, 2, 4, 5, 12, 22, 25, 101]
                .into_iter()
                .map(|n| category(locale, n))
                .collect()
        };
        assert_eq!(
            forms("pl"),
            [
                "many", "one", "few", "few", "many", "many", "few", "many", "many"
            ]
        );
        assert_eq!(
            forms("fr"),
            [
                "one", "one", "other", "other", "other", "other", "other", "other", "other"
            ]
        );
        assert_eq!(forms("pt-BR"), forms("fr"));
        assert!(
            forms("ja")
                .iter()
                .chain(&forms("zh-CN"))
                .all(|f| *f == "other")
        );
        assert_eq!(forms("tr")[1], "one");
    }

    #[test]
    fn english_keys_are_one_or_other() {
        assert_eq!(super::key("history.count", 1), "history.count_one");
        assert_eq!(super::key("history.count", 7), "history.count_other");
    }
}
