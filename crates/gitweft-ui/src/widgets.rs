//! Small pieces of the Graphite design that several views share.

use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use std::rc::Rc;

use crate::store::Look;
use crate::truncated::truncated;
use crate::{AppIcon, LookExt as _, Metrics, Palette};

/// A count next to a label ("Pull 3"): mono, accent on the selection tint, a pill.
pub fn count_chip(count: u32, p: &Palette, m: &Metrics) -> Div {
    div()
        .px(m.dp(5.))
        .rounded_full()
        .bg(p.sel)
        .text_color(p.acc)
        .font_family(crate::current::mono_font())
        .text_size(m.text.mono_small)
        .child(count.to_string())
}

/// A neutral count after a section title ("Unstaged 4").
pub fn section_count(count: usize, p: &Palette, m: &Metrics) -> Div {
    div()
        .px(m.dp(6.))
        .rounded_full()
        .bg(p.chip)
        .text_color(p.fg2)
        .font_family(crate::current::mono_font())
        .text_size(m.text.mono_small)
        .child(count.to_string())
}

/// A file's status letter on its color: 16 px, mono, bold, white ("M" on the modified color).
pub fn status_badge(letter: &'static str, color: Hsla, m: &Metrics) -> Div {
    let z = |value: f32| px(value * m.zoom);
    div()
        .size(z(16.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded(z(4.))
        .bg(color)
        .text_color(crate::palette::ink_on(color))
        .font_family(crate::current::mono_font())
        .text_size(z(10.))
        .font_weight(FontWeight::BOLD)
        .child(letter)
}

/// A path in mono: the folder part in the tertiary color, cut from the left when there's
/// no room ("…/albums/"), and the file name in full.
pub fn path_label(path: &str, p: &Palette, m: &Metrics) -> Div {
    let (dir, name) = split_path(path);
    h_flex()
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .font_family(crate::current::mono_font())
        .text_size(m.text.xs)
        .when(!dir.is_empty(), |this| {
            this.child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis_start()
                    // A cut text is narrower than its box: keep the slack on the left, so
                    // no gap opens before the name.
                    .text_right()
                    .text_color(p.fg3)
                    .child(dir.to_owned()),
            )
        })
        .child(
            div()
                .flex_shrink_0()
                .whitespace_nowrap()
                .text_color(p.fg)
                .child(name.to_owned()),
        )
}

/// `("web/src/", "app.ts")`: the folder part keeps its slash, and a folder path
/// (`"build/"`) is a name.
pub fn split_path(path: &str) -> (&str, &str) {
    let trimmed = path.strip_suffix('/').unwrap_or(path);
    match trimmed.rfind('/') {
        Some(slash) => path.split_at(slash + 1),
        None => ("", path),
    }
}

/// The header of a view that takes the centre (a diff, a blame, a file): 48 px on the
/// panel color, the title and a subtitle under it. Callers add their controls after it.
pub fn centre_header(
    title: impl Into<SharedString>,
    subtitle: impl Into<SharedString>,
    p: &Palette,
    m: &Metrics,
) -> Div {
    let z = |value: f32| px(value * m.zoom);
    h_flex()
        .min_h(z(48.))
        .flex_shrink_0()
        .px(z(14.))
        .py(z(6.))
        .gap(z(12.))
        .bg(p.panel)
        .border_b_1()
        .border_color(p.line)
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(
                    truncated("title", title)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.fg),
                )
                .child(
                    truncated("subtitle", subtitle)
                        .text_size(m.text.xs)
                        .text_color(p.fg3),
                ),
        )
}

/// The column labels over a list in the centre, in the graph's style: 28 px on the panel
/// color above a line, 11.5 px medium labels in the tertiary color. Give its cells the
/// widths of the rows' cells ([`list_row`] has the same padding and gaps).
pub fn list_header(p: &Palette, m: &Metrics) -> Div {
    let z = |value: f32| px(value * m.zoom);
    h_flex()
        .h(m.header)
        .w_full()
        .flex_shrink_0()
        .overflow_hidden()
        .px(z(12.))
        .gap(z(12.))
        .bg(p.panel)
        .border_b_1()
        .border_color(p.line)
        .text_size(m.text.xs)
        .font_weight(FontWeight::MEDIUM)
        .text_color(p.fg3)
}

