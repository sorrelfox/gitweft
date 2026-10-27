//! The colors of the Graphite design, for dark and light, plus the user's
//! overrides from the settings. Views read the resolved palette with `cx.palette()`; the
//! gpui-kit theme is built from it too (`kit_theme`), so kit widgets match.

use gitweft_core::settings::ColorPreset;
use std::collections::BTreeMap;

use gpui_kit::{Hsla, Rgba, rgba};

/// Lane colors; the graph layout's `PALETTE_SIZE` matches.
pub const LANES: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub dark: bool,
    /// Window background (the centre, title bar).
    pub bg: Hsla,
    /// Panels: toolbar, sidebar, right panel, status bar.
    pub panel: Hsla,
    /// Raised areas inside panels: banners, the WIP row, hunk headers, cards.
    pub panel2: Hsla,
    /// Borders and dividers.
    pub line: Hsla,
    pub fg: Hsla,
    /// Secondary text (authors, dates, paths).
    pub fg2: Hsla,
    /// Tertiary text (labels, SHAs, placeholders).
    pub fg3: Hsla,
    pub acc: Hsla,
    /// Text on the accent color.
    pub acc_fg: Hsla,
    /// Selected rows.
    pub sel: Hsla,
    /// Hovered rows and buttons.
    pub hover: Hsla,
    /// Count chips and tag pills.
    pub chip: Hsla,
    pub lanes: [Hsla; LANES],
    pub add: Hsla,
    pub del: Hsla,
    pub modified: Hsla,
    pub add_bg: Hsla,
    pub del_bg: Hsla,
    pub ours_bg: Hsla,
    pub theirs_bg: Hsla,
    pub moved_from_bg: Hsla,
    pub moved_to_bg: Hsla,
    /// Behind the matches of Find in the code views; the current one is drawn stronger.
    pub find_bg: Hsla,
    /// Behind dialogs.
    pub scrim: Hsla,
    /// The opacity of dimmed content: search misses in the graph, a conflict's dropped
    /// lines, a compared file marked viewed. High contrast keeps it readable.
    pub dim: f32,
}

/// An opaque color from `0xrrggbb`.
fn c(hex: u32) -> Hsla {
    rgba((hex << 8) | 0xff).into()
}

/// `0xrrggbb` at `alpha`.
fn ca(hex: u32, alpha: f32) -> Hsla {
    c(hex).opacity(alpha)
}

