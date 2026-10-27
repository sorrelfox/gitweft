//! Find in text (Ctrl+F) for the code views: the diff, the file, blame and the conflict tool
//! (#43). A view owns a [`FindBar`]; it gives the bar its rows' shown text, draws the bar's
//! matches as highlights, and scrolls to the current one.

use std::ops::Range;

use gitweft_ui::{AppIcon, LookExt};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Disableable as _, Icon, Sizable as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

actions!(
    find,
    [OpenFind, CloseFind, FindNext, FindPrevious, ToggleFindCase]
);

/// The bar's own key context (the text field in it passes Escape on).
pub(crate) const CONTEXT: &str = "FindBar";

/// Ctrl+F opens the bar in each code view's context (the views bind it); in the bar,
/// Enter / Shift+Enter (the text field's own) go to the next and previous match, F3 and
/// Shift+F3 too, Alt+C toggles matching case, and Escape closes it.
pub fn bind_keys(cx: &mut App) {
    let bar = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("escape", CloseFind, bar),
        KeyBinding::new("f3", FindNext, bar),
        KeyBinding::new("shift-f3", FindPrevious, bar),
        KeyBinding::new("alt-c", ToggleFindCase, bar),
    ]);
}

/// A match: a row of the view and the byte range of its shown text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Match {
    pub row: usize,
    pub range: Range<usize>,
}

/// Every match of `query` in `rows` (each a row index and its shown text), in order and not
/// overlapping. Without `case`, letters match either case (each character compared by its
/// lowercase). An empty query matches nothing.
pub(crate) fn find_matches<'a>(
    rows: impl IntoIterator<Item = (usize, &'a str)>,
    query: &str,
    case: bool,
) -> Vec<Match> {
    let fold = |c: char| {
        if case {
            c
        } else {
            c.to_lowercase().next().unwrap_or(c)
        }
    };
    let wanted: Vec<char> = query.chars().map(fold).collect();
    if wanted.is_empty() {
        return Vec::new();
    }
    let mut matches = Vec::new();
    for (row, text) in rows {
        let chars: Vec<(usize, char)> = text.char_indices().map(|(at, c)| (at, fold(c))).collect();
        let mut ix = 0;
        while ix + wanted.len() <= chars.len() {
            let found = chars[ix..ix + wanted.len()]
                .iter()
                .zip(&wanted)
                .all(|((_, c), w)| c == w);
            if found {
                let end = chars
                    .get(ix + wanted.len())
                    .map_or(text.len(), |(at, _)| *at);
                matches.push(Match {
                    row,
                    range: chars[ix].0..end,
                });
                ix += wanted.len();
            } else {
                ix += 1;
            }
        }
    }
    matches
}

/// What the bar asks of its view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FindEvent {
    /// The query or the case setting changed: find again ([`FindBar::set_matches`]).
    Changed,
    /// Scroll to the current match (it moved).
    Show,
    /// The bar closed: take the keys back.
    Closed,
}

pub(crate) struct FindBar {
    input: Entity<InputState>,
    open: bool,
    case: bool,
    matches: Vec<Match>,
    current: Option<usize>,
    _input_events: Subscription,
}

impl EventEmitter<FindEvent> for FindBar {}

impl FindBar {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx
            .new(|cx| InputState::new(window, cx).placeholder(t!("find.placeholder").into_owned()));
        let events = cx.subscribe(
            &input,
            |this: &mut Self, _, event: &InputEvent, cx| match event {
                InputEvent::Change => cx.emit(FindEvent::Changed),
                InputEvent::PressEnter { shift, .. } => this.step(!*shift, cx),
                _ => {}
            },
        );
        Self {
            input,
            open: false,
            case: false,
            matches: Vec::new(),
            current: None,
            _input_events: events,
        }
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// The query, empty while the bar is closed.
    pub(crate) fn query(&self, cx: &App) -> String {
        if self.open {
            self.input.read(cx).value().to_string()
        } else {
            String::new()
        }
    }

    pub(crate) fn case_sensitive(&self) -> bool {
        self.case
    }