/// A clickable row of a list in the centre: the graph's row height (it follows the
/// density), its hover and selection colors. A row too long for the panel is cut.
pub fn list_row(
    id: impl Into<ElementId>,
    selected: bool,
    p: &Palette,
    m: &Metrics,
) -> Stateful<Div> {
    let z = |value: f32| px(value * m.zoom);
    let hover = p.hover;
    h_flex()
        .id(id)
        .h(m.row)
        .w_full()
        .flex_shrink_0()
        .overflow_hidden()
        .px(z(12.))
        .gap(z(12.))
        .cursor_pointer()
        .when(selected, |row| row.bg(p.sel))
        .when(!selected, move |row| {
            row.hover(move |style| style.bg(hover))
        })
}

/// The text of a view that is loading, empty or failed, centred in it.
pub fn placeholder(text: impl Into<SharedString>, color: Hsla, m: &Metrics) -> Div {
    v_flex()
        .flex_1()
        .items_center()
        .justify_center()
        .px(px(24. * m.zoom))
        .text_size(m.text.small)
        .text_color(color)
        .child(text.into())
}

/// A small round status dot.
pub fn dot(color: Hsla, size: Pixels) -> Div {
    div().size(size).flex_shrink_0().rounded_full().bg(color)
}

/// A vertical divider between toolbar groups.
pub fn divider(p: &Palette, m: &Metrics) -> Div {
    div()
        .w(px(1.))
        .h(m.dp(20.))
        .mx(m.dp(6.))
        .flex_shrink_0()
        .bg(p.line)
}

/// A one-line banner under the toolbar: an icon in `tint`, the message, then its buttons
/// (an outlined action and a plain "Not now", in the design).
pub fn banner(
    icon: AppIcon,
    tint: Hsla,
    message: impl Into<SharedString>,
    buttons: Vec<AnyElement>,
    look: &Look,
) -> Div {
    let (p, m) = (&look.palette, &look.metrics);
    h_flex()
        .min_h(m.banner)
        .flex_shrink_0()
        .px(m.dp(14.))
        .py(m.dp(4.))
        .gap(m.dp(10.))
        .bg(p.panel2)
        .border_b_1()
        .border_color(p.line)
        .text_size(m.text.small)
        .text_color(p.fg2)
        .child(Icon::new(icon).text_color(tint).flex_shrink_0())
        .child(div().flex_1().min_w_0().child(message.into()))
        .children(buttons)
}

#[cfg(test)]
mod tests {
    use super::split_path;

    #[test]
    fn paths_split_before_the_name() {
        assert_eq!(split_path("web/src/app.ts"), ("web/src/", "app.ts"));
        assert_eq!(split_path("README.md"), ("", "README.md"));
        assert_eq!(split_path("target/build/"), ("target/", "build/"));
        assert_eq!(split_path("build/"), ("", "build/"));
        assert_eq!(
            split_path("old.rs → src/new.rs"),
            ("old.rs → src/", "new.rs")
        );
    }
}

/// What a [`LinkButton`] does when clicked (or given Enter or Space while it has the focus).
type OnClick = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

/// A focused [`LinkButton`]'s key context.
const LINK: &str = "LinkButton";

actions!(link, [PressLink]);

/// Enter and Space press the focused link. GPUI would turn them into a click by itself, but
/// only when no binding takes them first, and a link sits in lists whose Enter opens the row
/// and whose Space stages it: the link's own, deeper context wins over theirs.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("enter", PressLink, Some(LINK)),
        KeyBinding::new("space", PressLink, Some(LINK)),
    ]);
}

/// A text link that Tab reaches, for the clickable words the views are full of
/// ("Stage", "Restore", "Browse files"). It keeps a focus handle across frames by its id and
/// is a tab stop; while it has the focus it's outlined in the accent color, inside its own
/// bounds, so the layout doesn't move and a clipping row can't cut the outline off. Enter and
/// Space click it ([`PressLink`]). A click doesn't
/// take the focus, as the kit's buttons don't. A disabled link is dimmed and ignores clicks.
#[derive(IntoElement)]
pub struct LinkButton {
    id: ElementId,
    content: LinkContent,
    color: Hsla,
    /// The text's size (the surrounding text's when unset), or the icon's.
    size: Option<Pixels>,
    enabled: bool,
    on_click: Option<OnClick>,
    /// Hidden until this element group is hovered, unless `.1` (it's shown anyway).
    reveal: Option<(SharedString, bool)>,
    /// On hover: this text color and background (both unset: slightly faded).
    hover_color: Option<Hsla>,
    hover_bg: Option<Hsla>,
    /// A square box of this side, the content centered (an icon button).
    square: Option<Pixels>,
    /// Takes the width it's given and cuts its text short with an ellipsis.
    truncate: bool,
    tooltip: Option<SharedString>,
    /// The scroll container to keep the link visible in while it has the focus.
    reveal_in: Option<ScrollHandle>,
}

