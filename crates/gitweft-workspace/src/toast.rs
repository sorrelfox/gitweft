//! Toasts of our own (#38): gpui-kit 0.6.1's notification stack draws only the newest
//! one, so a second operation's result hid the first's. Here every toast stays visible,
//! stacked in the bottom-left corner (newest at the bottom), each with its own close
//! button; they go after a while, which hovering the stack pauses. The Activity panel
//! keeps every git command anyway.

use std::time::Duration;

use gitweft_ui::widgets::LinkButton;
use gitweft_ui::{AppIcon, LookExt};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

/// How a toast is colored, and how long it stays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Warning,
    Error,
}

impl ToastKind {
    /// Long enough to read; a warning or an error longer.
    fn lifetime(self) -> Duration {
        match self {
            ToastKind::Info | ToastKind::Success => Duration::from_secs(6),
            ToastKind::Warning => Duration::from_secs(10),
            ToastKind::Error => Duration::from_secs(15),
        }
    }
}

/// One toast: built like gpui-kit's `Notification` (`Toast::error(text).title(title)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub kind: ToastKind,
    pub title: Option<String>,
    pub message: String,
}

impl Toast {
    fn new(kind: ToastKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            title: None,
            message: message.into(),
        }
    }

    pub fn info(message: impl Into<String>) -> Self {
        Self::new(ToastKind::Info, message)
    }

    pub fn success(message: impl Into<String>) -> Self {
        Self::new(ToastKind::Success, message)
    }

    pub fn warning(message: impl Into<String>) -> Self {
        Self::new(ToastKind::Warning, message)
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::new(ToastKind::Error, message)
    }

    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }
}

/// At most this many are shown; a new one pushes the oldest out.
const MAX_SHOWN: usize = 5;
/// How often the timers count down.
const TICK: Duration = Duration::from_millis(250);

struct Shown {
    id: u64,
    toast: Toast,
    /// Time left, counted down while the stack isn't hovered.
    left: Duration,
}

/// The toasts on screen: the model, whose timing is plain code ([`Toasts::tick`]).
#[derive(Default)]
pub struct Toasts {
    shown: Vec<Shown>,
    next_id: u64,
    hovered: bool,
    ticking: bool,
}

impl Toasts {
    /// Adds `toast` at the bottom; the oldest goes when there are too many.
    pub fn push(&mut self, toast: Toast) {
        let left = toast.kind.lifetime();
        self.shown.push(Shown {
            id: self.next_id,
            toast,
            left,
        });
        self.next_id += 1;
        if self.shown.len() > MAX_SHOWN {
            self.shown.remove(0);
        }
    }

    /// `elapsed` has passed: the timers count down, unless the stack is hovered, and
    /// toasts whose time is up go. Whether anything changed.
    pub fn tick(&mut self, elapsed: Duration) -> bool {
        if self.hovered || self.shown.is_empty() {
            return false;
        }
        for shown in &mut self.shown {
            shown.left = shown.left.saturating_sub(elapsed);
        }
        let before = self.shown.len();
        self.shown.retain(|shown| !shown.left.is_zero());
        before != self.shown.len()
    }

    pub fn close(&mut self, id: u64) {
        self.shown.retain(|shown| shown.id != id);
    }

    pub fn set_hovered(&mut self, hovered: bool) {
        self.hovered = hovered;
    }

    /// The toasts shown, oldest first.
    #[cfg(test)]
    pub fn shown(&self) -> impl Iterator<Item = &Toast> {
        self.shown.iter().map(|shown| &shown.toast)
    }
}

/// The app's toasts (one stack: the main window shows it).
struct ToastLayer(Entity<Toasts>);

impl Global for ToastLayer {}

fn layer(cx: &mut App) -> Entity<Toasts> {
    if let Some(layer) = cx.try_global::<ToastLayer>() {
        return layer.0.clone();
    }
    let toasts = cx.new(|_| Toasts::default());
    cx.set_global(ToastLayer(toasts.clone()));
    toasts
}

