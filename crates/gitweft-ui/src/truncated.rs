//! Text cut to its width with "…" whose whole text shows on hover. Only a text that
//! was actually cut gets the tooltip: the last frame measured it (the text's natural width
//! in the text style it is drawn with, against the room it had).

use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;

use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// One line of text, cut with "…" when it doesn't fit; hovered, a cut one shows its whole
/// text (or [`Truncated::tooltip`]'s). `id` is unique among its siblings.
#[derive(IntoElement)]
pub struct Truncated {
    id: ElementId,
    text: SharedString,
    tooltip: Option<SharedString>,
    highlights: Vec<(Range<usize>, HighlightStyle)>,
    style: StyleRefinement,
}

pub fn truncated(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Truncated {
    Truncated {
        id: id.into(),
        text: text.into(),
        tooltip: None,
        highlights: Vec::new(),
        style: StyleRefinement::default(),
    }
}

impl Truncated {
    /// Another text for the tooltip when it's cut (a ref's full name, say).
    pub fn tooltip(mut self, text: impl Into<SharedString>) -> Self {
        self.tooltip = Some(text.into());
        self
    }

    /// Parts of the text drawn otherwise (a search's matches), as `StyledText` takes them:
    /// sorted byte ranges that don't overlap.
    pub fn highlights(mut self, highlights: Vec<(Range<usize>, HighlightStyle)>) -> Self {
        self.highlights = highlights;
        self
    }
}

impl Styled for Truncated {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

/// Whether `text` at its natural width (its highlighted parts in their style) is wider than
/// `room`, in the text style that applies here.
fn is_cut(
    text: &SharedString,
    highlights: &[(Range<usize>, HighlightStyle)],
    room: Pixels,
    window: &Window,
) -> bool {
    let style = window.text_style();
    let size = style.font_size.to_pixels(window.rem_size());
    // One line: the text is drawn on one line whatever newlines it has (a newline and a
    // space are one byte each, so the ranges still hold).
    let line: SharedString = if text.contains('\n') {
        text.replace('\n', " ").into()
    } else {
        text.clone()
    };
    let mut runs = Vec::new();
    let mut at = 0;
    for (range, highlight) in highlights {
        if range.start > at {
            runs.push(style.to_run(range.start - at));
        }
        runs.push(style.clone().highlight(*highlight).to_run(range.len()));
        at = range.end;
    }
    if at < line.len() {
        runs.push(style.to_run(line.len() - at));
    }
    let shaped = window.text_system().shape_line(line, size, &runs, None);
    // Half a pixel of slack: layout rounding isn't a cut.
    shaped.width > room + px(0.5)
}

impl RenderOnce for Truncated {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let cut = window
            .use_keyed_state(self.id.clone(), cx, |_, _| Rc::new(Cell::new(false)))
            .read(cx)
            .clone();
        let measured = cut.clone();
        let text = self.text.clone();
        let highlights = self.highlights.clone();
        // How the headless UI tests find it, and whether it was cut (a no-op outside them).
        let selector = {
            let (text, cut) = (self.text.clone(), cut.get());
            move || format!("{}:{text}", if cut { "cut" } else { "whole" })
        };
        let tip = self.tooltip.unwrap_or_else(|| self.text.clone());
        // The caller's styles (padding, width, colors) go on the outer element; the inner
        // one holds only the text, so its width is the text's room.
        let mut element = div()
            .id(self.id)
            .debug_selector(selector)
            .min_w_0()
            .overflow_hidden()
            .child(
                div()
                    .relative()
                    .truncate()
                    .map(|this| {
                        if self.highlights.is_empty() {
                            this.child(self.text)
                        } else {
                            this.child(StyledText::new(self.text).with_highlights(self.highlights))
                        }
                    })
                    .child(
                        canvas(
                            move |bounds, window, _| {
                                let now = is_cut(&text, &highlights, bounds.size.width, window);
                                // A change shows (or drops) the tooltip from the next frame.
                                if measured.replace(now) != now {
                                    window.request_animation_frame();
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full(),
                    ),
            )
            .when(cut.get(), |this| {
                this.tooltip(move |window, cx| {
                    let tip = tip.clone();
                    Tooltip::element(move |_, _| {
                        let shown = tip.clone();
                        div()
                            .debug_selector(move || format!("tooltip:{shown}"))
                            .child(tip.clone())
                    })
                    .build(window, cx)
                })
            });
        element.style().refine(&self.style);
        element
    }
}
