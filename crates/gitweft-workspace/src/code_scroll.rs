//! Long lines in the code views (diff, file, blame): scrolled sideways, with the line
//! numbers and the blame column staying put, or wrapped (the `diff.wrap_lines` setting).
//!
//! The sideways scroll is ours, not the list's: an overlay takes wheel and touchpad moves
//! that go more sideways than up or down (Shift+wheel is sideways on Wayland) in the
//! capture phase, before the list would scroll up and down with them, and each row shifts
//! only its text by [`HScroll::offset`]. Wrapped rows have different heights, so that mode
//! draws with gpui's `list` instead of `uniform_list`.

use std::cell::Cell;
use std::rc::Rc;

use gitweft_ui::current::mono_font;
use gitweft_ui::{Metrics, Palette};
use gpui_kit::component::h_flex;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// How far a view's code text is scrolled sideways, and how far it can go.
#[derive(Default)]
pub(crate) struct HScroll {
    offset: Pixels,
    max: Pixels,
    /// The widest text and the part of it that fits, for the thumb.
    content: Pixels,
    visible: Pixels,
    /// The list's width in the last frame (the overlay measures it while painting).
    viewport: Rc<Cell<Pixels>>,
    /// A thumb drag going on: the pointer's x and the offset when it started.
    drag: Option<(Pixels, Pixels)>,
}

impl HScroll {
    pub fn offset(&self) -> Pixels {
        self.offset
    }

    /// The list's width in the last frame (zero before the first).
    pub fn viewport(&self) -> Pixels {
        self.viewport.get()
    }

    /// The widest text and how much of a line's width shows it; the offset stays in range.
    pub fn update(&mut self, content: Pixels, visible: Pixels) {
        self.content = content;
        self.visible = visible.max(Pixels::ZERO);
        self.max = (content - self.visible).max(Pixels::ZERO);
        self.offset = self.offset.min(self.max);
    }

    /// Scrolls by the least that shows the text from `start` to `end` (offsets into the line,
    /// as `text_width` measures them): a found match (#43). Whether the offset changed.
    pub(crate) fn reveal(&mut self, start: Pixels, end: Pixels) -> bool {
        let before = self.offset;
        if start < self.offset {
            self.offset = start;
        } else if end > self.offset + self.visible {
            self.offset = (end - self.visible).min(start);
        }
        self.offset = self.offset.clamp(Pixels::ZERO, self.max);
        self.offset != before
    }

    /// Moves by a wheel's sideways delta (negative: towards the lines' ends). Whether the
    /// offset changed.
    pub(crate) fn scroll_by(&mut self, delta: Pixels) -> bool {
        let offset = (self.offset - delta).clamp(Pixels::ZERO, self.max);
        let moved = offset != self.offset;
        self.offset = offset;
        moved
    }

    /// The thumb was pressed with the pointer at `x`.
    fn start_drag(&mut self, x: Pixels) {
        self.drag = Some((x, self.offset));
    }

    /// Follows a thumb drag to the pointer's `x`; `ratio` is how far the text moves per pixel
    /// the thumb moves. Whether the offset changed.
    fn drag_to(&mut self, x: Pixels, ratio: f32) -> bool {
        let Some((start_x, start_offset)) = self.drag else {
            return false;
        };
        let offset = (start_offset + (x - start_x) * ratio).clamp(Pixels::ZERO, self.max);
        let moved = offset != self.offset;
        self.offset = offset;
        moved
    }

    /// The thumb was let go. Whether a drag was going on.
    fn end_drag(&mut self) -> bool {
        self.drag.take().is_some()
    }
}

/// The Tab width setting: columns between tab stops in the code views (#80).
pub(crate) fn tab_width(cx: &App) -> usize {
    gitweft_ui::SettingsStore::settings(cx).diff.tab_width()
}

/// The advance of one character of the code font at `size` (every character of a
/// monospace font has it).
pub(crate) fn char_width(window: &Window, size: Pixels) -> Pixels {
    let text_system = window.text_system();
    let id = text_system.resolve_font(&font(mono_font()));
    text_system.em_advance(id, size).unwrap_or(size * 0.6)
}

/// The width `columns` columns of the code font take, plus a little room after them. Count
/// them with `gitweft_git::display::columns`: a wide character takes two (#42).
pub(crate) fn text_width(window: &Window, size: Pixels, columns: usize) -> Pixels {
    char_width(window, size) * (columns as f32 + 2.)
}

/// A line-number column for numbers up to `largest` in the code font at `size`, with
/// `padding` beside them: at least `minimum` wide (the design's width, which fits four
/// digits), wider for longer numbers (#42). Half a character more than the digits: with
/// exactly their width, rounding wraps the last digit onto a second line.
pub(crate) fn number_width(
    window: &Window,
    size: Pixels,
    largest: usize,
    padding: Pixels,
    minimum: Pixels,
) -> Pixels {
    let digits = largest.max(1).ilog10() as f32 + 1.5;
    (char_width(window, size) * digits + padding).max(minimum)
}

/// The kit's `pr_2()` (half a rem), which the number columns' width has to count.
pub(crate) fn pr_2(m: &Metrics) -> Pixels {
    m.rem * 0.5
}

