//! The sidebar, the centre and the right panel. The side panels keep the widths they
//! are given (the design's, until a divider is dragged) and have only minimums; the centre
//! takes the rest and keeps a minimum of its own. Widths are saved in design pixels, so the
//! zoom scales them.
//!
//! Our own layout rather than gpui-kit's resizable group: the kit's drag passes what one
//! panel can't take on to the next, so dragging one side panel could shrink the other,
//! and a panel's size is only its first one (a zoom needed the kit's state rebuilt).

use super::*;

/// The design's side panel widths and the minimums, in design pixels.
pub(super) const SIDEBAR_WIDTH: f32 = 256.;
pub(super) const RIGHT_PANEL_WIDTH: f32 = 340.;
const SIDEBAR_MIN: f32 = 180.;
const RIGHT_PANEL_MIN: f32 = 260.;
/// A drag never makes the centre (graph, diff, …) narrower than this.
const CENTRE_MIN: f32 = 360.;
/// The dividers' grab area on each side of their 1 px line.
const GRAB: f32 = 4.;
/// When the window is too narrow for both side panels and the centre's minimum, the right
/// panel gives way first, then the sidebar, then the centre (flex shrink factors).
const RIGHT_SHRINK: f32 = 10_000.;
const SIDEBAR_SHRINK: f32 = 100.;

/// A divider being dragged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PanelDivider {
    Sidebar,
    Right,
}

/// The widths while a divider is dragged; they're saved when it is released.
#[derive(Default)]
pub(super) struct Panels {
    dragged: Option<(f32, f32)>,
}

/// The sidebar's and the right panel's widths in design pixels: the saved ones, or the
/// design's.
fn saved_widths(cx: &App) -> (f32, f32) {
    let layout = &SettingsStore::settings(cx).layout;
    (
        layout.sidebar_width.unwrap_or(SIDEBAR_WIDTH),
        layout.right_panel_width.unwrap_or(RIGHT_PANEL_WIDTH),
    )
}

/// The widths after dragging `divider` to `x`, all in design pixels from the layout's left
/// edge, in a layout `width` wide. The dragged panel stays within its minimum and what the
/// other panel and the centre's minimum leave; the other panel keeps its width.
pub(super) fn dragged_widths(
    divider: PanelDivider,
    x: f32,
    width: f32,
    (sidebar, right): (f32, f32),
) -> (f32, f32) {
    match divider {
        PanelDivider::Sidebar => {
            let most = width - right - CENTRE_MIN;
            (x.min(most).max(SIDEBAR_MIN), right)
        }
        PanelDivider::Right => {
            let most = width - sidebar - CENTRE_MIN;
            (sidebar, (width - x).min(most).max(RIGHT_PANEL_MIN))
        }
    }
}

impl Workspace {
    /// The side panels' widths as drawn now, in design pixels.
    pub(super) fn panel_widths(&self, cx: &App) -> (f32, f32) {
        self.panels.dragged.unwrap_or_else(|| saved_widths(cx))
    }

    fn on_divider_drag(
        &mut self,
        event: &DragMoveEvent<PanelDivider>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let zoom = cx.metrics().zoom;
        let x = f32::from(event.event.position.x - event.bounds.left()) / zoom;
        // The panels share what the two 1 px dividers leave.
        let width = f32::from(event.bounds.size.width - px(2.)) / zoom; // unscaled: 2 hairlines
        let widths = dragged_widths(*event.drag(cx), x, width, self.panel_widths(cx));
        if self.panels.dragged != Some(widths) {
            self.panels.dragged = Some(widths);
            cx.notify();
        }
    }

    /// A divider was released: the widths go to the settings.
    fn save_panel_widths(&mut self, cx: &mut Context<Self>) {
        if let Some((sidebar, right)) = self.panels.dragged.take() {
            SettingsStore::update(cx, |settings| {
                settings.layout.sidebar_width = Some(sidebar);
                settings.layout.right_panel_width = Some(right);
            });
            cx.notify();
        }
    }

