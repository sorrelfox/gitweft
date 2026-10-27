//! Text as the code views show it: tabs at tab stops, and widths in columns of the code
//! font, where wide characters (CJK, most emoji) take two (#42).

use std::borrow::Cow;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// `text` with each tab replaced by the spaces to the next tab stop, `width` columns apart
/// (the code views' Tab width setting, #80), counted in columns.
pub fn expand_tabs(text: &str, width: usize) -> Cow<'_, str> {
    if !text.contains('\t') {
        return Cow::Borrowed(text);
    }
    let width = width.max(1);
    let mut out = String::with_capacity(text.len() + width);
    let mut column = 0;
    for ch in text.chars() {
        if ch == '\t' {
            let spaces = width - column % width;
            out.extend(std::iter::repeat_n(' ', spaces));
            column += spaces;
        } else {
            out.push(ch);
            column += ch.width().unwrap_or(0);
        }
    }
    Cow::Owned(out)
}

/// The columns `text` takes (tabs expanded first).
pub fn columns(text: &str) -> usize {
    text.width()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_stop_at_multiples_of_the_width() {
        assert_eq!(expand_tabs("a\tb", 4), "a   b");
        assert_eq!(expand_tabs("abcd\te", 4), "abcd    e");
        assert_eq!(expand_tabs("\t\tx", 4), "        x");
        assert_eq!(expand_tabs("ab\tcd\tef", 4), "ab  cd  ef");
        // A wide character takes two columns before the tab.
        assert_eq!(expand_tabs("漢\tx", 4), "漢  x");
        assert!(matches!(expand_tabs("no tabs", 4), Cow::Borrowed(_)));
        // Other widths; zero is taken as one.
        assert_eq!(expand_tabs("ab\tc", 8), "ab      c");
        assert_eq!(expand_tabs("ab\tc", 2), "ab  c");
        assert_eq!(expand_tabs("ab\tc", 0), "ab c");
    }

    #[test]
    fn wide_characters_take_two_columns() {
        assert_eq!(columns("abc"), 3);
        assert_eq!(columns("漢字"), 4);
        assert_eq!(columns("かな END"), 8);
        assert_eq!(columns(""), 0);
    }
}