enum LinkContent {
    Text(SharedString),
    Icon(AppIcon),
}

impl LinkButton {
    /// A text link, in the surrounding text's size unless [`Self::size`] sets one.
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>, color: Hsla) -> Self {
        Self::with(id.into(), LinkContent::Text(label.into()), color)
    }

    /// An icon button (a close ×) in a `square` box, which the focus outline goes round.
    pub fn icon(id: impl Into<ElementId>, icon: AppIcon, color: Hsla, square: Pixels) -> Self {
        Self {
            square: Some(square),
            ..Self::with(id.into(), LinkContent::Icon(icon), color)
        }
    }

    fn with(id: ElementId, content: LinkContent, color: Hsla) -> Self {
        Self {
            id,
            content,
            color,
            size: None,
            enabled: true,
            on_click: None,
            reveal: None,
            reveal_in: None,
            hover_color: None,
            hover_bg: None,
            square: None,
            truncate: false,
            tooltip: None,
        }
    }

    /// The text's size, or the icon's.
    pub fn size(mut self, size: Pixels) -> Self {
        self.size = Some(size);
        self
    }

    pub fn hover_color(mut self, color: Hsla) -> Self {
        self.hover_color = Some(color);
        self
    }

    pub fn hover_bg(mut self, color: Hsla) -> Self {
        self.hover_bg = Some(color);
        self
    }

    /// Takes the width it's given and cuts its text short with an ellipsis.
    pub fn truncate(mut self) -> Self {
        self.truncate = true;
        self
    }

    pub fn tooltip(mut self, text: impl Into<SharedString>) -> Self {
        self.tooltip = Some(text.into());
        self
    }

    /// Invisible until the element group `group` is hovered (a row's link that would
    /// crowd every row), unless `shown` (the row is selected or has the keyboard cursor) or
    /// the link itself has the focus: Tab never lands on something that can't be seen.
    pub fn reveal_on(mut self, group: impl Into<SharedString>, shown: bool) -> Self {
        self.reveal = Some((group.into(), shown));
        self
    }

    /// Keeps the link in view inside `scroll` while it has the focus. GPUI doesn't scroll
    /// the focus into view, so Tab could land below the fold of a scrolling panel and Enter
    /// would act on something the user can't see.
    pub fn reveal_in(mut self, scroll: &ScrollHandle) -> Self {
        self.reveal_in = Some(scroll.clone());
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Runs `on_click` on a click, Enter or Space; the click goes no further (a link in a
    /// clickable row doesn't also click the row).
    pub fn on_click(
        mut self,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Rc::new(on_click));
        self
    }
}

