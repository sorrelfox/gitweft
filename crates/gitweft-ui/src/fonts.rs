//! Bundled fonts. They are compiled into the binary, so the UI looks the same on every
//! desktop and nothing is ever fetched (docs/OFFLINE.md). All of them are OFL-1.1; the
//! license texts are in `assets/fonts/*-OFL.txt` and ship with every package.

use std::borrow::Cow;

use gpui_kit::{App, SharedString};

/// The UI family of the Graphite design.
pub const UI_FONT: &str = "IBM Plex Sans";
/// The monospace family (SHAs, dates, paths, code) of the Graphite design.
pub const MONO_FONT: &str = "IBM Plex Mono";

/// A font family compiled into the binary.
pub struct BundledFamily {
    /// The family name, as GPUI (and `fc-scan`) sees it.
    pub name: &'static str,
    pub monospace: bool,
    faces: &'static [&'static [u8]],
}

/// Every bundled family, the defaults first. Plex covers the weights the design uses
/// (400/500/600) plus bold, which some views still ask for.
pub const BUNDLED: &[BundledFamily] = &[
    BundledFamily {
        name: UI_FONT,
        monospace: false,
        faces: &[
            include_bytes!("../../../assets/fonts/IBMPlexSans-Regular.ttf"),
            include_bytes!("../../../assets/fonts/IBMPlexSans-Medium.ttf"),
            include_bytes!("../../../assets/fonts/IBMPlexSans-SemiBold.ttf"),
            include_bytes!("../../../assets/fonts/IBMPlexSans-Bold.ttf"),
        ],
    },
    BundledFamily {
        name: MONO_FONT,
        monospace: true,
        faces: &[
            include_bytes!("../../../assets/fonts/IBMPlexMono-Regular.ttf"),
            include_bytes!("../../../assets/fonts/IBMPlexMono-Medium.ttf"),
            include_bytes!("../../../assets/fonts/IBMPlexMono-SemiBold.ttf"),
        ],
    },
    BundledFamily {
        name: "Inter",
        monospace: false,
        faces: &[
            include_bytes!("../../../assets/fonts/Inter-Regular.ttf"),
            include_bytes!("../../../assets/fonts/Inter-Medium.ttf"),
            include_bytes!("../../../assets/fonts/Inter-SemiBold.ttf"),
            include_bytes!("../../../assets/fonts/Inter-Bold.ttf"),
        ],
    },
    BundledFamily {
        name: "JetBrains Mono",
        monospace: true,
        faces: &[
            include_bytes!("../../../assets/fonts/JetBrainsMono-Regular.ttf"),
            include_bytes!("../../../assets/fonts/JetBrainsMono-Bold.ttf"),
        ],
    },
];

/// Adds every bundled face to GPUI's text system. On failure the UI falls back to system
/// fonts, which is ugly but usable, so it's reported rather than fatal.
pub fn register(cx: &mut App) {
    let fonts = BUNDLED
        .iter()
        .flat_map(|family| family.faces)
        .map(|bytes| Cow::Borrowed(*bytes))
        .collect();
    if let Err(err) = cx.text_system().add_fonts(fonts) {
        eprintln!("gitweft: failed to load bundled fonts, using system fonts: {err}");
    }
}

/// `wanted` if it's bundled or installed, otherwise `default`. GPUI panics when it has
/// to lay out text in a family it can't find, so a name from the settings (a font
/// uninstalled since, a typo in the file) must never reach it unchecked.
pub fn resolve_family(wanted: &str, default: &'static str, installed: &[String]) -> SharedString {
    let known = BUNDLED.iter().any(|family| family.name == wanted)
        || installed.iter().any(|name| name == wanted);
    if known {
        SharedString::from(wanted.to_string())
    } else {
        default.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_families_fall_back() {
        let installed = vec!["DejaVu Sans".to_string()];
        assert_eq!(resolve_family("Inter", UI_FONT, &installed), "Inter");
        assert_eq!(
            resolve_family("DejaVu Sans", UI_FONT, &installed),
            "DejaVu Sans"
        );
        assert_eq!(resolve_family("Comic Nope", UI_FONT, &installed), UI_FONT);
        assert_eq!(resolve_family("", MONO_FONT, &installed), MONO_FONT);
    }

    #[test]
    fn every_face_is_a_truetype_font() {
        for family in BUNDLED {
            assert!(!family.faces.is_empty(), "{} has no faces", family.name);
            for face in family.faces {
                assert_eq!(
                    &face[..4],
                    &[0, 1, 0, 0],
                    "{} face isn't TrueType",
                    family.name
                );
            }
        }
    }

    #[test]
    fn the_defaults_are_bundled_first() {
        assert_eq!(BUNDLED[0].name, UI_FONT);
        assert!(!BUNDLED[0].monospace);
        assert_eq!(BUNDLED[1].name, MONO_FONT);
        assert!(BUNDLED[1].monospace);
    }
}
