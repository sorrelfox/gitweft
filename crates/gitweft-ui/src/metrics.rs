//! Sizes of the Graphite design, scaled by the user's zoom, density and text sizes. Views
//! read them with `cx.metrics()` instead of pixel constants, so every setting reaches them.

use gitweft_core::settings::{Appearance, Density};
use gpui_kit::{Pixels, px};

/// Text sizes. `ui` is the body text (13 px in the design); the others follow it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextSizes {
    pub ui: Pixels,
    /// 12.5 px: sidebar rows, secondary lines.
    pub small: Pixels,
    /// 11.5 px: column headers, captions, the status bar.
    pub xs: Pixels,
    /// 11 px: uppercase section labels, counts.
    pub label: Pixels,
    /// 12 px: SHAs, dates, paths, code.
    pub mono: Pixels,
    /// 11 px: pills, badges, small mono counts.
    pub mono_small: Pixels,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    pub zoom: f32,
    /// gpui-kit's rem: its widget heights and spacing are in rem (`h_8` = 2 rem = 28 px).
    pub rem: Pixels,
    pub text: TextSizes,
    /// Graph, history and other list rows: 32 comfortable, 28 compact.
    pub row: Pixels,
    /// Sidebar branch rows: 30 / 28.
    pub sidebar_row: Pixels,
    /// File rows in the staging and details panels.
    pub file_row: Pixels,
    /// Column headers and small section headers.
    pub header: Pixels,
    /// Panel section headers ("Unstaged", "Commit").
    pub section: Pixels,
    pub title_bar: Pixels,
    pub toolbar: Pixels,
    pub banner: Pixels,
    pub status_bar: Pixels,
    pub lane: Pixels,
    pub stroke: Pixels,
    pub node: Pixels,
    pub head_node: Pixels,
    pub head_ring: Pixels,
    pub pill: Pixels,
    pub code_line: Pixels,
    pub conflict_line: Pixels,
    pub radius: Pixels,
    pub radius_lg: Pixels,
}

/// The design's rem, before zoom.
const REM: f32 = 14.0;

impl Metrics {
    pub fn new(appearance: &Appearance) -> Self {
        let zoom = appearance.zoom;
        let z = |value: f32| px(value * zoom);
        let ui = appearance.ui_font_size;
        let mono = appearance.mono_font_size;
        // Rows are designed for 13 px text; bigger text makes them taller in proportion.
        let grow = (ui / 13.0).max(1.0);
        let rows = |comfortable: f32, compact: f32| {
            let base = match appearance.density {
                Density::Comfortable => comfortable,
                Density::Compact => compact,
            };
            z((base * grow).round())
        };
        // Code lines are 20 in the design (18 when compact), taller for bigger code text.
        let code_line = match appearance.density {
            Density::Comfortable => z(20f32.max((mono * 1.66).round())),
            Density::Compact => z(18f32.max((mono * 1.5).round())),
        };
        Self {
            zoom,
            rem: z(REM),
            text: TextSizes {
                ui: z(ui),
                small: z(ui - 0.5),
                xs: z(ui - 1.5),
                label: z(ui - 2.0),
                mono: z(mono),
                mono_small: z(mono - 1.0),
            },
            row: rows(32.0, 28.0),
            sidebar_row: rows(30.0, 28.0),
            file_row: rows(28.0, 26.0),
            header: z(28.0),
            section: z(40.0),
            title_bar: z(40.0),
            toolbar: z(46.0),
            banner: z(40.0),
            status_bar: z(26.0),
            lane: z(14.0),
            stroke: z(2.0),
            node: z(4.0),
            head_node: z(5.5),
            head_ring: z(8.5),
            pill: z(20.0),
            code_line,
            conflict_line: code_line + z(2.0),
            radius: z(6.0),
            radius_lg: z(8.0),
        }
    }

    /// `value` design pixels at the current zoom: a gap, padding or size of the design that
    /// no named size above covers.
    pub fn dp(&self, value: f32) -> Pixels {
        px(value * self.zoom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn appearance(density: Density, zoom: f32, ui: f32) -> Appearance {
        Appearance {
            density,
            zoom,
            ui_font_size: ui,
            ..Appearance::default()
        }
    }

    #[test]
    fn the_defaults_are_the_design() {
        let metrics = Metrics::new(&Appearance::default());
        assert_eq!(metrics.row, px(32.0));
        assert_eq!(metrics.sidebar_row, px(30.0));
        assert_eq!(metrics.rem, px(14.0));
        assert_eq!(metrics.text.ui, px(13.0));
        assert_eq!(metrics.text.mono, px(12.0));
        assert_eq!(metrics.code_line, px(20.0));
        assert_eq!(metrics.title_bar, px(40.0));
    }

    #[test]
    fn density_and_zoom() {
        let compact = Metrics::new(&appearance(Density::Compact, 1.0, 13.0));
        assert_eq!(compact.row, px(28.0));
        // Code lines follow the density too.
        assert_eq!(compact.code_line, px(18.0));
        assert_eq!(compact.conflict_line, px(20.0));
        let zoomed = Metrics::new(&appearance(Density::Comfortable, 1.5, 13.0));
        assert_eq!(zoomed.row, px(48.0));
        assert_eq!(zoomed.rem, px(21.0));
        assert_eq!(zoomed.text.ui, px(19.5));
        assert_eq!(zoomed.dp(8.0), px(12.0));
    }

    #[test]
    fn bigger_text_gets_taller_rows() {
        let big = Metrics::new(&appearance(Density::Compact, 1.0, 16.0));
        // 28 × 16/13
        assert_eq!(big.row, px(34.0));
        assert!(big.sidebar_row >= big.text.ui * 2.0);
    }
}