    /// Opens the bar (or takes the keys to it again) with its text selected.
    pub(crate) fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let reopened = !self.open;
        self.open = true;
        self.input.update(cx, |input, cx| {
            input.focus(window, cx);
            input.select_all(window, cx);
        });
        if reopened {
            cx.emit(FindEvent::Changed);
        }
        cx.notify();
    }

    pub(crate) fn close(&mut self, cx: &mut Context<Self>) {
        if self.open {
            self.open = false;
            self.matches.clear();
            self.current = None;
            cx.emit(FindEvent::Closed);
            cx.notify();
        }
    }

    pub(crate) fn toggle_case(&mut self, cx: &mut Context<Self>) {
        self.case = !self.case;
        cx.emit(FindEvent::Changed);
        cx.notify();
    }

    /// The matches found for the query now; the current one is the first at or after row
    /// `from` (where the view is scrolled to), else the first.
    pub(crate) fn set_matches(&mut self, matches: Vec<Match>, from: usize, cx: &mut Context<Self>) {
        self.current =
            (!matches.is_empty()).then(|| matches.iter().position(|m| m.row >= from).unwrap_or(0));
        self.matches = matches;
        if self.current.is_some() {
            cx.emit(FindEvent::Show);
        }
        cx.notify();
    }

    /// The next match (`forward`), else the previous, wrapping around.
    pub(crate) fn step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let count = self.matches.len();
        let Some(current) = self.current else {
            return;
        };
        self.current = Some(if forward {
            (current + 1) % count
        } else {
            (current + count - 1) % count
        });
        cx.emit(FindEvent::Show);
        cx.notify();
    }

    pub(crate) fn current(&self) -> Option<&Match> {
        self.current.and_then(|ix| self.matches.get(ix))
    }

    /// Row `row`'s matches, each with whether it's the current one.
    pub(crate) fn row_matches(
        &self,
        row: usize,
    ) -> impl Iterator<Item = (Range<usize>, bool)> + '_ {
        let start = self.matches.partition_point(|m| m.row < row);
        self.matches[start..]
            .iter()
            .enumerate()
            .take_while(move |(_, m)| m.row == row)
            .map(move |(offset, m)| (m.range.clone(), self.current == Some(start + offset)))
    }

    /// The highlights for row `row`'s matches: `bg` behind each, stronger behind the current.
    pub(crate) fn highlights(&self, row: usize, bg: Hsla) -> Vec<(Range<usize>, HighlightStyle)> {
        self.row_matches(row)
            .map(|(range, current)| {
                let background = if current {
                    Hsla {
                        a: (bg.a * 2.2).min(1.),
                        ..bg
                    }
                } else {
                    bg
                };
                let style = HighlightStyle {
                    background_color: Some(background),
                    ..HighlightStyle::default()
                };
                (range, style)
            })
            .collect()
    }
}

/// A view's find bar, made on the first Ctrl+F (the views are made without a window, and
/// the bar's text field needs one).
#[derive(Default)]
pub(crate) struct FindSlot {
    bar: Option<Entity<FindBar>>,
    _events: Option<Subscription>,
}

impl FindSlot {
    /// The open bar, to draw under the view's header.
    pub(crate) fn element(&self, cx: &App) -> Option<AnyElement> {
        self.bar
            .as_ref()
            .filter(|bar| bar.read(cx).is_open())
            .map(|bar| bar.clone().into_any_element())
    }

    /// Opens the bar (making it the first time), its events going to `on_event`.
    pub(crate) fn open<V: 'static>(
        &mut self,
        window: &mut Window,
        cx: &mut Context<V>,
        on_event: fn(&mut V, &FindEvent, &mut Window, &mut Context<V>),
    ) {
        let bar = match &self.bar {
            Some(bar) => bar.clone(),
            None => {
                let bar = cx.new(|cx| FindBar::new(window, cx));
                self._events = Some(cx.subscribe_in(
                    &bar,
                    window,
                    move |view, _, event, window, cx| on_event(view, event, window, cx),
                ));
                self.bar = Some(bar.clone());
                bar
            }
        };
        bar.update(cx, |bar, cx| bar.open(window, cx));
    }

    /// The query and whether case matters, while the bar is open.
    pub(crate) fn query(&self, cx: &App) -> Option<(String, bool)> {
        let bar = self.bar.as_ref()?.read(cx);
        let query = bar.query(cx);
        (!query.is_empty()).then(|| (query, bar.case_sensitive()))
    }

    /// Hands the bar the matches found, the current one the first at or after row `from`.
    pub(crate) fn set_matches<V>(&self, matches: Vec<Match>, from: usize, cx: &mut Context<V>) {
        if let Some(bar) = &self.bar {
            bar.update(cx, |bar, cx| bar.set_matches(matches, from, cx));
        }
    }

    pub(crate) fn current(&self, cx: &App) -> Option<Match> {
        self.bar.as_ref()?.read(cx).current().cloned()
    }

    /// Row `row`'s match highlights, behind with `bg` (none while the bar is closed).
    pub(crate) fn highlights(
        &self,
        row: usize,
        bg: Hsla,
        cx: &App,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        match &self.bar {
            Some(bar) if bar.read(cx).is_open() => bar.read(cx).highlights(row, bg),
            _ => Vec::new(),
        }
    }
}