/// WCAG's relative luminance of an opaque color.
pub fn luminance(color: Hsla) -> f32 {
    let rgb = color.to_rgb();
    let channel = |v: f32| {
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(rgb.r) + 0.7152 * channel(rgb.g) + 0.0722 * channel(rgb.b)
}

/// WCAG's contrast ratio of two opaque colors, 1 to 21.
pub fn contrast(a: Hsla, b: Hsla) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// Text readable on `background` (a lane, a change color, a colored button): white or a
/// warm dark ink, whichever contrasts more; black in the few mid-tones where neither
/// reaches WCAG AA's 4.5:1 (black always does when white doesn't).
pub fn ink_on(background: Hsla) -> Hsla {
    let (white, dark) = (c(0xffffff), c(0x1a1200));
    let best = if contrast(white, background) >= contrast(dark, background) {
        white
    } else {
        dark
    };
    if contrast(best, background) >= 4.5 {
        best
    } else {
        c(0x000000)
    }
}

/// The selection is the accent at this alpha, unless the user picks one.
fn selection_alpha(dark: bool) -> f32 {
    if dark { 0.14 } else { 0.10 }
}

impl Palette {
    /// Graphite as designed (the design's CSS variables), with fg3 and the dark accent's
    /// text brought to WCAG AA (the user's choice).
    pub fn graphite(dark: bool) -> Self {
        if dark {
            Self {
                dark,
                bg: c(0x14161a),
                panel: c(0x1a1d23),
                panel2: c(0x20242b),
                line: c(0x2a2f38),
                fg: c(0xe6e8ec),
                fg2: c(0x9aa3b2),
                // The design's #6b7280 was 3.2–3.8:1 on the grounds; lifted to WCAG AA.
                fg3: c(0x858b96),
                acc: c(0x5b9dff),
                // White on this blue was 2.7:1: the dark ink, 6.8:1.
                acc_fg: ink_on(c(0x5b9dff)),
                sel: ca(0x5b9dff, selection_alpha(true)),
                hover: ca(0xffffff, 0.04),
                chip: c(0x242933),
                lanes: [
                    c(0xf25f83),
                    c(0x41c97e),
                    c(0x3fb8f5),
                    c(0xa98bfa),
                    c(0xf5b73c),
                    c(0x31d0c0),
                    c(0xfb8f4a),
                    c(0xe879f9),
                ],
                add: c(0x3ec97a),
                del: c(0xf0567a),
                modified: c(0xe0a521),
                add_bg: ca(0x3ec97a, 0.14),
                del_bg: ca(0xf0567a, 0.14),
                ours_bg: ca(0x3fb8f5, 0.14),
                theirs_bg: ca(0xa98bfa, 0.16),
                moved_from_bg: ca(0xe879f9, 0.16),
                moved_to_bg: ca(0x31d0c0, 0.16),
                find_bg: ca(0xf5b73c, 0.28),
                scrim: ca(0x000000, 0.5),
                dim: 0.45,
            }
        } else {
            Self {
                dark,
                bg: c(0xe9ecf0),
                panel: c(0xffffff),
                panel2: c(0xf4f6f8),
                line: c(0xdbe0e7),
                fg: c(0x1a1d23),
                fg2: c(0x5b6472),
                // The design's #8a93a1 was 2.6–3.1:1 on the grounds; darkened to WCAG AA.
                fg3: c(0x656b76),
                acc: c(0x2f6fe4),
                acc_fg: c(0xffffff),
                sel: ca(0x2f6fe4, selection_alpha(false)),
                hover: ca(0x000000, 0.035),
                chip: c(0xeef0f3),
                lanes: [
                    c(0xd9335f),
                    c(0x1f9d5b),
                    c(0x1b8fd6),
                    c(0x7c5cf0),
                    c(0xc98a00),
                    c(0x0f9f92),
                    c(0xe0691f),
                    c(0xc03fd9),
                ],
                add: c(0x1f9d5b),
                del: c(0xd9335f),
                modified: c(0xb17a00),
                add_bg: ca(0x1f9d5b, 0.12),
                del_bg: ca(0xd9335f, 0.10),
                ours_bg: ca(0x1b8fd6, 0.12),
                theirs_bg: ca(0x7c5cf0, 0.12),
                moved_from_bg: ca(0xc03fd9, 0.12),
                moved_to_bg: ca(0x0f9f92, 0.12),
                find_bg: ca(0xd99a00, 0.26),
                scrim: ca(0x1a1d23, 0.3),
                dim: 0.45,
            }
        }
    }

    /// High contrast: black on white (white on black), lines and quieter text still well
    /// above WCAG AA, lanes and change colors at full strength, stronger tints.
    pub fn high_contrast(dark: bool) -> Self {
        if dark {
            Self {
                dark,
                bg: c(0x000000),
                panel: c(0x0a0a0a),
                panel2: c(0x161616),
                line: c(0x707070),
                fg: c(0xffffff),
                fg2: c(0xdadada),
                fg3: c(0xb4b4b4),
                acc: c(0x6cb6ff),
                acc_fg: c(0x000000),
                sel: ca(0x6cb6ff, 0.30),
                hover: ca(0xffffff, 0.10),
                chip: c(0x262626),
                lanes: [
                    c(0xff6b8b),
                    c(0x4ce08a),
                    c(0x5cc8ff),
                    c(0xc3a6ff),
                    c(0xffd24a),
                    c(0x3fe6d4),
                    c(0xffa25c),
                    c(0xf592ff),
                ],
                add: c(0x4ce08a),
                del: c(0xff6b8b),
                modified: c(0xffd24a),
                add_bg: ca(0x4ce08a, 0.24),
                del_bg: ca(0xff6b8b, 0.24),
                ours_bg: ca(0x5cc8ff, 0.24),
                theirs_bg: ca(0xc3a6ff, 0.26),
                moved_from_bg: ca(0xf592ff, 0.26),
                moved_to_bg: ca(0x3fe6d4, 0.26),
                find_bg: ca(0xffd24d, 0.40),
                scrim: ca(0x000000, 0.65),
                dim: 0.75,
            }
        } else {
            Self {
                dark,
                bg: c(0xffffff),
                panel: c(0xffffff),
                panel2: c(0xf0f0f0),
                line: c(0x5c5c5c),
                fg: c(0x000000),
                fg2: c(0x1f1f1f),
                fg3: c(0x3d3d3d),
                acc: c(0x0047b3),
                acc_fg: c(0xffffff),
                sel: ca(0x0047b3, 0.18),
                hover: ca(0x000000, 0.07),
                chip: c(0xe6e6e6),
                lanes: [
                    c(0xb0003a),
                    c(0x006b2e),
                    c(0x005a9e),
                    c(0x4b2bb3),
                    c(0x7a5200),
                    c(0x006b63),
                    c(0xa33d00),
                    c(0x8f00a3),
                ],
                add: c(0x006b2e),
                del: c(0xb0003a),
                modified: c(0x7a5200),
                add_bg: ca(0x006b2e, 0.16),
                del_bg: ca(0xb0003a, 0.14),
                ours_bg: ca(0x005a9e, 0.16),
                theirs_bg: ca(0x4b2bb3, 0.16),
                moved_from_bg: ca(0x8f00a3, 0.16),
                moved_to_bg: ca(0x006b63, 0.16),
                find_bg: ca(0xb07800, 0.30),
                scrim: ca(0x000000, 0.45),
                dim: 0.75,
            }
        }
    }

    /// The built-in colors of `preset`.
    pub fn preset(preset: ColorPreset, dark: bool) -> Self {
        match preset {
            ColorPreset::Graphite => Self::graphite(dark),
            ColorPreset::HighContrast => Self::high_contrast(dark),
        }
    }

    /// Graphite with the user's `overrides` (token key → hex) on top.
    pub fn resolve(dark: bool, overrides: &BTreeMap<String, String>) -> Self {
        Self::resolve_preset(ColorPreset::Graphite, dark, overrides)
    }

    /// `preset`'s colors with the user's `overrides` (token key → hex) on top. Unknown
    /// keys and values that aren't hex are ignored. The selection follows an overridden
    /// accent unless it's overridden too.
    pub fn resolve_preset(
        preset: ColorPreset,
        dark: bool,
        overrides: &BTreeMap<String, String>,
    ) -> Self {
        let base = Self::preset(preset, dark);
        let mut palette = base;
        for (key, value) in overrides {
            if let (Some(token), Some(color)) = (ColorToken::from_key(key), parse_hex(value)) {
                token.set(&mut palette, color);
            }
        }
        let own_selection = overrides
            .get(ColorToken::Selection.key())
            .is_some_and(|value| parse_hex(value).is_some());
        let own_accent = palette.acc != base.acc;
        if !own_selection && own_accent {
            palette.sel = palette.acc.opacity(base.sel.a);
        }
        palette
    }

    /// The color of graph lane `color` (the layout's `ColorId`).
    pub fn lane(&self, color: u16) -> Hsla {
        self.lanes[usize::from(color) % LANES]
    }
}

macro_rules! color_tokens {
    ($($variant:ident => $key:literal, ($($field:tt)+));* $(;)?) => {
        /// A palette color the user can change, with its stable key in `settings.json`.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum ColorToken {
            $($variant),*
        }

        impl ColorToken {
            pub const ALL: &[ColorToken] = &[$(ColorToken::$variant),*];

            pub fn key(self) -> &'static str {
                match self {
                    $(ColorToken::$variant => $key),*
                }
            }

            pub fn from_key(key: &str) -> Option<Self> {
                match key {
                    $($key => Some(ColorToken::$variant),)*
                    _ => None,
                }
            }

            pub fn get(self, palette: &Palette) -> Hsla {
                match self {
                    $(ColorToken::$variant => palette.$($field)+),*
                }
            }

            pub fn set(self, palette: &mut Palette, color: Hsla) {
                match self {
                    $(ColorToken::$variant => palette.$($field)+ = color),*
                }
            }
        }
    };
}