/// Shows `toast`.
pub fn push(toast: Toast, cx: &mut App) {
    let toasts = layer(cx);
    toasts.update(cx, |toasts, cx| {
        toasts.push(toast);
        cx.notify();
        if !toasts.ticking {
            toasts.ticking = true;
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(TICK).await;
                    let more = this
                        .update(cx, |toasts, cx| {
                            if toasts.tick(TICK) {
                                cx.notify();
                            }
                            toasts.ticking = !toasts.shown.is_empty();
                            toasts.ticking
                        })
                        .unwrap_or(false);
                    if !more {
                        break;
                    }
                }
            })
            .detach();
        }
    });
}

/// The stack, for the shell to draw above everything else.
pub fn render_layer(cx: &mut App) -> Entity<Toasts> {
    layer(cx)
}

impl Render for Toasts {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (p, m) = (*cx.palette(), *cx.metrics());
        let z = |value: f32| px(value * m.zoom);
        let cards = self.shown.iter().enumerate().map(|(ix, shown)| {
            let id = shown.id;
            let accent = match shown.toast.kind {
                ToastKind::Info => p.acc,
                ToastKind::Success => p.add,
                ToastKind::Warning => p.modified,
                ToastKind::Error => p.del,
            };
            h_flex()
                .id(("toast", id))
                .debug_selector(move || format!("toast-{ix}"))
                .w(z(360.))
                .items_start()
                .gap(z(8.))
                .p(z(10.))
                .bg(p.panel)
                .border_1()
                .border_color(p.line)
                .border_l(z(3.))
                .border_color(accent)
                .rounded(m.radius)
                .shadow_md()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(z(2.))
                        .when_some(shown.toast.title.clone(), |this, title| {
                            this.child(
                                div()
                                    .text_color(p.fg)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(title),
                            )
                        })
                        .child(
                            gitweft_ui::flow::flow(
                                ("toast-message", id),
                                shown.toast.message.clone(),
                            )
                            .text_color(p.fg2),
                        ),
                )
                .child(
                    LinkButton::icon(("toast-close", id), AppIcon::Close, p.fg3, z(20.))
                        .tooltip(t!("toast.close").into_owned())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.close(id);
                            cx.notify();
                        })),
                )
        });
        div().absolute().left(z(12.)).bottom(z(36.)).child(
            v_flex()
                .id("toasts")
                .gap(z(8.))
                .text_size(m.text.small)
                // Hovering keeps them all while they're read.
                .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                    this.set_hovered(*hovered);
                    cx.notify();
                }))
                .occlude()
                .children(cards),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Rust's own `#[test]`: `gpui_kit::*` brings GPUI's `test` macro.
    use std::prelude::v1::test;

    #[test]
    fn every_toast_is_kept_until_its_time_is_up() {
        let mut toasts = Toasts::default();
        toasts.push(Toast::success("pushed"));
        toasts.push(Toast::error("fetch failed").title("Fetch"));
        toasts.push(Toast::info("copied"));
        assert_eq!(toasts.shown().count(), 3, "all shown, not only the newest");

        // Hovered: nothing counts down.
        toasts.set_hovered(true);
        assert!(!toasts.tick(Duration::from_secs(60)));
        assert_eq!(toasts.shown().count(), 3);
        toasts.set_hovered(false);

        // 6 s: the info and the success go, the error stays (15 s).
        assert!(toasts.tick(Duration::from_secs(6)));
        let left: Vec<_> = toasts.shown().map(|t| t.message.as_str()).collect();
        assert_eq!(left, ["fetch failed"]);
        assert!(toasts.tick(Duration::from_secs(9)));
        assert_eq!(toasts.shown().count(), 0);
    }

    #[test]
    fn the_oldest_goes_when_too_many_and_one_can_be_closed() {
        let mut toasts = Toasts::default();
        for n in 0..7 {
            toasts.push(Toast::info(format!("{n}")));
        }
        let shown: Vec<_> = toasts.shown().map(|t| t.message.clone()).collect();
        assert_eq!(shown, ["2", "3", "4", "5", "6"]);
        toasts.close(4);
        let shown: Vec<_> = toasts.shown().map(|t| t.message.clone()).collect();
        assert_eq!(shown, ["2", "3", "5", "6"]);
    }
}
