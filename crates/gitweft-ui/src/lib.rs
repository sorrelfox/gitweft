//! Gitweft's design system ("Graphite"): bundled fonts and icons, the
//! palette and metrics resolved from the settings, the gpui-kit theme built from them, and
//! the settings store.
//!
//! Views take colors from `cx.palette()` and sizes from `cx.metrics()` (import
//! [`LookExt`]), never from gpui-kit theme fields or pixel literals.

pub mod current;
pub mod flow;
pub mod fonts;
pub mod icons;
pub mod kit_theme;
pub mod metrics;
pub mod palette;
pub mod store;
pub mod truncated;
pub mod widgets;

pub use icons::{AppAssets, AppIcon};
pub use metrics::Metrics;
pub use palette::{ColorToken, Palette};
pub use store::{Look, LookExt, SettingsStore, apply};
