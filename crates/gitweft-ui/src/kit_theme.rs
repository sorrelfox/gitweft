//! The gpui-kit theme, built from our palette so the kit's own widgets (buttons, inputs,
//! menus, dialogs, tabs, scrollbars, notifications) look like the rest of Graphite.
//!
//! The config is built in code and applied with `Theme::change`, which sets colors, fonts,
//! sizes and radius together. Never change `ThemeColor` fields by hand afterwards: the kit
//! derives tokens (scrollbars, focus rings) from the config and they'd go stale.

use gpui_kit::component::{ThemeConfig, ThemeConfigColors, ThemeMode};
use gpui_kit::{Hsla, Rgba, SharedString};

use crate::metrics::Metrics;
use crate::palette::{Palette, ink_on, to_hex};

/// Font families, already checked to exist (GPUI panics on an unknown family).
pub struct Fonts {
    pub ui: SharedString,
    pub mono: SharedString,
}

/// `a` moved towards `b` by `t` (0–1), in RGB.
pub fn mix(a: Hsla, b: Hsla, t: f32) -> Hsla {
    let (a, b) = (Rgba::from(a), Rgba::from(b));
    let lerp = |x: f32, y: f32| x + (y - x) * t;
    Rgba {
        r: lerp(a.r, b.r),
        g: lerp(a.g, b.g),
        b: lerp(a.b, b.b),
        a: lerp(a.a, b.a),
    }
    .into()
}

