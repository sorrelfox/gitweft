//! Text that wraps at its spaces only. GPUI's wrapper also breaks before `+`, `/` and
//! `(`, which split "(Ctrl+Z)" and "(~/.gitconfig)" across lines, and no glue character stops
//! it. This one picks the breaks itself: the last frame measured each word in the text style
//! it is drawn with, against the width it had, and the text is drawn with a newline at each
//! break. Lines it makes fit, so GPUI breaks none of them again; a word wider than a whole
//! line (a long path) is left on its line for GPUI to wrap inside it.

use std::cell::RefCell;
use std::rc::Rc;

use gpui_kit::*;

/// Wrapped text whose lines break at spaces only. `id` is unique among its siblings.
#[derive(IntoElement)]
pub struct Flow {
    id: ElementId,
    text: SharedString,
    style: StyleRefinement,
}

pub fn flow(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Flow {
    Flow {
        id: id.into(),
        text: text.into(),
        style: StyleRefinement::default(),
    }
}

impl Styled for Flow {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

/// The width last measured, and the text with its breaks for it.
type Measured = Rc<RefCell<Option<(Pixels, String)>>>;

impl RenderOnce for Flow {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let measured: Measured = window
            .use_keyed_state(self.id.clone(), cx, |_, _| Rc::new(RefCell::new(None)))
            .read(cx)
            .clone();
        let shown = match &*measured.borrow() {
            Some((_, broken)) => SharedString::from(broken.clone()),
            None => self.text.clone(),
        };
        let text = self.text.clone();
        // How the headless UI tests see where the lines broke (a no-op outside them).
        let selector = {
            let shown = shown.clone();
            move || format!("flow:{shown}")
        };
        let mut element = div().id(self.id).debug_selector(selector).child(
            div().relative().whitespace_normal().child(shown).child(
                canvas(
                    move |bounds, window, _| {
                        let room = bounds.size.width;
                        if measured.borrow().as_ref().map(|(at, _)| *at) == Some(room) {
                            return;
                        }
                        let broken = break_text(&text, room, &word_width(window));
                        let changed = measured.borrow().as_ref().map(|(_, b)| b) != Some(&broken);
                        *measured.borrow_mut() = Some((room, broken));
                        // The new breaks show from the next frame.
                        if changed {
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
        );
        element.style().refine(&self.style);
        element
    }
}

/// How wide a piece of text is in the text style that applies here.
fn word_width(window: &Window) -> impl Fn(&str) -> Pixels + '_ {
    let style = window.text_style();
    let size = style.font_size.to_pixels(window.rem_size());
    move |word: &str| {
        let line = SharedString::from(word.to_owned());
        let runs = [style.to_run(word.len())];
        window
            .text_system()
            .shape_line(line, size, &runs, None)
            .width
    }
}

/// `text` with a newline where each line ends in `room`, breaking at spaces only: each word
/// goes on the line it fits, else starts the next one. A word wider than `room` stays where
/// it falls, for the wrapper to break inside.
fn break_text(text: &str, room: Pixels, width: &dyn Fn(&str) -> Pixels) -> String {
    // Half a pixel of slack: layout rounding mustn't make GPUI break a line again.
    let room = room - px(0.5); // unscaled: rounding slack, not a size
    let space = width(" ");
    let mut out = String::with_capacity(text.len());
    for (ix, paragraph) in text.split('\n').enumerate() {
        if ix > 0 {
            out.push('\n');
        }
        let mut used: Option<Pixels> = None;
        for word in paragraph.split(' ').filter(|word| !word.is_empty()) {
            let w = width(word);
            used = Some(match used {
                None => {
                    out.push_str(word);
                    w
                }
                Some(used) if used + space + w <= room || w > room => {
                    out.push(' ');
                    out.push_str(word);
                    used + space + w
                }
                Some(_) => {
                    out.push('\n');
                    out.push_str(word);
                    w
                }
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::prelude::v1::test;

    /// Every character 10 px wide.
    fn width(text: &str) -> Pixels {
        px(10. * text.chars().count() as f32)
    }

    #[test]
    fn lines_break_at_spaces_only() {
        // 15 characters to a line: "(Ctrl+Z)" goes down whole.
        assert_eq!(
            break_text("Undo it (Ctrl+Z) now", px(150.), &width),
            "Undo it\n(Ctrl+Z) now"
        );
        // A line that fits stays (with the half pixel of slack); one pixel short, it breaks.
        assert_eq!(break_text("aaaa bbbb", px(91.), &width), "aaaa bbbb");
        assert_eq!(break_text("aaaa bbbb", px(90.), &width), "aaaa\nbbbb");
    }

    #[test]
    fn paragraphs_and_blank_lines_stay() {
        assert_eq!(
            break_text("one\n\ntwo three", px(150.), &width),
            "one\n\ntwo three"
        );
    }

    #[test]
    fn a_word_wider_than_the_line_stays_where_it_falls() {
        // The path follows "because" on its line; the wrapper breaks inside it.
        assert_eq!(
            break_text(
                "because /tmp/a-very-long/path/index.lock exists",
                px(150.),
                &width
            ),
            "because /tmp/a-very-long/path/index.lock\nexists"
        );
    }
}
