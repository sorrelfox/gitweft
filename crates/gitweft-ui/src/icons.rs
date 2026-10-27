//! The Graphite design's icons (`assets/icons`, drawn from the mockup's own paths: 24×24,
//! stroke 1.7, round caps, `currentColor`) and the asset source that serves them.
//!
//! gpui-kit embeds only ~100 of its Lucide icons; any other `IconName` compiles and draws
//! nothing. Ours live under their own prefix, so they can never shadow a kit icon.

use std::borrow::Cow;

use gpui_kit::assets::IconNamed;
use gpui_kit::{AssetSource, Result, SharedString};

/// Where our icons live in the asset namespace.
pub const ICON_PREFIX: &str = "gitweft/icons/";

macro_rules! app_icons {
    ($($variant:ident => $file:literal),* $(,)?) => {
        /// An icon of the Graphite design. Use it wherever gpui-kit takes an icon
        /// (`Icon::new(AppIcon::Fetch)`, `Button::icon(AppIcon::Fetch)`).
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum AppIcon {
            $($variant),*
        }

        impl AppIcon {
            pub const ALL: &[AppIcon] = &[$(AppIcon::$variant),*];

            /// The icon's path in the asset source (`gitweft/icons/<name>.svg`).
            pub fn asset_path(self) -> &'static str {
                match self {
                    $(AppIcon::$variant => concat!("gitweft/icons/", $file, ".svg")),*
                }
            }

            fn svg(self) -> &'static [u8] {
                match self {
                    $(AppIcon::$variant => include_bytes!(concat!("../../../assets/icons/", $file, ".svg"))),*
                }
            }
        }
    };
}

app_icons! {
    Activity => "activity",
    Box => "box",
    Branch => "branch",
    Check => "check",
    ChevronDown => "chevron-down",
    ChevronRight => "chevron-right",
    Close => "close",
    Diff => "diff",
    Fetch => "fetch",
    File => "file",
    Filter => "filter",
    Folder => "folder",
    History => "history",
    Info => "info",
    Merge => "merge",
    Moon => "moon",
    More => "more",
    Plus => "plus",
    Pull => "pull",
    Push => "push",
    Redo => "redo",
    Search => "search",
    Settings => "settings",
    Sidebar => "sidebar",
    Stash => "stash",
    Sun => "sun",
    Tag => "tag",
    Undo => "undo",
}

impl IconNamed for AppIcon {
    fn path(self) -> SharedString {
        self.asset_path().into()
    }
}

/// The application's asset source: our icons first, then gpui-kit's embedded assets.
/// The kit's source reports an unknown path as an error rather than `None`, so it has
/// to come last.
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path.starts_with(ICON_PREFIX) {
            return Ok(AppIcon::ALL
                .iter()
                .find(|icon| icon.asset_path() == path)
                .map(|icon| Cow::Borrowed(icon.svg())));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths: Vec<SharedString> = AppIcon::ALL
            .iter()
            .map(|icon| icon.asset_path())
            .filter(|asset| asset.starts_with(path))
            .map(Into::into)
            .collect();
        paths.extend(gpui_kit::assets::Assets.list(path)?);
        paths.sort_by(|a, b| a.as_ref().cmp(b.as_ref()));
        paths.dedup();
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_loads_as_a_stroked_svg() {
        for icon in AppIcon::ALL {
            let bytes = AppAssets
                .load(icon.asset_path())
                .unwrap()
                .unwrap_or_else(|| panic!("{icon:?} doesn't load"));
            let svg = std::str::from_utf8(&bytes).unwrap();
            assert!(svg.starts_with("<svg"), "{icon:?}");
            // GPUI draws icons as a mask tinted with the text color.
            assert!(svg.contains(r#"stroke="currentColor""#), "{icon:?}");
        }
    }

    #[test]
    fn icon_paths_are_unique_and_prefixed() {
        let mut paths: Vec<_> = AppIcon::ALL.iter().map(|icon| icon.asset_path()).collect();
        assert!(paths.iter().all(|path| path.starts_with(ICON_PREFIX)));
        paths.sort();
        paths.dedup();
        assert_eq!(paths.len(), AppIcon::ALL.len());
    }

    #[test]
    fn kit_icons_still_resolve() {
        let close = gpui_kit::assets::IconName::Close.path();
        assert!(AppAssets.load(&close).unwrap().is_some());
    }

    #[test]
    fn an_unknown_icon_of_ours_is_none() {
        assert!(AppAssets.load("gitweft/icons/nope.svg").unwrap().is_none());
    }

    #[test]
    fn listing_includes_ours() {
        assert_eq!(
            AppAssets.list(ICON_PREFIX).unwrap().len(),
            AppIcon::ALL.len()
        );
    }
}
