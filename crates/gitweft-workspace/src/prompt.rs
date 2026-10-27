//! Confirmation prompts (`window.prompt`) on the Graphite palette. GPUI has no system
//! prompt on Linux, and its fallback is a fixed-width white box that clips longer text.

use gitweft_ui::LookExt;
use gitweft_ui::current::ui_font;
use gitweft_ui::flow::flow;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// The prompt builder for [`App::set_prompt_builder`].
pub(crate) fn build(
    level: PromptLevel,
    message: &str,
    detail: Option<&str>,
    actions: &[PromptButton],
    handle: PromptHandle,
    window: &mut Window,
    cx: &mut App,
) -> RenderablePromptHandle {
    let prompt = cx.new(|cx| ThemedPrompt {
        level,
        message: message.to_owned(),
        detail: detail.map(str::to_owned),
        actions: actions.to_vec(),
        focus: cx.focus_handle(),
    });
    handle.with_view(prompt, window, cx)
}

struct ThemedPrompt {
    level: PromptLevel,
    message: String,
    detail: Option<String>,
    actions: Vec<PromptButton>,
    focus: FocusHandle,
}

impl ThemedPrompt {
    fn key_choice(&self, key: &str) -> Option<usize> {
        key_choice(self.level, &self.actions, key)
    }
}

/// Escape picks Cancel. Enter picks the first button that isn't Cancel, except on a
/// Warning or Critical prompt (ours ask before something is thrown away): there Enter
/// picks Cancel as well, so a stray Enter never discards, deletes or resets.
fn key_choice(level: PromptLevel, actions: &[PromptButton], key: &str) -> Option<usize> {
    let cancel = actions.iter().position(PromptButton::is_cancel);
    match key {
        "enter" if matches!(level, PromptLevel::Warning | PromptLevel::Critical) => cancel,
        "enter" => actions.iter().position(|action| !action.is_cancel()),
        "escape" => cancel,
        _ => None,
    }
}

impl EventEmitter<PromptResponse> for ThemedPrompt {}

impl Focusable for ThemedPrompt {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ThemedPrompt {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);
        let z = |value: f32| px(value * m.zoom);
        // Our Warning prompts ask before something is thrown away (discard, delete, drop).
        let destructive = matches!(self.level, PromptLevel::Warning | PromptLevel::Critical);
        // Cancel on the left, the confirming button on the right, as in the app's dialogs.
        let mut order: Vec<usize> = (0..self.actions.len()).collect();
        order.sort_by_key(|&ix| !self.actions[ix].is_cancel());
        let buttons: Vec<Button> = order
            .into_iter()
            .map(|ix| {
                let action = &self.actions[ix];
                Button::new(("prompt-action", ix))
                    .label(action.label().clone())
                    .when(!action.is_cancel(), |button| {
                        if destructive {
                            button.danger()
                        } else {
                            button.primary()
                        }
                    })
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.stop_propagation();
                        cx.emit(PromptResponse(ix));
                    }))
            })
            .collect();

        // Drawn by the window above everything else, so it sets its own font and colors: a
        // panel card on the scrim, like the kit's dialogs and menus.
        // The scrim takes the mouse: nothing behind the prompt is clicked, hovered or
        // scrolled while it waits for an answer.
        div()
            .debug_selector(|| "prompt".into())
            .occlude()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(p.scrim)
            .font_family(ui_font())
            .text_size(m.text.ui)
            .text_color(p.fg)
            .child(
                v_flex()
                    .track_focus(&self.focus)
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        if let Some(ix) = this.key_choice(&event.keystroke.key) {
                            cx.stop_propagation();
                            cx.emit(PromptResponse(ix));
                        }
                    }))
                    .w(z(440.))
                    .max_w(relative(0.9))
                    .p(z(16.))
                    .gap(z(12.))
                    .rounded(m.radius_lg)
                    .border_1()
                    .border_color(p.line)
                    .bg(p.panel)
                    .shadow_lg()
                    .child(
                        flow("prompt-message", self.message.clone())
                            .font_weight(FontWeight::SEMIBOLD),
                    )
                    .when_some(self.detail.clone(), |this, detail| {
                        this.child(
                            flow("prompt-detail", detail)
                                .text_size(m.text.small)
                                .text_color(p.fg2),
                        )
                    })
                    .child(
                        h_flex()
                            .justify_end()
                            .gap(z(8.))
                            .pt(z(4.))
                            .children(buttons),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::key_choice;
    use gpui_kit::{PromptButton, PromptLevel};

    #[test]
    fn enter_confirms_an_info_prompt_but_cancels_a_warning() {
        let actions = [
            PromptButton::ok("Delete".to_owned()),
            PromptButton::cancel("Cancel".to_owned()),
        ];
        assert_eq!(key_choice(PromptLevel::Info, &actions, "enter"), Some(0));
        assert_eq!(key_choice(PromptLevel::Warning, &actions, "enter"), Some(1));
        assert_eq!(
            key_choice(PromptLevel::Critical, &actions, "enter"),
            Some(1)
        );
        assert_eq!(
            key_choice(PromptLevel::Warning, &actions, "escape"),
            Some(1)
        );
        assert_eq!(key_choice(PromptLevel::Info, &actions, "a"), None);
    }
}