color_tokens! {
    Background => "bg", (bg);
    Panel => "panel", (panel);
    Panel2 => "panel2", (panel2);
    Line => "line", (line);
    Foreground => "fg", (fg);
    Foreground2 => "fg2", (fg2);
    Foreground3 => "fg3", (fg3);
    Accent => "acc", (acc);
    AccentForeground => "acc_fg", (acc_fg);
    Selection => "sel", (sel);
    Hover => "hover", (hover);
    Chip => "chip", (chip);
    Lane0 => "lane.0", (lanes[0]);
    Lane1 => "lane.1", (lanes[1]);
    Lane2 => "lane.2", (lanes[2]);
    Lane3 => "lane.3", (lanes[3]);
    Lane4 => "lane.4", (lanes[4]);
    Lane5 => "lane.5", (lanes[5]);
    Lane6 => "lane.6", (lanes[6]);
    Lane7 => "lane.7", (lanes[7]);
    Add => "diff.add", (add);
    Delete => "diff.del", (del);
    Modified => "diff.mod", (modified);
    AddBackground => "diff.add_bg", (add_bg);
    DeleteBackground => "diff.del_bg", (del_bg);
    MovedFromBackground => "diff.moved_from_bg", (moved_from_bg);
    MovedToBackground => "diff.moved_to_bg", (moved_to_bg);
    FindBackground => "find.bg", (find_bg);
    OursBackground => "merge.ours_bg", (ours_bg);
    TheirsBackground => "merge.theirs_bg", (theirs_bg);
    Scrim => "scrim", (scrim);
}