impl RenderOnce for LinkButton {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        // GPUI takes a tab stop from the handle's own flag (`TabStopMap::insert`), not the
        // element's: the handle is made one, and `tab_index` marks the element, as the kit's
        // buttons do. Without both, Tab passed the links by.
        let focus = window
            .use_keyed_state(self.id.clone(), cx, |_, cx| {
                cx.focus_handle().tab_stop(true)
            })
            .read(cx)
            .clone();
        let focused = self.enabled && focus.is_focused(window);
        let reveal_in = self.reveal_in.clone().filter(|_| focused);
        let ring = cx.palette().acc;
        let radius = cx.metrics().radius;
        let color = self.color;
        let (hover_color, hover_bg) = (self.hover_color, self.hover_bg);
        // Hidden until its group is hovered, unless shown anyway or focused.
        let hidden = self
            .reveal
            .filter(|(_, shown)| !shown && !focused)
            .map(|(group, _)| group);
        let (text_size, content) = match self.content {
            LinkContent::Text(label) => (self.size, label.into_any_element()),
            LinkContent::Icon(icon) => {
                let icon = Icon::new(icon);
                let icon = match self.size {
                    Some(size) => icon.size(size),
                    None => icon,
                };
                (None, icon.into_any_element())
            }
        };
        let link = div()
            .id(self.id)
            .relative()
            .map(|this| {
                if self.truncate {
                    // A caller that cuts a link's text gives the whole of it in the tooltip.
                    this.min_w_0().truncate() // untooltipped: the tooltip is the caller's
                } else {
                    this.flex_shrink_0()
                }
            })
            .when_some(self.square, |this, side| {
                this.size(side)
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(radius)
            })
            .when_some(text_size, |this, size| this.text_size(size))
            .when_some(self.tooltip, |this, text| {
                this.tooltip(move |window, cx| Tooltip::new(text.clone()).build(window, cx))
            })
            .text_color(if hidden.is_some() {
                transparent_black()
            } else {
                color
            })
            .when_some(hidden, |this, group| {
                this.group_hover(group, move |style| style.text_color(color))
            })
            .when(self.enabled, |this| {
                this.track_focus(&focus)
                    .tab_index(0)
                    .cursor_pointer()
                    .hover(move |style| {
                        if hover_color.is_none() && hover_bg.is_none() {
                            return style.opacity(0.8);
                        }
                        let style = match hover_color {
                            Some(color) => style.text_color(color),
                            None => style,
                        };
                        match hover_bg {
                            Some(bg) => style.bg(bg),
                            None => style,
                        }
                    })
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
                    .when_some(self.on_click, |this, on_click| {
                        let press = on_click.clone();
                        this.key_context(LINK)
                            .on_action(move |_: &PressLink, window, cx| {
                                let event = ClickEvent::Keyboard(KeyboardClickEvent {
                                    button: KeyboardButton::Enter,
                                    bounds: Bounds::default(),
                                });
                                press(&event, window, cx)
                            })
                            .on_click(move |event, window, cx| {
                                cx.stop_propagation();
                                on_click(event, window, cx)
                            })
                    })
            })
            .when(!self.enabled, |this| this.opacity(0.4))
            .child(content)
            .when(focused, |this| {
                this.child(
                    div()
                        .absolute()
                        .inset_0()
                        .rounded(px(3.))
                        .border_1()
                        .border_color(ring),
                )
            });
        // Focused and asked to stay visible: the wrapper measures the link once the layout
        // is known and scrolls its container to it.
        match reveal_in {
            Some(scroll) => div()
                .on_children_prepainted(move |bounds, window, _| {
                    if let Some(link) = bounds.first() {
                        scroll_into_view(&scroll, *link, window);
                    }
                })
                .child(link)
                .into_any_element(),
            None => link.into_any_element(),
        }
    }
}

/// Scrolls `scroll` the least it takes for `child` to be inside it, up or down. GPUI leaves
/// the focus where it is, so Tab could land below a panel's fold.
pub fn scroll_into_view(scroll: &ScrollHandle, child: Bounds<Pixels>, window: &mut Window) {
    let view = scroll.bounds();
    if view.size.height <= px(0.) {
        return;
    }
    let dy = if child.top() < view.top() {
        view.top() - child.top()
    } else if child.bottom() > view.bottom() {
        view.bottom() - child.bottom()
    } else {
        return;
    };
    let offset = scroll.offset();
    scroll.set_offset(point(offset.x, offset.y + dy));
    window.refresh();
}

/// Ends the hover when the pointer leaves the window. GPUI keeps the pointer's last position
/// on a leave (`MouseExitEvent` moves nothing), so the element under it stayed hovered: its
/// hover look and its tooltip stayed until the pointer came back. Painted once per window,
/// this answers a leave with no button held by a move to a point outside the window, which
/// unhovers everything as a real move would. A leave during a press (a drag) is left alone.
pub fn pointer_leave_marker() -> impl IntoElement {
    canvas(
        |_, _, _| {},
        |_, _, window, _| {
            window.on_mouse_event(|event: &MouseExitEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || event.pressed_button.is_some() {
                    return;
                }
                let modifiers = event.modifiers;
                window.defer(cx, move |window, cx| {
                    window.dispatch_event(
                        PlatformInput::MouseMove(MouseMoveEvent {
                            position: point(px(-1.), px(-1.)), // unscaled: outside any window
                            pressed_button: None,
                            modifiers,
                        }),
                        cx,
                    );
                });
            });
        },
    )
    .absolute()
    .size_0()
}