/// Laid over a code list (in a `relative` container): measures the list, takes sideways
/// scrolling, and draws where the text is scrolled to while lines are cut: a thumb that can
/// be dragged.
pub(crate) fn overlay<V: 'static>(
    view: WeakEntity<V>,
    access: fn(&mut V) -> &mut HScroll,
    scroll: &HScroll,
    p: &Palette,
    m: &Metrics,
) -> AnyElement {
    // The thumb: where it is, how wide, and how far the text moves per pixel it moves.
    let track = scroll.viewport.get();
    let geometry = (scroll.max > Pixels::ZERO && scroll.content > Pixels::ZERO).then(|| {
        let width = (track * (scroll.visible / scroll.content)).max(m.dp(24.));
        let room = track - width;
        let left = room * (scroll.offset / scroll.max);
        let ratio = if room > Pixels::ZERO {
            scroll.max / room
        } else {
            0.
        };
        (left, width, ratio)
    });
    let ratio = geometry.map_or(0., |(_, _, ratio)| ratio);
    let dragging = scroll.drag.is_some();

    let viewport = scroll.viewport.clone();
    let wheel_view = view.clone();
    let listener = canvas(
        move |bounds, window, _| {
            // This frame's range and thumb came from the width measured last time: when it
            // changed (the view's first frame, a resize), draw once more with the new one. A
            // view that opens without any input (a diff following a file staged outside the
            // app) kept a stray thumb otherwise. `refresh` does nothing while drawing.
            if viewport.replace(bounds.size.width) != bounds.size.width {
                window.request_animation_frame();
            }
            bounds
        },
        move |_, bounds, window, _| {
            if dragging {
                // Anywhere in the window: the pointer leaves the thumb while dragging.
                let (moves, ups) = (wheel_view.clone(), wheel_view.clone());
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                    if phase != DispatchPhase::Capture {
                        return;
                    }
                    moves
                        .update(cx, |this, cx| {
                            if access(this).drag_to(event.position.x, ratio) {
                                cx.notify();
                            }
                        })
                        .ok();
                });
                window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
                    if phase != DispatchPhase::Capture {
                        return;
                    }
                    ups.update(cx, |this, cx| {
                        if access(this).end_drag() {
                            cx.notify();
                        }
                    })
                    .ok();
                });
            }
            let view = wheel_view;
            window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
                if phase != DispatchPhase::Capture || !bounds.contains(&event.position) {
                    return;
                }
                let delta = event.delta.pixel_delta(window.line_height());
                let sideways = if event.modifiers.shift && delta.x.is_zero() {
                    delta.y
                } else if delta.x.abs() > delta.y.abs() {
                    delta.x
                } else {
                    return;
                };
                view.update(cx, |this, cx| {
                    if access(this).scroll_by(sideways) {
                        cx.notify();
                    }
                })
                .ok();
                // The list would scroll up and down with it otherwise.
                cx.stop_propagation();
            });
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full();

    let (idle, active) = (p.fg3.opacity(0.5), p.fg2.opacity(0.8));
    let thumb = geometry.map(|(left, width, _)| {
        // A taller area than the 4 px bar takes the press.
        div()
            .id("code-hscroll-thumb")
            .absolute()
            .bottom_0()
            .left(left)
            .w(width)
            .h(m.dp(12.))
            .flex()
            .items_end()
            .pb(m.dp(2.))
            .group("code-hscroll-thumb")
            .on_mouse_down(MouseButton::Left, move |event, _, cx| {
                view.update(cx, |this, cx| {
                    access(this).start_drag(event.position.x);
                    cx.notify();
                })
                .ok();
                cx.stop_propagation();
            })
            .child(
                div()
                    .w_full()
                    .h(m.dp(4.))
                    .rounded(m.dp(2.))
                    .bg(if dragging { active } else { idle })
                    .group_hover("code-hscroll-thumb", move |style| style.bg(active)),
            )
    });
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .child(listener)
        .children(thumb)
        .into_any_element()
}

/// The first row shown by a `uniform_list` of `row_height` rows, from its scroll offset. (Its
/// handle's `logical_scroll_top` counts only the rows laid out, so it says 0 or thereabouts
/// wherever the list is scrolled to.)
pub(crate) fn uniform_top(handle: &UniformListScrollHandle, row_height: Pixels) -> usize {
    if row_height <= Pixels::ZERO {
        return 0;
    }
    let offset = handle.0.borrow().base_handle.offset();
    ((-offset.y) / row_height).floor().max(0.) as usize
}

/// Carries the first row shown across a switch between the plain list (of `row_height`
/// rows) and the wrapped one.
pub(crate) fn switch_mode(
    wrap: bool,
    uniform: &UniformListScrollHandle,
    list: &ListState,
    count: usize,
    row_height: Pixels,
) {
    if wrap {
        let top = uniform_top(uniform, row_height);
        list.reset(count);
        list.scroll_to(ListOffset {
            item_ix: top.min(count.saturating_sub(1)),
            offset_in_item: Pixels::ZERO,
        });
    } else {
        let top = list.logical_scroll_top().item_ix;
        uniform.scroll_to_item_strict(top, ScrollStrategy::Top);
    }
}