impl Render for FindBar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);
        let z = |value: f32| px(value * m.zoom);
        let count = match (self.matches.len(), self.current) {
            (0, _) if self.input.read(cx).value().is_empty() => String::new(),
            (0, _) => t!("find.none").into_owned(),
            (total, Some(current)) => {
                t!("find.count", current = current + 1, total = total).into_owned()
            }
            (total, None) => tn!("find.total", count = total).into_owned(),
        };
        let has = !self.matches.is_empty();
        h_flex()
            .key_context(CONTEXT)
            .on_action(cx.listener(|this, _: &CloseFind, _, cx| this.close(cx)))
            .on_action(cx.listener(|this, _: &FindNext, _, cx| this.step(true, cx)))
            .on_action(cx.listener(|this, _: &FindPrevious, _, cx| this.step(false, cx)))
            .on_action(cx.listener(|this, _: &ToggleFindCase, _, cx| this.toggle_case(cx)))
            .flex_shrink_0()
            .px(z(12.))
            .py(z(6.))
            .gap(z(8.))
            .items_center()
            .border_b_1()
            .border_color(p.line)
            .bg(p.panel)
            .child(
                div().w(z(260.)).child(
                    Input::new(&self.input)
                        .small()
                        .prefix(Icon::new(AppIcon::Search).text_color(p.fg3)),
                ),
            )
            .child(
                div()
                    .min_w(z(72.))
                    .text_size(m.text.small)
                    .text_color(if has || count.is_empty() {
                        p.fg3
                    } else {
                        p.del
                    })
                    .child(count),
            )
            .child(
                Button::new("find-case")
                    .xsmall()
                    .label("Aa")
                    .map(|button| {
                        if self.case {
                            button.primary()
                        } else {
                            button.ghost()
                        }
                    })
                    .tooltip(t!("find.match_case").into_owned())
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_case(cx))),
            )
            .child(
                Button::new("find-previous")
                    .xsmall()
                    .ghost()
                    .label(t!("find.previous").into_owned())
                    .disabled(!has)
                    .on_click(cx.listener(|this, _, _, cx| this.step(false, cx))),
            )
            .child(
                Button::new("find-next")
                    .xsmall()
                    .ghost()
                    .label(t!("find.next").into_owned())
                    .disabled(!has)
                    .on_click(cx.listener(|this, _, _, cx| this.step(true, cx))),
            )
            .child(div().flex_1())
            .child(
                Button::new("find-close")
                    .xsmall()
                    .ghost()
                    .label(t!("find.close").into_owned())
                    .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{Match, find_matches};

    fn found(rows: &[&str], query: &str, case: bool) -> Vec<(usize, &'static str)> {
        let texts: Vec<(usize, &str)> = rows.iter().copied().enumerate().collect();
        find_matches(texts, query, case)
            .into_iter()
            .map(|Match { row, range }| {
                (
                    row,
                    &*Box::leak(rows[row][range].to_owned().into_boxed_str()),
                )
            })
            .collect()
    }

    #[test]
    fn matches_come_in_order_without_overlapping() {
        assert_eq!(
            found(&["let a = foo(foo);", "no", "Foo"], "foo", false),
            [(0, "foo"), (0, "foo"), (2, "Foo")]
        );
        assert_eq!(found(&["aaaa"], "aa", false), [(0, "aa"), (0, "aa")]);
        assert!(found(&["anything"], "", false).is_empty());
    }

    #[test]
    fn case_matters_only_when_asked() {
        assert_eq!(found(&["Foo foo"], "foo", true), [(0, "foo")]);
        assert_eq!(found(&["Foo foo"], "FOO", false).len(), 2);
    }

    #[test]
    fn ranges_are_bytes_of_the_shown_text() {
        // Multi-byte characters before and inside the match.
        let rows = ["漢字 Straße", "ÉTÉ été"];
        let texts: Vec<(usize, &str)> = rows.iter().copied().enumerate().collect();
        let matches = find_matches(texts.clone(), "straße", false);
        assert_eq!(&rows[0][matches[0].range.clone()], "Straße");
        let matches = find_matches(texts, "été", false);
        let words: Vec<&str> = matches
            .iter()
            .map(|m| &rows[m.row][m.range.clone()])
            .collect();
        assert_eq!(words, ["ÉTÉ", "été"]);
    }
}