    /// The three panels, `centre` being `None` before the graph is loaded.
    pub(super) fn render_panels(
        &self,
        sidebar: AnyElement,
        centre: Option<AnyElement>,
        right: AnyElement,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (p, m) = (*cx.palette(), *cx.metrics());
        let (sidebar_width, right_width) = self.panel_widths(cx);
        h_flex()
            .id("repo-layout")
            .size_full()
            .overflow_hidden()
            .on_drag_move(cx.listener(Self::on_divider_drag))
            // A drag ends wherever the mouse is released.
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.save_panel_widths(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.save_panel_widths(cx)),
            )
            .child(
                div()
                    .id("panel-sidebar")
                    .debug_selector(|| "panel-sidebar".into())
                    .h_full()
                    .flex_basis(m.dp(sidebar_width))
                    .map(|mut this| {
                        this.style().flex_shrink = Some(SIDEBAR_SHRINK);
                        this
                    })
                    .min_w(m.dp(SIDEBAR_MIN))
                    .overflow_hidden()
                    .child(sidebar),
            )
            .child(divider(PanelDivider::Sidebar, &p, &m))
            .child(
                // The centre takes what the side panels leave; a narrow one cuts its
                // content off rather than drawing it over the right panel.
                v_flex()
                    .id("panel-centre")
                    .debug_selector(|| "panel-centre".into())
                    .h_full()
                    .flex_grow_1()
                    .flex_basis(m.dp(CENTRE_MIN))
                    .flex_shrink_1()
                    .min_w_0()
                    .overflow_hidden()
                    .children(centre),
            )
            .child(divider(PanelDivider::Right, &p, &m))
            .child(
                div()
                    .id("panel-right")
                    .debug_selector(|| "panel-right".into())
                    .h_full()
                    .flex_basis(m.dp(right_width))
                    .map(|mut this| {
                        this.style().flex_shrink = Some(RIGHT_SHRINK);
                        this
                    })
                    .min_w(m.dp(RIGHT_PANEL_MIN))
                    .overflow_hidden()
                    .child(right),
            )
    }
}

/// A 1 px line between two panels. Its grab area reaches past it on both sides, drawn
/// after the panels so that neither covers it.
fn divider(which: PanelDivider, p: &Palette, m: &Metrics) -> impl IntoElement {
    let (id, grab_id) = match which {
        PanelDivider::Sidebar => ("divider-sidebar", "divider-sidebar-grab"),
        PanelDivider::Right => ("divider-right", "divider-right-grab"),
    };
    let (line, hover) = (p.line, p.acc);
    div()
        .id(id)
        .relative()
        .w(px(1.))
        .h_full()
        .flex_shrink_0()
        .bg(line)
        .child(deferred(
            div()
                .id(grab_id)
                .debug_selector(move || grab_id.into())
                .absolute()
                .top_0()
                .bottom_0()
                .left(-m.dp(GRAB))
                .w(m.dp(GRAB) * 2. + px(1.))
                .flex()
                .justify_center()
                .occlude()
                .cursor(CursorStyle::ResizeLeftRight)
                .group(grab_id)
                .child(
                    div()
                        .w(px(1.))
                        .h_full()
                        .group_hover(grab_id, move |style| style.bg(hover)),
                )
                .on_drag(which, |_, _, _, cx| cx.new(|_| EmptyView)),
        ))
}

#[cfg(test)]
mod tests {
    use super::*;
    // Rust's own `#[test]`: `gpui_kit::*` brings GPUI's `test` macro.
    use std::prelude::v1::test;

    #[test]
    fn a_side_panel_has_only_a_minimum_and_never_takes_the_other_ones_room() {
        let widths = (256., 340.);
        // Far past the old 520 maximum: up to the centre's minimum, the right panel as it was.
        assert_eq!(
            dragged_widths(PanelDivider::Sidebar, 700., 2000., widths),
            (700., 340.)
        );
        assert_eq!(
            dragged_widths(PanelDivider::Sidebar, 1900., 2000., widths),
            (2000. - 340. - CENTRE_MIN, 340.)
        );
        assert_eq!(
            dragged_widths(PanelDivider::Sidebar, 10., 2000., widths),
            (SIDEBAR_MIN, 340.)
        );
        // The right panel: measured from the right edge, the sidebar untouched.
        assert_eq!(
            dragged_widths(PanelDivider::Right, 1000., 2000., widths),
            (256., 1000.)
        );
        assert_eq!(
            dragged_widths(PanelDivider::Right, 0., 2000., widths),
            (256., 2000. - 256. - CENTRE_MIN)
        );
        assert_eq!(
            dragged_widths(PanelDivider::Right, 1990., 2000., widths),
            (256., RIGHT_PANEL_MIN)
        );
        // No room at all: the minimum wins, the other panel still keeps its width.
        assert_eq!(
            dragged_widths(PanelDivider::Sidebar, 400., 700., widths),
            (SIDEBAR_MIN, 340.)
        );
    }
}