/// Shows row `ix` at the top of whichever list is drawn.
pub(crate) fn scroll_to_top(
    wrap: bool,
    uniform: &UniformListScrollHandle,
    list: &ListState,
    ix: usize,
) {
    if wrap {
        list.scroll_to(ListOffset {
            item_ix: ix,
            offset_in_item: Pixels::ZERO,
        });
    } else {
        uniform.scroll_to_item_strict(ix, ScrollStrategy::Top);
    }
}

/// A new list state for rows drawn wrapped.
pub(crate) fn list_state(count: usize) -> ListState {
    ListState::new(count, ListAlignment::Top, px(400.)) // unscaled: overdraw
}

/// The Wrap toggle of a code view's header: flips the setting for every code view.
pub(crate) fn wrap_button(wrap: bool) -> impl IntoElement {
    use gpui_kit::component::button::{Button, ButtonVariants};
    use gpui_kit::component::{Selectable, Sizable};
    Button::new("wrap-lines")
        .small()
        .ghost()
        .selected(wrap)
        .label(rust_i18n::t!("code.wrap").into_owned())
        .on_click(move |_, _, cx| {
            gitweft_ui::SettingsStore::update(cx, |settings| settings.diff.wrap_lines = !wrap);
        })
}

/// Whether code views wrap long lines.
pub(crate) fn wrapping(cx: &App) -> bool {
    gitweft_ui::SettingsStore::settings(cx).diff.wrap_lines
}

/// A code row's text: shifted by the sideways scroll and cut at the row's end, or wrapped.
pub(crate) fn code_text(text: impl IntoElement, wrap: bool, offset: Pixels) -> Div {
    if wrap {
        div().flex_1().min_w_0().child(text)
    } else {
        div()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .child(div().ml(-offset).whitespace_nowrap().child(text))
    }
}

/// A code row: one line high, or (wrapped) as high as its text, the cells stretched so
/// numbers sit on the first line and bars run the whole height.
pub(crate) fn code_row(wrap: bool, line_height: Pixels) -> Div {
    h_flex()
        .min_w_0()
        .line_height(line_height)
        .when(wrap, |row| row.items_stretch().min_h(line_height))
        .when(!wrap, |row| row.h(line_height).overflow_hidden())
}

#[cfg(test)]
mod tests {
    // Not `super::*`: gpui's glob brings its own `test` attribute.
    use super::HScroll;
    use gpui_kit::{Pixels, px};

    #[test]
    fn the_offset_stays_in_range() {
        let mut scroll = HScroll::default();
        scroll.update(px(1000.), px(400.));
        // A wheel towards the lines' ends has a negative delta.
        assert!(scroll.scroll_by(px(-250.)));
        assert_eq!(scroll.offset(), px(250.));
        assert!(scroll.scroll_by(px(-1000.)));
        assert_eq!(scroll.offset(), px(600.));
        assert!(!scroll.scroll_by(px(-10.)), "already at the end");
        assert!(scroll.scroll_by(px(5000.)));
        assert_eq!(scroll.offset(), Pixels::ZERO);
        // Shorter text (or a wider view) pulls the offset back.
        scroll.scroll_by(px(-600.));
        scroll.update(px(500.), px(400.));
        assert_eq!(scroll.offset(), px(100.));
        scroll.update(px(300.), px(400.));
        assert_eq!(scroll.offset(), Pixels::ZERO);
        assert!(!scroll.scroll_by(px(-50.)), "everything fits");
    }

    #[test]
    fn the_top_row_comes_from_the_offset() {
        use gpui_kit::{UniformListScrollHandle, point};
        let handle = UniformListScrollHandle::new();
        assert_eq!(super::uniform_top(&handle, px(20.)), 0);
        // 205 px down a list of 20 px rows: row 10 is at the top (half of it cut off).
        handle
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(0.), px(-205.)));
        assert_eq!(super::uniform_top(&handle, px(20.)), 10);
        assert_eq!(super::uniform_top(&handle, px(18.)), 11);
        assert_eq!(super::uniform_top(&handle, Pixels::ZERO), 0);
    }

    #[test]
    fn a_thumb_drag_moves_the_text_in_proportion() {
        let mut scroll = HScroll::default();
        scroll.update(px(1000.), px(400.));
        // No drag going on: nothing moves.
        assert!(!scroll.drag_to(px(500.), 2.5));
        // 2.5 text pixels per thumb pixel: 40 px right moves the text 100 px.
        scroll.start_drag(px(100.));
        assert!(scroll.drag_to(px(140.), 2.5));
        assert_eq!(scroll.offset(), px(100.));
        // Far past the end: at the end; back past the start: at the start.
        scroll.drag_to(px(2000.), 2.5);
        assert_eq!(scroll.offset(), px(600.));
        scroll.drag_to(px(-500.), 2.5);
        assert_eq!(scroll.offset(), Pixels::ZERO);
        assert!(scroll.end_drag());
        assert!(!scroll.end_drag());
        assert!(!scroll.drag_to(px(300.), 2.5));
    }
}
