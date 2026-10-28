//! Small presentation helpers shared by views.

use gitweft_core::settings::DateFormat;
use gpui_kit::*;
use rust_i18n::t;

/// Local time as `YYYY-MM-DD HH:MM`.
pub fn format_time(seconds: i64) -> String {
    if seconds == 0 {
        return String::new();
    }
    match gitweft_ui::current::date_format() {
        DateFormat::Full => local_time(seconds, true),
        DateFormat::Short => local_time(seconds, false),
        // Older than a month reads better as a date.
        DateFormat::Relative => relative_time(seconds).unwrap_or_else(|| local_time(seconds, true)),
    }
}

/// How long ago `seconds` was ("5 min ago"), or its date beyond 30 days.
pub fn ago(seconds: i64) -> String {
    relative_time(seconds).unwrap_or_else(|| local_time(seconds, true))
}

/// "just now", "5 min ago", "3 h ago", "12 d ago"; `None` beyond 30 days or in the future.
fn relative_time(seconds: i64) -> Option<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    let age = u64::try_from(now - seconds).ok()?;
    (age < 2_592_000).then(|| age_text(age))
}

/// An age in seconds as "just now", "5 min ago", "3 h ago" or "12 d ago".
pub fn age_text(age: u64) -> String {
    match age {
        0..60 => t!("time.just_now").into_owned(),
        60..3_600 => tn!("time.minutes_ago", count = age / 60).into_owned(),
        3_600..86_400 => tn!("time.hours_ago", count = age / 3_600).into_owned(),
        _ => tn!("time.days_ago", count = age / 86_400).into_owned(),
    }
}

/// `2026-08-29 12:27` (with the year) or `08-29 12:27`, in local time; the order and
/// separators come from the locale (`time.date_time`, `time.date_time_short`).
fn local_time(seconds: i64, with_year: bool) -> String {
    let Ok(time) = jiff::Timestamp::from_second(seconds) else {
        return String::new();
    };
    let time = time.to_zoned(jiff::tz::TimeZone::system());
    let month = format!("{:02}", time.month());
    let day = format!("{:02}", time.day());
    let hour = format!("{:02}", time.hour());
    let minute = format!("{:02}", time.minute());
    if with_year {
        let year = format!("{:04}", time.year());
        t!(
            "time.date_time",
            year = year,
            month = month,
            day = day,
            hour = hour,
            minute = minute
        )
    } else {
        t!(
            "time.date_time_short",
            month = month,
            day = day,
            hour = hour,
            minute = minute
        )
    }
    .into_owned()
}

/// Names in a sentence: "a.txt, b.txt, c.txt", with the locale's separator (`list.separator`).
pub fn join_list<S: AsRef<str>>(items: impl IntoIterator<Item = S>) -> String {
    let separator = t!("list.separator");
    let mut out = String::new();
    for (i, item) in items.into_iter().enumerate() {
        if i > 0 {
            out.push_str(&separator);
        }
        out.push_str(item.as_ref());
    }
    out
}

/// Up to two initials of a name ("Ada Lovelace" → "AL"), for avatars.
pub fn initials(name: &str) -> String {
    let mut words = name
        .split_whitespace()
        .filter_map(|word| word.chars().find(|c| c.is_alphanumeric()));
    let first = words.next();
    let last = words.next_back();
    [first, last]
        .into_iter()
        .flatten()
        .flat_map(char::to_uppercase)
        .collect()
}

/// Which lane color a person's avatar takes, from the email address, so a person keeps
/// their color. (Avatars are drawn locally; nothing is ever fetched from Gravatar or similar.)
pub fn avatar_lane(email: &str) -> usize {
    let hash = email
        .trim()
        .to_lowercase()
        .bytes()
        .fold(0x811c_9dc5u32, |hash, byte| {
            (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
        });
    hash as usize % gitweft_ui::palette::LANES
}

/// A person's initials on their lane color.
pub fn avatar(name: &str, email: &str, diameter: Pixels, p: &gitweft_ui::Palette) -> Div {
    let color = p.lanes[avatar_lane(email)];
    div()
        .flex_shrink_0()
        .size(diameter)
        .rounded_full()
        .bg(color)
        .flex()
        .items_center()
        .justify_center()
        .text_color(gitweft_ui::palette::ink_on(color))
        .text_size(diameter * 0.4)
        .font_weight(FontWeight::SEMIBOLD)
        .child(initials(name))
}

#[cfg(test)]
mod tests {
    // Not `super::*`: the gpui glob import would shadow `#[test]` with gpui's own macro.
    use super::{age_text, avatar_lane, initials, join_list, local_time};

    #[test]
    fn ages_take_their_plural_forms() {
        assert_eq!(age_text(20), "just now");
        assert_eq!(age_text(60), "1 min ago");
        assert_eq!(age_text(5 * 60), "5 min ago");
        assert_eq!(age_text(3 * 3600 + 5), "3 h ago");
        assert_eq!(age_text(400 * 86400), "400 d ago");
    }

    #[test]
    fn local_times_keep_their_shape() {
        let full = local_time(1_700_000_000, true);
        assert_eq!(full.len(), "2023-11-14 22:13".len(), "{full}");
        assert_eq!(local_time(1_700_000_000, false).len(), "11-14 22:13".len());
        // Beyond what a date can hold: nothing rather than a wrong date.
        assert_eq!(local_time(i64::MAX, true), "");
    }

    #[test]
    fn lists_use_the_locale_separator() {
        assert_eq!(
            join_list(["a.txt", "b.txt", "c.txt"]),
            "a.txt, b.txt, c.txt"
        );
        assert_eq!(join_list(&["only".to_owned()]), "only");
        assert_eq!(join_list(Vec::<String>::new()), "");
    }

    #[test]
    fn initials_of_names() {
        assert_eq!(initials("Ada Lovelace"), "AL");
        assert_eq!(initials("renovate[bot]"), "R");
        assert_eq!(initials("  jean-luc  de la  Picard "), "JP");
        assert_eq!(initials("Łukasz"), "Ł");
        assert_eq!(initials(""), "");
    }

    #[test]
    fn avatar_lane_ignores_email_case() {
        assert_eq!(avatar_lane("A@B.org"), avatar_lane("a@b.org "));
    }
}
