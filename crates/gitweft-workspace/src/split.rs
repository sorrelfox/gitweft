//! Dividers between the right panel's stacked sections: dragged, they move the line
//! between two sections; a double click puts it back where it was by default. The views
//! keep the dragged value while the mouse is down and save it to the layout settings when
//! it is released.

use gitweft_core::settings::{LayoutSettings, SHARE};
use gitweft_ui::{Metrics, Palette, SettingsStore};
use gpui_kit::*;

/// Each divider, and what it moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Split {
    /// Between Unstaged and Staged: Unstaged's share of the two lists.
    Lists,
    /// Above the commit box: its height in design pixels.
    CommitBox,
    /// Under a commit's message and facts: their height in design pixels.
    DetailsTop,
    /// Above the file preview: its share of the details panel.
    Preview,
}

/// The grab area on each side of the line.
const GRAB: f32 = 4.;

impl Split {
    fn name(self) -> &'static str {
        match self {
            Split::Lists => "split-lists",
            Split::CommitBox => "split-commit-box",
            Split::DetailsTop => "split-details-top",
            Split::Preview => "split-preview",
        }
    }

    fn slot(self, layout: &mut LayoutSettings) -> &mut Option<f32> {
        match self {
            Split::Lists => &mut layout.staging_split,
            Split::CommitBox => &mut layout.commit_box_height,
            Split::DetailsTop => &mut layout.details_top_height,
            Split::Preview => &mut layout.preview_share,
        }
    }

    /// The saved value, `None` for the default layout.
    pub(crate) fn saved(self, cx: &App) -> Option<f32> {
        let layout = &SettingsStore::settings(cx).layout;
        match self {
            Split::Lists => layout.staging_split,
            Split::CommitBox => layout.commit_box_height,
            Split::DetailsTop => layout.details_top_height,
            Split::Preview => layout.preview_share,
        }
    }

    /// Saves `value` (or the default, `None`).
    pub(crate) fn save(self, value: Option<f32>, cx: &mut App) {
        SettingsStore::update(cx, |settings| *self.slot(&mut settings.layout) = value);
    }
}

/// A share of the room between `start` and `end` for a line at `at` (all in pixels), within
/// the limits a share is kept in.
pub(crate) fn share(at: Pixels, start: Pixels, end: Pixels) -> f32 {
    let room = f32::from(end - start);
    if room <= 0. {
        return 0.5;
    }
    (f32::from(at - start) / room).clamp(SHARE.0, SHARE.1)
}

/// The value a view shows for `split`: the one being dragged, else the saved one.
pub(crate) fn current(split: Split, dragged: Option<(Split, f32)>, cx: &App) -> Option<f32> {
    match dragged {
        Some((which, value)) if which == split => Some(value),
        _ => split.saved(cx),
    }
}

/// Saves what was dragged, if anything; true when something was.
pub(crate) fn release(dragged: &mut Option<(Split, f32)>, cx: &mut App) -> bool {
    let Some((split, value)) = dragged.take() else {
        return false;
    };
    split.save(Some(value), cx);
    true
}

/// A grab area over the line between two stacked sections, which draw the line themselves
/// (their borders). It takes no room; it is drawn after the sections, so neither covers
/// half of it.
pub(crate) fn divider(split: Split, p: &Palette, m: &Metrics) -> impl IntoElement {
    let name = split.name();
    let hover = p.acc;
    div()
        .relative()
        .w_full()
        .h(px(0.))
        .flex_shrink_0()
        .child(deferred(
            div()
                .id(name)
                .debug_selector(move || name.into())
                .absolute()
                .left_0()
                .right_0()
                .top(-m.dp(GRAB))
                .h(m.dp(GRAB) * 2.)
                .flex()
                .items_center()
                .occlude()
                .cursor(CursorStyle::ResizeUpDown)
                .group(name)
                .child(
                    div()
                        .w_full()
                        .h(px(1.))
                        .group_hover(name, move |style| style.bg(hover)),
                )
                .on_drag(split, |_, _, _, cx| cx.new(|_| EmptyView))
                .on_click(move |event, _, cx| {
                    if event.click_count() == 2 {
                        split.save(None, cx);
                    }
                }),
        ))
}

#[cfg(test)]
mod tests {
    use super::*;
    // Rust's own `#[test]`: `gpui_kit::*` brings GPUI's `test` macro.
    use std::prelude::v1::test;

    #[test]
    fn a_share_is_where_the_line_is_within_its_limits() {
        assert_eq!(share(px(150.), px(100.), px(300.)), 0.25);
        assert_eq!(share(px(0.), px(100.), px(300.)), SHARE.0);
        assert_eq!(share(px(400.), px(100.), px(300.)), SHARE.1);
        assert_eq!(share(px(150.), px(300.), px(300.)), 0.5);
    }
}