pub fn kit_config(p: &Palette, fonts: &Fonts, metrics: &Metrics) -> ThemeConfig {
    let hex = |color: Hsla| Some(SharedString::from(to_hex(color)));
    let white = gpui_kit::white();
    // Hover and pressed states of colored buttons: lighter on dark, darker on light.
    let towards = if p.dark { white } else { gpui_kit::black() };
    let hover = |color: Hsla| hex(mix(color, towards, 0.12));
    let active = |color: Hsla| hex(mix(color, towards, 0.22));
    // Text on the colored buttons: white or dark, whichever is readable (#32).
    let (on_danger, on_success, on_warning) = (ink_on(p.del), ink_on(p.add), ink_on(p.modified));

    // Some fields (the base palette) are private, so no struct literal.
    let mut colors = ThemeConfigColors::default();
    colors.accent = hex(p.chip);
    colors.accent_foreground = hex(p.fg);
    colors.accordion = hex(p.panel);
    colors.background = hex(p.bg);
    colors.border = hex(p.line);
    colors.foreground = hex(p.fg);
    colors.button = hex(p.panel);
    colors.button_active = hex(p.line);
    colors.button_foreground = hex(p.fg);
    colors.button_hover = hex(p.chip);
    colors.button_primary = hex(p.acc);
    colors.button_primary_hover = hover(p.acc);
    colors.button_primary_active = active(p.acc);
    colors.button_primary_foreground = hex(p.acc_fg);
    colors.group_box = hex(p.panel2);
    colors.group_box_foreground = hex(p.fg);
    colors.group_box_title_foreground = hex(p.fg2);
    colors.caret = hex(p.acc);
    colors.danger = hex(p.del);
    colors.danger_hover = hover(p.del);
    colors.danger_active = active(p.del);
    colors.danger_foreground = hex(on_danger);
    // Colored buttons have tokens of their own, which otherwise fall back to soft tints of
    // the plain colors (a prompt's destructive "Delete" read as disabled).
    colors.button_danger = hex(p.del);
    colors.button_danger_hover = hover(p.del);
    colors.button_danger_active = active(p.del);
    colors.button_danger_foreground = hex(on_danger);
    colors.button_success = hex(p.add);
    colors.button_success_hover = hover(p.add);
    colors.button_success_active = active(p.add);
    colors.button_success_foreground = hex(on_success);
    colors.button_warning = hex(p.modified);
    colors.button_warning_hover = hover(p.modified);
    colors.button_warning_active = active(p.modified);
    colors.button_warning_foreground = hex(on_warning);
    colors.button_info = hex(p.acc);
    colors.button_info_hover = hover(p.acc);
    colors.button_info_active = active(p.acc);
    colors.button_info_foreground = hex(p.acc_fg);
    colors.description_list_label = hex(p.panel2);
    colors.description_list_label_foreground = hex(p.fg2);
    colors.drag_border = hex(p.acc);
    colors.drop_target = hex(p.acc.opacity(0.15));
    colors.info = hex(p.acc);
    colors.info_hover = hover(p.acc);
    colors.info_active = active(p.acc);
    colors.info_foreground = hex(p.acc_fg);
    colors.input = hex(p.line);
    colors.link = hex(p.acc);
    colors.link_hover = hover(p.acc);
    colors.link_active = active(p.acc);
    colors.list = hex(p.panel);
    colors.list_active = hex(p.sel);
    colors.list_active_border = hex(p.acc);
    colors.list_even = hex(p.panel);
    colors.list_head = hex(p.panel);
    colors.list_hover = hex(p.hover);
    colors.muted = hex(p.panel2);
    colors.muted_foreground = hex(p.fg3);
    colors.popover = hex(p.panel);
    colors.popover_foreground = hex(p.fg);
    colors.primary = hex(p.acc);
    colors.primary_hover = hover(p.acc);
    colors.primary_active = active(p.acc);
    colors.primary_foreground = hex(p.acc_fg);
    colors.progress_bar = hex(p.acc);
    colors.ring = hex(p.acc);
    colors.scrollbar = hex(p.bg.opacity(0.0));
    colors.scrollbar_thumb = hex(p.fg3.opacity(0.35));
    colors.scrollbar_thumb_hover = hex(p.fg3.opacity(0.6));
    colors.secondary = hex(p.chip);
    colors.secondary_hover = hex(p.line);
    colors.secondary_active = hex(p.line);
    colors.secondary_foreground = hex(p.fg);
    colors.selection = hex(p.acc.opacity(0.3));
    colors.sidebar = hex(p.panel);
    colors.sidebar_accent = hex(p.sel);
    colors.sidebar_accent_foreground = hex(p.fg);
    colors.sidebar_border = hex(p.line);
    colors.sidebar_foreground = hex(p.fg);
    colors.sidebar_primary = hex(p.acc);
    colors.sidebar_primary_foreground = hex(p.acc_fg);
    colors.skeleton = hex(p.chip);
    colors.slider_bar = hex(p.acc);
    colors.slider_thumb = hex(white);
    colors.success = hex(p.add);
    colors.success_hover = hover(p.add);
    colors.success_active = active(p.add);
    colors.success_foreground = hex(on_success);
    colors.switch = hex(p.line);
    colors.switch_thumb = hex(white);
    colors.tab = hex(p.bg);
    colors.tab_active = hex(p.panel);
    colors.tab_active_foreground = hex(p.fg);
    colors.tab_bar = hex(p.bg);
    colors.tab_bar_segmented = hex(p.chip);
    colors.tab_foreground = hex(p.fg2);
    colors.table = hex(p.panel);
    colors.table_active = hex(p.sel);
    colors.table_active_border = hex(p.acc);
    colors.table_even = hex(p.panel);
    colors.table_head = hex(p.panel);
    colors.table_head_foreground = hex(p.fg3);
    colors.table_hover = hex(p.hover);
    colors.table_row_border = hex(p.line);
    colors.table_foot = hex(p.panel);
    colors.table_foot_foreground = hex(p.fg2);
    colors.title_bar = hex(p.bg);
    colors.title_bar_border = hex(p.line);
    colors.status_bar = hex(p.panel);
    colors.status_bar_border = hex(p.line);
    colors.tiles = hex(p.bg);
    colors.warning = hex(p.modified);
    colors.warning_hover = hover(p.modified);
    colors.warning_active = active(p.modified);
    colors.warning_foreground = hex(on_warning);
    colors.overlay = hex(p.scrim);
    colors.window_border = hex(p.line);

    ThemeConfig {
        is_default: false,
        name: if p.dark {
            "Graphite Dark"
        } else {
            "Graphite Light"
        }
        .into(),
        mode: if p.dark {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        },
        font_size: Some(f32::from(metrics.rem)),
        font_family: Some(fonts.ui.clone()),
        mono_font_family: Some(fonts.mono.clone()),
        mono_font_size: Some(f32::from(metrics.text.mono)),
        radius: Some(f32::from(metrics.radius).round() as usize),
        radius_lg: Some(f32::from(metrics.radius_lg).round() as usize),
        // The design is flat: borders, no drop shadows on inputs and buttons.
        shadow: Some(false),
        colors,
        highlight: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::parse_hex;
    use gitweft_core::settings::Appearance;

    fn fonts() -> Fonts {
        Fonts {
            ui: "IBM Plex Sans".into(),
            mono: "IBM Plex Mono".into(),
        }
    }

    #[test]
    fn the_kit_config_carries_the_palette_fonts_and_sizes() {
        let metrics = Metrics::new(&Appearance::default());
        for dark in [true, false] {
            let palette = Palette::graphite(dark);
            let config = kit_config(&palette, &fonts(), &metrics);
            assert_eq!(config.mode.is_dark(), dark);
            assert_eq!(config.font_size, Some(14.0));
            assert_eq!(config.mono_font_size, Some(12.0));
            assert_eq!(config.radius, Some(6));
            assert_eq!(config.font_family.as_deref(), Some("IBM Plex Sans"));
            let colors = &config.colors;
            assert_eq!(colors.background.as_deref(), Some(&*to_hex(palette.bg)));
            assert_eq!(colors.border.as_deref(), Some(&*to_hex(palette.line)));
            assert_eq!(colors.primary.as_deref(), Some(&*to_hex(palette.acc)));
            assert_eq!(colors.list_active.as_deref(), Some(&*to_hex(palette.sel)));
            assert_eq!(colors.button_danger.as_deref(), Some(&*to_hex(palette.del)));
        }
    }

    #[test]
    fn every_color_is_hex_the_kit_can_read() {
        let metrics = Metrics::new(&Appearance::default());
        let config = kit_config(&Palette::graphite(true), &fonts(), &metrics);
        let json = serde_json::to_value(&config.colors).unwrap();
        let mut count = 0;
        for (key, value) in json.as_object().unwrap() {
            if let Some(text) = value.as_str() {
                assert!(parse_hex(text).is_some(), "{key}: {text}");
                count += 1;
            }
        }
        assert!(count > 90, "only {count} colors set");
    }

    #[test]
    fn mixing() {
        let black = gpui_kit::black();
        let white = gpui_kit::white();
        assert_eq!(to_hex(mix(black, white, 0.0)), "#000000ff");
        assert_eq!(to_hex(mix(black, white, 1.0)), "#ffffffff");
        assert_eq!(to_hex(mix(black, white, 0.5)), "#808080ff");
    }
}