/// `#rgb`, `#rrggbb` or `#rrggbbaa` as a color.
pub fn parse_hex(text: &str) -> Option<Hsla> {
    let digits = text.strip_prefix('#')?;
    if !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let full = match digits.len() {
        3 => digits
            .chars()
            .flat_map(|c| [c, c])
            .chain("ff".chars())
            .collect(),
        6 => format!("{digits}ff"),
        8 => digits.to_string(),
        _ => return None,
    };
    u32::from_str_radix(&full, 16)
        .ok()
        .map(|value| rgba(value).into())
}

/// `#rrggbbaa`, as gpui-kit's theme files and our settings take it.
pub fn to_hex(color: Hsla) -> String {
    let Rgba { r, g, b, a } = Rgba::from(color);
    let byte = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}{:02x}",
        byte(r),
        byte(g),
        byte(b),
        byte(a)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graphite_is_the_design() {
        let dark = Palette::graphite(true);
        assert_eq!(to_hex(dark.bg), "#14161aff");
        assert_eq!(to_hex(dark.acc), "#5b9dffff");
        assert_eq!(to_hex(dark.lanes[7]), "#e879f9ff");
        let light = Palette::graphite(false);
        assert_eq!(to_hex(light.panel), "#ffffffff");
        assert_eq!(to_hex(light.fg2), "#5b6472ff");
        // rgba(47,111,228,.1)
        assert_eq!(to_hex(light.sel), "#2f6fe41a");
    }

    #[test]
    fn token_keys_are_unique_and_round_trip() {
        let mut keys: Vec<_> = ColorToken::ALL.iter().map(|token| token.key()).collect();
        for token in ColorToken::ALL {
            assert_eq!(ColorToken::from_key(token.key()), Some(*token));
        }
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), ColorToken::ALL.len());
        assert_eq!(ColorToken::from_key("nope"), None);
    }

    #[test]
    fn overrides_replace_tokens_and_bad_ones_are_ignored() {
        let overrides = BTreeMap::from([
            ("bg".to_string(), "#000000".to_string()),
            ("lane.3".to_string(), "#123".to_string()),
            ("fg".to_string(), "white".to_string()),
            ("nope".to_string(), "#ffffff".to_string()),
        ]);
        let palette = Palette::resolve(true, &overrides);
        assert_eq!(to_hex(palette.bg), "#000000ff");
        assert_eq!(to_hex(palette.lanes[3]), "#112233ff");
        assert_eq!(palette.fg, Palette::graphite(true).fg);
    }

    #[test]
    fn the_selection_follows_the_accent_unless_chosen() {
        let accent = BTreeMap::from([("acc".to_string(), "#ff0000".to_string())]);
        let palette = Palette::resolve(true, &accent);
        assert_eq!(to_hex(palette.sel), "#ff000024");

        let mut both = accent.clone();
        both.insert("sel".into(), "#00ff0080".into());
        assert_eq!(to_hex(Palette::resolve(true, &both).sel), "#00ff0080");
        assert_eq!(
            Palette::resolve(false, &BTreeMap::new()),
            Palette::graphite(false)
        );
    }

    #[test]
    fn hex_parsing() {
        assert_eq!(parse_hex("#abc").map(to_hex), Some("#aabbccff".into()));
        assert_eq!(parse_hex("#a1b2c3").map(to_hex), Some("#a1b2c3ff".into()));
        assert_eq!(parse_hex("#a1b2c380").map(to_hex), Some("#a1b2c380".into()));
        for bad in ["abc", "#ab", "#abcd", "#ggg", "#", "#abcdef0"] {
            assert!(parse_hex(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn lanes_wrap() {
        let palette = Palette::graphite(true);
        assert_eq!(palette.lane(9), palette.lanes[1]);
    }
}

#[cfg(test)]
mod contrast_tests {
    use super::*;

    /// `color` at `alpha` over the opaque `ground`: what the eye gets.
    fn over(color: Hsla, alpha: f32, ground: Hsla) -> Hsla {
        let (a, b) = (color.to_rgb(), ground.to_rgb());
        let mix = |x: f32, y: f32| x * alpha + y * (1. - alpha);
        Rgba {
            r: mix(a.r, b.r),
            g: mix(a.g, b.g),
            b: mix(a.b, b.b),
            a: 1.,
        }
        .into()
    }

    fn every_palette() -> Vec<(&'static str, Palette)> {
        vec![
            ("graphite dark", Palette::graphite(true)),
            ("graphite light", Palette::graphite(false)),
            ("high contrast dark", Palette::high_contrast(true)),
            ("high contrast light", Palette::high_contrast(false)),
        ]
    }

    #[test]
    fn ink_keeps_the_old_picks() {
        assert_eq!(ink_on(c(0xf5b73c)), c(0x1a1200));
        assert_eq!(ink_on(c(0x7c5cf0)), c(0xffffff));
    }

    /// The HEAD pill, status badges, avatars and the kit's colored buttons put `ink_on`
    /// text on these, in every preset.
    #[test]
    fn ink_is_readable_on_every_colored_ground() {
        for (name, p) in every_palette() {
            let grounds = p
                .lanes
                .iter()
                .copied()
                .chain([p.add, p.del, p.modified, p.acc]);
            for ground in grounds {
                let ratio = contrast(ink_on(ground), ground);
                assert!(ratio >= 4.5, "{name}: {} on {ground:?}", ratio);
            }
        }
        // Mid-tones where neither white nor the dark ink reaches 4.5:1 get black.
        for grey in (0..=255).map(|v| c(v * 0x010101)) {
            assert!(contrast(ink_on(grey), grey) >= 4.5, "{grey:?}");
        }
    }

    #[test]
    fn high_contrast_dimmed_text_meets_wcag() {
        for dark in [true, false] {
            let p = Palette::high_contrast(dark);
            for ground in [p.bg, p.panel, p.panel2] {
                for (name, text) in [("fg", p.fg), ("fg2", p.fg2), ("fg3", p.fg3)] {
                    let seen = over(text, p.dim, ground);
                    assert!(contrast(seen, ground) >= 4.5, "{name} dimmed, dark {dark}");
                }
                // Lanes are lines and dots: 3:1, WCAG's bar for graphics.
                for (ix, lane) in p.lanes.iter().enumerate() {
                    let seen = over(*lane, p.dim, ground);
                    assert!(
                        contrast(seen, ground) >= 3.,
                        "lane {ix} dimmed, dark {dark}"
                    );
                }
            }
        }
    }

    #[test]
    fn graphite_text_meets_wcag_aa() {
        for dark in [true, false] {
            let p = Palette::graphite(dark);
            for ground in [p.bg, p.panel, p.panel2] {
                for (name, text) in [("fg", p.fg), ("fg2", p.fg2), ("fg3", p.fg3)] {
                    let ratio = contrast(text, ground);
                    assert!(ratio >= 4.5, "{name} {ratio} on {ground:?}, dark {dark}");
                }
            }
            let ratio = contrast(p.acc_fg, p.acc);
            assert!(ratio >= 4.5, "on the accent {ratio}, dark {dark}");
        }
    }

    #[test]
    fn high_contrast_meets_wcag() {
        for dark in [true, false] {
            let p = Palette::high_contrast(dark);
            for ground in [p.bg, p.panel, p.panel2] {
                assert!(contrast(p.fg, ground) >= 15., "fg, dark {dark}");
                assert!(contrast(p.fg2, ground) >= 7., "fg2, dark {dark}");
                assert!(contrast(p.fg3, ground) >= 4.5, "fg3, dark {dark}");
                assert!(contrast(p.acc, ground) >= 4.5, "accent, dark {dark}");
            }
            assert!(contrast(p.line, p.bg) >= 3., "lines, dark {dark}");
            assert!(
                contrast(p.acc_fg, p.acc) >= 4.5,
                "on the accent, dark {dark}"
            );
            for (ix, lane) in p.lanes.iter().enumerate() {
                assert!(contrast(*lane, p.bg) >= 4.5, "lane {ix}, dark {dark}");
            }
            for (name, color) in [("add", p.add), ("del", p.del), ("modified", p.modified)] {
                assert!(contrast(color, p.bg) >= 4.5, "{name}, dark {dark}");
            }
        }
    }

    #[test]
    fn a_preset_resolves_with_overrides_on_top() {
        let none = BTreeMap::new();
        assert_eq!(
            Palette::resolve_preset(ColorPreset::Graphite, true, &none),
            Palette::graphite(true)
        );
        assert_eq!(
            Palette::resolve_preset(ColorPreset::HighContrast, false, &none),
            Palette::high_contrast(false)
        );
        let mut overrides = BTreeMap::new();
        overrides.insert("bg".to_owned(), "#123456".to_owned());
        let p = Palette::resolve_preset(ColorPreset::HighContrast, true, &overrides);
        assert_eq!(p.bg, parse_hex("#123456").unwrap());
        assert_eq!(p.fg, Palette::high_contrast(true).fg);
    }
}
