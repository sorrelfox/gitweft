//! The settings window: one per app (opening it again brings it forward). Every field reads
//! and writes `SettingsStore`, so the main window follows each change live and the file is
//! saved shortly after.

use std::rc::Rc;

use gitweft_core::external::{Environment, default_terminal};
use gitweft_core::settings::{
    AppSettings, ColorPreset, DateFormat, Density, DiffLayout, GraphSettings, MAX_SAVED_COLORS,
    MONO_FONT_SIZE, ThemeChoice, ToolSettings, UI_FONT_SIZE,
};
use gitweft_ui::fonts::{BUNDLED, MONO_FONT, UI_FONT};
use gitweft_ui::palette::to_hex;
use gitweft_ui::{AppIcon, ColorToken, LookExt, Metrics, Palette, SettingsStore};
use gpui_kit::component::Disableable as _;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::{Root, TitleBar, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::keymap::{self, Bindable, KeymapStore};
use crate::settings_ui::{
    NumberFieldOptions, SettingField, SettingGroup, SettingItem, SettingPage, SettingsNav,
};

actions!(settings, [OpenSettings]);

/// The open settings window, to bring it forward instead of opening a second one.
struct SettingsWindowHandle(WindowHandle<Root>);

impl Global for SettingsWindowHandle {}

/// Whether `window` is the settings window.
pub(crate) fn is_settings_window(window: &Window, cx: &App) -> bool {
    cx.try_global::<SettingsWindowHandle>()
        .is_some_and(|open| open.0.window_id() == window.window_handle().window_id())
}

/// Types `text` into the settings window's search (the headless UI tests).
#[cfg(test)]
pub(crate) fn search_for(text: &str, window: &mut Window, cx: &mut App) {
    let root = window
        .root::<Root>()
        .flatten()
        .expect("the settings window");
    let view = root
        .read(cx)
        .view()
        .clone()
        .downcast::<SettingsWindow>()
        .ok();
    let search = view.expect("its view").read(cx).nav.search.clone();
    search.update(cx, |search, cx| {
        search.set_value(text.to_owned(), window, cx)
    });
}

/// Takes `handle`'s window for the settings window, as far as recording keys goes (tests).
#[cfg(test)]
pub(crate) fn pretend_settings_window(handle: WindowHandle<Root>, cx: &mut App) {
    cx.set_global(SettingsWindowHandle(handle));
}

pub(crate) fn bind_keys(cx: &mut App) {
    // No key context: it works in any window, whatever has the focus.
    cx.bind_keys([KeyBinding::new("ctrl-,", OpenSettings, None)]);
    cx.on_action(|_: &OpenSettings, cx| open(cx));
}

/// Opens the settings window, or brings it forward when it's open already.
pub(crate) fn open(cx: &mut App) {
    if let Some(handle) = cx.try_global::<SettingsWindowHandle>().map(|open| open.0)
        && cx
            .windows()
            .iter()
            .any(|window| window.window_id() == handle.window_id())
    {
        // Deferred: Ctrl+, pressed in the settings window itself dispatches from inside
        // that window, which can't be updated until the dispatch is over (updating it
        // right away failed, and a second window opened).
        cx.defer(move |cx| {
            handle
                .update(cx, |_, window, _| window.activate_window())
                .ok();
        });
        return;
    }
    let m = *cx.metrics();
    let options = WindowOptions {
        // Opens large enough for its zoomed content; the minimum is a window's.
        window_bounds: Some(WindowBounds::centered(size(m.dp(820.), m.dp(600.)), cx)),
        window_min_size: Some(size(px(560.), px(420.))), // unscaled: a window's
        app_id: Some("io.github.sorrelfox.Gitweft".into()),
        window_decorations: Some(WindowDecorations::Client),
        titlebar: Some(TitlebarOptions {
            title: Some(t!("settings.window_title").into_owned().into()),
            ..TitleBar::title_bar_options()
        }),
        ..TitleBar::window_options()
    };
    let opened = cx.open_window(options, |window, cx| {
        // A key recording left waiting ends with the window: the next key pressed elsewhere
        // must do what it's bound to, not become a binding (#11).
        window.on_window_should_close(cx, |_, cx| {
            KeymapStore::record(cx, None);
            true
        });
        let view = cx.new(|cx| SettingsWindow::new(window, cx));
        cx.new(|cx| Root::new(view, window, cx))
    });
    match opened {
        Ok(handle) => cx.set_global(SettingsWindowHandle(handle)),
        Err(err) => eprintln!("gitweft: couldn't open the settings window: {err}"),
    }
}

struct SettingsWindow {
    /// Which palette the Colors page edits (the one in use when the window opened).
    editing_dark: bool,
    /// One picker per color token, showing the palette being edited.
    pickers: Vec<(ColorToken, Entity<ColorPickerState>)>,
    /// The pickers must show another palette (set without a window at hand).
    resync: bool,
    /// The terminal used when none is set, looked for on a thread when the window opens
    /// (Tools page; in a Flatpak that asks the host): `None` until the answer is in.
    detected_terminal: Option<Option<String>>,
    /// The name to save the colors in use under (Colors page).
    save_name: Entity<InputState>,
    /// The page shown, and the search (#88, #89).
    nav: SettingsNav,
    _subscriptions: Vec<Subscription>,
}

impl SettingsWindow {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editing_dark = cx.palette().dark;
        let palette = edited_palette(editing_dark, cx);
        let mut subscriptions = Vec::new();
        let pickers = ColorToken::ALL
            .iter()
            .map(|&token| {
                let color = token.get(&palette);
                let state = cx.new(|cx| ColorPickerState::new(window, cx).default_value(color));
                subscriptions.push(cx.subscribe_in(
                    &state,
                    window,
                    move |this, _, event: &ColorPickerEvent, _, cx| {
                        let ColorPickerEvent::Change(color) = event;
                        let (dark, color) = (this.editing_dark, *color);
                        SettingsStore::update(cx, |settings| {
                            let colors = settings.appearance.colors.for_mode_mut(dark);
                            match color {
                                Some(color) => colors.insert(token.key().into(), to_hex(color)),
                                None => colors.remove(token.key()),
                            };
                        });
                    },
                ));
                (token, state)
            })
            .collect();
        // Changes made here or anywhere else (the sun/moon, a reset): redraw and keep
        // the pickers in step.
        subscriptions.push(
            cx.observe_global_in::<SettingsStore>(window, |this, window, cx| {
                this.sync_pickers(window, cx);
                cx.notify();
            }),
        );
        // The Keyboard page follows recording and every change of keys.
        subscriptions.push(cx.observe_global_in::<KeymapStore>(window, |_, _, cx| cx.notify()));
        let save_name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("settings.save_colors_placeholder").into_owned())
        });
        // Enter saves; typing redraws the button (Save or Replace).
        subscriptions.push(cx.subscribe_in(
            &save_name,
            window,
            |_, state, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => save_named_colors(state, window, cx),
                InputEvent::Change => cx.notify(),
                _ => {}
            },
        ));
        let (found_tx, found) = async_channel::bounded(1);
        let started = std::thread::Builder::new()
            .name("gitweft-terminal".into())
            .spawn(move || {
                let _ = found_tx.send_blocking(default_terminal(&Environment::current()));
            });
        if let Err(err) = started {
            eprintln!("gitweft: couldn't start a thread: {err}");
        }
        cx.spawn(async move |this, cx| {
            let found = found.recv().await.ok().flatten();
            this.update(cx, |this, cx| {
                this.detected_terminal = Some(found);
                cx.notify();
            })
            .ok();
        })
        .detach();
        let nav = SettingsNav::new(window, cx);
        // Typing a search redraws the findings.
        subscriptions.push(cx.subscribe(&nav.search, |_, _, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                cx.notify();
            }
        }));
        Self {
            editing_dark,
            pickers,
            resync: false,
            detected_terminal: None,
            save_name,
            nav,
            _subscriptions: subscriptions,
        }
    }

    /// Shows the edited palette in the pickers. Compared as hex, so a picker isn't reset
    /// under a drag by rounding.
    fn sync_pickers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let palette = edited_palette(self.editing_dark, cx);
        for (token, state) in &self.pickers {
            let color = token.get(&palette);
            state.update(cx, |state, cx| {
                if state.value().map(to_hex) != Some(to_hex(color)) {
                    state.set_value(color, window, cx);
                }
            });
        }
    }

    fn pages(&self, cx: &mut Context<Self>) -> Vec<SettingPage> {
        vec![
            general_page(),
            // Theme, Text and Colors under it, each a page of its own (#90).
            appearance_page(cx).child(self.colors_page(cx)),
            graph_page(),
            diff_page(),
            tools_page(self.detected_terminal.as_ref().map(Option::as_deref)),
            keyboard_page(),
        ]
    }

    fn colors_page(&self, cx: &mut Context<Self>) -> SettingPage {
        let view = cx.entity().downgrade();
        let read_view = view.clone();
        let editing = SettingItem::new(
            t!("settings.colors_editing").into_owned(),
            SettingField::dropdown(
                vec![
                    ("dark".into(), t!("settings.theme_dark").into_owned().into()),
                    (
                        "light".into(),
                        t!("settings.theme_light").into_owned().into(),
                    ),
                ],
                move |cx| {
                    let dark = read_view
                        .upgrade()
                        .is_none_or(|view| view.read(cx).editing_dark);
                    if dark { "dark" } else { "light" }.into()
                },
                move |value, cx| {
                    if let Some(view) = view.upgrade() {
                        view.update(cx, |this, cx| {
                            this.editing_dark = value.as_ref() == "dark";
                            this.resync = true;
                            cx.notify();
                        });
                    }
                },
            ),
        )
        .description(t!("settings.colors_editing_hint").into_owned());
        let dark = self.editing_dark;
        let preview =
            SettingItem::render(move |_, cx| preview(edited_palette(dark, cx), *cx.metrics()));
        let preset = SettingItem::new(
            t!("settings.color_preset").into_owned(),
            choice(
                vec![
                    (
                        ColorPreset::Graphite,
                        "graphite",
                        t!("settings.preset_graphite").into_owned(),
                    ),
                    (
                        ColorPreset::HighContrast,
                        "high_contrast",
                        t!("settings.preset_high_contrast").into_owned(),
                    ),
                ],
                |settings| settings.appearance.preset,
                |settings, value| settings.appearance.preset = value,
                ColorPreset::Graphite,
            ),
        )
        .description(t!("settings.color_preset_hint").into_owned());
        // The preset and the preview, then a page for the saved colors and one per group
        // of colors (#89: each shows its own).
        let mut page = SettingPage::new(t!("settings.colors").into_owned())
            .group(SettingGroup::new().item(preset).item(editing).item(preview))
            .child(
                SettingPage::new(t!("settings.saved_colors").into_owned())
                    .group(self.saved_colors_group(cx)),
            );
        for (title, tokens) in COLOR_GROUPS {
            let items = tokens.iter().filter_map(|token| {
                let (_, state) = self.pickers.iter().find(|(t, _)| t == token)?;
                Some(self.color_item(*token, state.clone()))
            });
            page = page.child(
                SettingPage::new(group_title(title)).group(SettingGroup::new().items(items)),
            );
        }
        page
    }

    /// A name field to save the colors in use, then one row per saved name: its colors in
    /// the palette being edited, Use and Delete.
    fn saved_colors_group(&self, cx: &App) -> SettingGroup {
        let input = self.save_name.clone();
        let save = SettingItem::new(
            t!("settings.save_colors").into_owned(),
            SettingField::render(move |_, cx| save_colors_field(&input, cx)),
        )
        .description(t!("settings.save_colors_hint").into_owned());
        let dark = self.editing_dark;
        let rows: Vec<SettingItem> = SettingsStore::settings(cx)
            .appearance
            .saved_colors
            .keys()
            .map(|name| {
                let shown = name.clone();
                SettingItem::new(
                    name.clone(),
                    SettingField::render(move |_, cx| saved_colors_field(&shown, dark, cx)),
                )
            })
            .collect();
        SettingGroup::new()
            .description(t!("settings.saved_colors_hint").into_owned())
            .item(save)
            .items(rows)
    }

    fn color_item(&self, token: ColorToken, state: Entity<ColorPickerState>) -> SettingItem {
        let dark = self.editing_dark;
        let key = token.key();
        let field = SettingField::render(move |_, _| ColorPicker::new(&state)).on_reset(
            move |cx| {
                SettingsStore::settings(cx)
                    .appearance
                    .colors
                    .for_mode(dark)
                    .contains_key(key)
            },
            move |_, cx| {
                SettingsStore::update(cx, |settings| {
                    settings.appearance.colors.for_mode_mut(dark).remove(key);
                });
            },
        );
        SettingItem::new(color_label(token), field).keywords([key])
    }
}

impl Render for SettingsWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.resync) {
            self.sync_pickers(window, cx);
        }
        let look = cx.look().clone();
        let m = look.metrics;
        let pages = Rc::new(self.pages(cx));
        let view = cx.entity().downgrade();
        let shown = pages.clone();
        let on_select: crate::settings_ui::OnSelect = Rc::new(move |path, _, cx| {
            view.update(cx, |this, cx| {
                this.nav.select(&shown, &path);
                cx.notify();
            })
            .ok();
        });
        let content = crate::settings_ui::SettingsLayout {
            pages,
            nav: self.nav.clone(),
            sidebar_width: m.dp(220.),
            on_select,
        };
        v_flex()
            .size_full()
            .bg(look.palette.panel)
            .text_color(look.palette.fg)
            .font_family(look.fonts.ui.clone())
            .text_size(look.metrics.text.ui)
            .child(
                TitleBar::new().child(
                    div()
                        .text_color(look.palette.fg2)
                        .child(t!("settings.window_title").into_owned()),
                ),
            )
            .child(div().flex_1().min_h_0().child(content))
            .child(gitweft_ui::widgets::pointer_leave_marker())
    }
}

/// Every command whose keys can change, in groups: Record takes the next keys pressed,
/// Remove leaves the command without keys, and the item's reset brings its defaults back.
fn keyboard_page() -> SettingPage {
    let commands = Rc::new(keymap::bindable());
    let mut page = SettingPage::new(t!("settings.keyboard").into_owned())
        .icon(AppIcon::Settings)
        .description(t!("settings.keyboard_hint").into_owned());
    for group in keymap::GROUPS {
        let items: Vec<SettingItem> = commands
            .iter()
            .enumerate()
            .filter(|(_, command)| command.group == group)
            .map(|(ix, command)| {
                let name = command.name();
                let all = commands.clone();
                let field = SettingField::render(move |_, cx| key_field(&all, ix, cx)).on_reset(
                    move |cx| cx.global::<KeymapStore>().is_changed(name),
                    move |_, cx| KeymapStore::set(cx, name, None),
                );
                SettingItem::new(command.label.clone(), field).keywords([name])
            })
            .collect();
        page = page.group(
            SettingGroup::new()
                .title(t!(group).into_owned())
                .items(items),
        );
    }
    page
}

/// A command's keys as key caps (or "Press the new keys…" while recording), a note when
/// another command has the same keys, then Record (Cancel) and Remove.
fn key_field(commands: &[Bindable], ix: usize, cx: &App) -> AnyElement {
    let command = &commands[ix];
    let name = command.name();
    let (p, m) = (*cx.palette(), *cx.metrics());
    let store = cx.global::<KeymapStore>();
    let recording = store.recording() == Some(name);
    let keys = store.keys(name);
    let conflicts = store.conflicts(command, commands);
    let pending = store.pending();
    let shown = if recording && pending.is_empty() {
        div()
            .text_color(p.acc)
            .child(t!("keyboard.press_keys").into_owned())
            .into_any_element()
    } else if recording {
        // The chords so far; a pause ends the sequence.
        h_flex()
            .gap_1()
            .children(
                pending
                    .iter()
                    .filter_map(|stroke| Keystroke::parse(stroke).ok())
                    .map(Kbd::new),
            )
            .child(div().text_color(p.acc).child("…"))
            .into_any_element()
    } else if keys.is_empty() {
        div()
            .text_color(p.fg3)
            .child(t!("keyboard.unbound").into_owned())
            .into_any_element()
    } else {
        h_flex()
            .gap_2()
            .children(keys.iter().map(|key| {
                h_flex().gap_1().children(
                    key.split_whitespace()
                        .filter_map(|stroke| Keystroke::parse(stroke).ok())
                        .map(Kbd::new),
                )
            }))
            .into_any_element()
    };
    h_flex()
        .gap_2()
        .items_center()
        .when(!conflicts.is_empty() && !recording, |row| {
            row.child(
                div().text_size(m.text.xs).text_color(p.modified).child(
                    t!(
                        "keyboard.conflict",
                        names = crate::util::join_list(&conflicts)
                    )
                    .into_owned(),
                ),
            )
        })
        .child(shown)
        .child(
            Button::new(SharedString::from(format!("record-{name}")))
                .small()
                .outline()
                .label(if recording {
                    t!("keyboard.cancel").into_owned()
                } else {
                    t!("keyboard.record").into_owned()
                })
                .on_click(move |_, _, cx| {
                    KeymapStore::record(cx, if recording { None } else { Some(name) })
                }),
        )
        .when(!keys.is_empty() && !recording, |row| {
            row.child(
                Button::new(SharedString::from(format!("remove-{name}")))
                    .small()
                    .ghost()
                    .label(t!("keyboard.remove").into_owned())
                    .on_click(move |_, _, cx| KeymapStore::set(cx, name, Some(Vec::new()))),
            )
        })
        .into_any_element()
}

/// The name field and Save (Replace when that name holds colors already; disabled
/// without a name, or for a new name once as many as allowed are saved).
fn save_colors_field(input: &Entity<InputState>, cx: &App) -> AnyElement {
    let m = cx.metrics();
    let name = input.read(cx).value().trim().to_owned();
    let saved = &SettingsStore::settings(cx).appearance.saved_colors;
    let replaces = saved.contains_key(&name);
    let full = !replaces && saved.len() >= MAX_SAVED_COLORS;
    let state = input.clone();
    h_flex()
        .gap_2()
        .items_center()
        .child(div().w(m.dp(180.)).child(Input::new(input).small()))
        .child(
            Button::new("save-colors")
                .small()
                .outline()
                .label(if replaces {
                    t!("settings.save_colors_replace").into_owned()
                } else {
                    t!("settings.save_colors_button").into_owned()
                })
                .disabled(name.is_empty() || full)
                .on_click(move |_, window, cx| save_named_colors(&state, window, cx)),
        )
        .into_any_element()
}

/// Saves the colors in use under the typed name and empties the field.
fn save_named_colors(input: &Entity<InputState>, window: &mut Window, cx: &mut App) {
    let name = input.read(cx).value().trim().to_owned();
    let mut saved = false;
    SettingsStore::update(cx, |settings| {
        saved = settings.appearance.save_colors(&name)
    });
    if saved {
        input.update(cx, |state, cx| state.set_value("", window, cx));
    }
}

/// Saved colors: a strip of them (backgrounds, text, accent, the first lanes) as the
/// edited palette would look, Use ("In use" when they are) and Delete.
fn saved_colors_field(name: &str, dark: bool, cx: &App) -> AnyElement {
    let m = cx.metrics();
    let appearance = &SettingsStore::settings(cx).appearance;
    let Some(saved) = appearance.saved_colors.get(name) else {
        return div().into_any_element();
    };
    let p = Palette::resolve_preset(saved.preset, dark, saved.colors.for_mode(dark));
    let in_use = appearance.saved_colors_in_use() == Some(name);
    let colors = [
        p.bg, p.panel, p.panel2, p.fg, p.fg2, p.acc, p.lanes[0], p.lanes[1], p.lanes[2],
        p.lanes[3], p.add, p.del,
    ];
    let strip = h_flex()
        .rounded(m.dp(4.))
        .border_1()
        .border_color(cx.palette().line)
        .overflow_hidden()
        .children(colors.map(|color| div().w(m.dp(12.)).h(m.dp(18.)).bg(color)));
    let (used, deleted) = (name.to_owned(), name.to_owned());
    h_flex()
        .gap_2()
        .items_center()
        .child(strip)
        .child(
            Button::new(SharedString::from(format!("use-colors-{name}")))
                .small()
                .outline()
                .label(if in_use {
                    t!("settings.saved_colors_in_use").into_owned()
                } else {
                    t!("settings.saved_colors_use").into_owned()
                })
                .disabled(in_use)
                .on_click(move |_, window, cx| use_saved_colors(used.clone(), window, cx)),
        )
        .child(
            Button::new(SharedString::from(format!("delete-colors-{name}")))
                .small()
                .ghost()
                .label(t!("settings.saved_colors_delete").into_owned())
                .on_click(move |_, window, cx| delete_saved_colors(deleted.clone(), window, cx)),
        )
        .into_any_element()
}

/// Puts saved colors in use; asks first when that would replace changed colors that
/// aren't saved under any name.
fn use_saved_colors(name: String, window: &mut Window, cx: &mut App) {
    let apply = |name: &str, cx: &mut App| {
        SettingsStore::update(cx, |settings| {
            settings.appearance.use_saved_colors(name);
        });
    };
    if !SettingsStore::settings(cx).appearance.has_unsaved_colors() {
        apply(&name, cx);
        return;
    }
    let answer = window.prompt(
        PromptLevel::Warning,
        &t!("settings.saved_colors_replace_title", name = name),
        Some(&t!("settings.saved_colors_replace_text")),
        &[
            PromptButton::ok(t!("settings.saved_colors_use_anyway").into_owned()),
            PromptButton::cancel(t!("toolbar.cancel").into_owned()),
        ],
        cx,
    );
    cx.spawn(async move |cx| {
        if answer.await == Ok(0) {
            cx.update(|cx| apply(&name, cx));
        }
    })
    .detach();
}

/// Deletes saved colors after asking; the colors in use stay.
fn delete_saved_colors(name: String, window: &mut Window, cx: &mut App) {
    let answer = window.prompt(
        PromptLevel::Warning,
        &t!("settings.saved_colors_delete_title", name = name),
        Some(&t!("settings.saved_colors_delete_text")),
        &[
            PromptButton::ok(t!("settings.saved_colors_delete").into_owned()),
            PromptButton::cancel(t!("toolbar.cancel").into_owned()),
        ],
        cx,
    );
    cx.spawn(async move |cx| {
        if answer.await == Ok(0) {
            cx.update(|cx| {
                SettingsStore::update(cx, |settings| {
                    settings.appearance.saved_colors.remove(&name);
                });
            });
        }
    })
    .detach();
}

/// The palette the Colors page shows: Graphite with the user's colors for that mode.
fn edited_palette(dark: bool, cx: &App) -> Palette {
    let appearance = &SettingsStore::settings(cx).appearance;
    Palette::resolve_preset(appearance.preset, dark, appearance.colors.for_mode(dark))
}

/// A dropdown over one of the settings' enums: each value with its key and label.
fn choice<T: Copy + PartialEq + 'static>(
    options: Vec<(T, &'static str, String)>,
    get: fn(&AppSettings) -> T,
    set: fn(&mut AppSettings, T),
    default: T,
) -> SettingField<SharedString> {
    let items = options
        .iter()
        .map(|(_, key, label)| ((*key).into(), label.clone().into()))
        .collect();
    let keys: Rc<Vec<(T, &'static str)>> = Rc::new(
        options
            .iter()
            .map(|(value, key, _)| (*value, *key))
            .collect(),
    );
    let default_key = keys
        .iter()
        .find(|(value, _)| *value == default)
        .map_or("", |(_, key)| *key);
    let read = keys.clone();
    SettingField::dropdown(
        items,
        move |cx| {
            let current = get(SettingsStore::settings(cx));
            read.iter()
                .find(|(value, _)| *value == current)
                .map(|(_, key)| SharedString::from(*key))
                .unwrap_or_default()
        },
        move |key, cx| {
            if let Some(&(value, _)) = keys.iter().find(|(_, k)| *k == key.as_ref()) {
                SettingsStore::update(cx, |settings| set(settings, value));
            }
        },
    )
    .default_value(default_key)
}

fn general_page() -> SettingPage {
    let date_format = choice(
        vec![
            (
                DateFormat::Full,
                "full",
                t!("settings.date_full").into_owned(),
            ),
            (
                DateFormat::Short,
                "short",
                t!("settings.date_short").into_owned(),
            ),
            (
                DateFormat::Relative,
                "relative",
                t!("settings.date_relative").into_owned(),
            ),
        ],
        |settings| settings.general.date_format,
        |settings, value| settings.general.date_format = value,
        DateFormat::Full,
    );
    let restore = SettingField::switch(
        |cx| SettingsStore::settings(cx).general.restore_session,
        |value, cx| SettingsStore::update(cx, |settings| settings.general.restore_session = value),
    )
    .default_value(true);
    // The trash's limits (#19): 0 is off, the default.
    let trash_days = SettingField::number_input(
        NumberFieldOptions {
            min: 0.0,
            max: 3650.0,
            step: 1.0,
        },
        |cx| f64::from(SettingsStore::settings(cx).general.trash_max_age_days),
        |value, cx| {
            SettingsStore::update(cx, |settings| {
                settings.general.trash_max_age_days = value.round().clamp(0.0, 3650.0) as u32;
            });
        },
    )
    .default_value(0.0);
    let trash_mb = SettingField::number_input(
        NumberFieldOptions {
            min: 0.0,
            max: 1_000_000.0,
            step: 100.0,
        },
        |cx| f64::from(SettingsStore::settings(cx).general.trash_max_size_mb),
        |value, cx| {
            SettingsStore::update(cx, |settings| {
                settings.general.trash_max_size_mb = value.round().clamp(0.0, 1_000_000.0) as u32;
            });
        },
    )
    .default_value(0.0);
    SettingPage::new(t!("settings.general").into_owned())
        .icon(AppIcon::Settings)
        .group(
            SettingGroup::new()
                .item(
                    SettingItem::new(t!("settings.date_format").into_owned(), date_format)
                        .description(t!("settings.date_format_hint").into_owned()),
                )
                .item(
                    SettingItem::new(t!("settings.restore_session").into_owned(), restore)
                        .description(t!("settings.restore_session_hint").into_owned()),
                )
                .item(
                    SettingItem::new(t!("settings.trash_max_age").into_owned(), trash_days)
                        .description(t!("settings.trash_max_age_hint").into_owned()),
                )
                .item(
                    SettingItem::new(t!("settings.trash_max_size").into_owned(), trash_mb)
                        .description(t!("settings.trash_max_size_hint").into_owned()),
                ),
        )
}

fn appearance_page(cx: &App) -> SettingPage {
    let theme = choice(
        vec![
            (
                ThemeChoice::System,
                "system",
                t!("settings.theme_system").into_owned(),
            ),
            (
                ThemeChoice::Light,
                "light",
                t!("settings.theme_light").into_owned(),
            ),
            (
                ThemeChoice::Dark,
                "dark",
                t!("settings.theme_dark").into_owned(),
            ),
        ],
        |settings| settings.appearance.theme,
        |settings, value| settings.appearance.theme = value,
        ThemeChoice::System,
    );
    let density = choice(
        vec![
            (
                Density::Comfortable,
                "comfortable",
                t!("settings.density_comfortable").into_owned(),
            ),
            (
                Density::Compact,
                "compact",
                t!("settings.density_compact").into_owned(),
            ),
        ],
        |settings| settings.appearance.density,
        |settings, value| settings.appearance.density = value,
        Density::Comfortable,
    );
    // A zoom set in settings.json that isn't one of these is listed too, rather than
    // leaving the choice empty (#44).
    let mut zooms: Vec<(f32, &'static str)> = [0.8f32, 0.9, 1.0, 1.1, 1.25, 1.5]
        .into_iter()
        .zip(["0.8", "0.9", "1.0", "1.1", "1.25", "1.5"])
        .collect();
    let current = SettingsStore::settings(cx).appearance.zoom;
    if !zooms.iter().any(|(value, _)| *value == current) {
        zooms.push((current, "current"));
        zooms.sort_by(|a, b| a.0.total_cmp(&b.0));
    }
    let zoom = choice(
        zooms
            .into_iter()
            .map(|(value, key)| {
                let percent = (value * 100.0).round();
                (
                    value,
                    key,
                    t!("settings.zoom_percent", percent = percent).into_owned(),
                )
            })
            .collect(),
        |settings| settings.appearance.zoom,
        |settings, value| settings.appearance.zoom = value,
        1.0,
    );
    let ui_size = SettingField::number_input(
        NumberFieldOptions {
            min: UI_FONT_SIZE.0.into(),
            max: UI_FONT_SIZE.2.into(),
            step: 1.0,
        },
        |cx| SettingsStore::settings(cx).appearance.ui_font_size.into(),
        |value, cx| {
            SettingsStore::update(cx, |settings| {
                settings.appearance.ui_font_size = value as f32;
            });
        },
    )
    .default_value(f64::from(UI_FONT_SIZE.1));
    let mono_size = SettingField::number_input(
        NumberFieldOptions {
            min: MONO_FONT_SIZE.0.into(),
            max: MONO_FONT_SIZE.2.into(),
            step: 1.0,
        },
        |cx| SettingsStore::settings(cx).appearance.mono_font_size.into(),
        |value, cx| {
            SettingsStore::update(cx, |settings| {
                settings.appearance.mono_font_size = value as f32;
            });
        },
    )
    .default_value(f64::from(MONO_FONT_SIZE.1));

    // Theme and Text each a page of their own under Appearance (#88), and Colors after
    // them (`SettingsWindow::pages`).
    let theme_page = SettingPage::new(t!("settings.theme_group").into_owned()).group(
        SettingGroup::new()
            .item(
                SettingItem::new(t!("settings.theme").into_owned(), theme)
                    .description(t!("settings.theme_hint").into_owned()),
            )
            .item(
                SettingItem::new(t!("settings.density").into_owned(), density)
                    .description(t!("settings.density_hint").into_owned()),
            )
            .item(
                SettingItem::new(t!("settings.zoom").into_owned(), zoom)
                    .description(t!("settings.zoom_hint").into_owned()),
            ),
    );
    let text_page = SettingPage::new(t!("settings.text_group").into_owned()).group(
        SettingGroup::new()
            .item(SettingItem::new(
                t!("settings.ui_font").into_owned(),
                font_field(false, cx),
            ))
            .item(SettingItem::new(
                t!("settings.ui_font_size").into_owned(),
                ui_size,
            ))
            .item(
                SettingItem::new(t!("settings.mono_font").into_owned(), font_field(true, cx))
                    .description(t!("settings.mono_font_hint").into_owned()),
            )
            .item(SettingItem::new(
                t!("settings.mono_font_size").into_owned(),
                mono_size,
            )),
    );
    SettingPage::new(t!("settings.appearance").into_owned())
        .icon(AppIcon::Sun)
        .child(theme_page)
        .child(text_page)
}

/// The bundled families of that kind first, then every installed family.
fn font_field(mono: bool, cx: &App) -> SettingField<SharedString> {
    let mut names: Vec<SharedString> = BUNDLED
        .iter()
        .filter(|family| family.monospace == mono)
        .map(|family| family.name.into())
        .collect();
    let mut installed: Vec<&String> = SettingsStore::installed_fonts(cx)
        .iter()
        // Names starting with a dot are GPUI's aliases, not families.
        .filter(|name| !name.starts_with('.'))
        .filter(|name| !BUNDLED.iter().any(|family| family.name == name.as_str()))
        .collect();
    installed.sort();
    installed.dedup();
    names.extend(installed.into_iter().map(|name| name.clone().into()));
    let options = names
        .iter()
        .map(|name| (name.clone(), name.clone()))
        .collect();
    SettingField::scrollable_dropdown(
        options,
        move |cx| {
            let appearance = &SettingsStore::settings(cx).appearance;
            if mono {
                appearance.mono_font.clone().into()
            } else {
                appearance.ui_font.clone().into()
            }
        },
        move |name, cx| {
            SettingsStore::update(cx, |settings| {
                if mono {
                    settings.appearance.mono_font = name.to_string();
                } else {
                    settings.appearance.ui_font = name.to_string();
                }
            });
        },
    )
    .default_value(if mono { MONO_FONT } else { UI_FONT })
}

/// The graph's columns: which are shown, and a reset for the widths dragged in the graph.
fn graph_page() -> SettingPage {
    use gpui_kit::component::button::Button;
    use gpui_kit::component::{Disableable as _, Sizable as _};

    let column = |get: fn(&GraphSettings) -> bool, set: fn(&mut GraphSettings, bool)| {
        SettingField::switch(
            move |cx| get(&SettingsStore::settings(cx).graph),
            move |value, cx| SettingsStore::update(cx, |settings| set(&mut settings.graph, value)),
        )
        .default_value(true)
    };
    let widths = SettingField::render(|_, cx| {
        let graph = &SettingsStore::settings(cx).graph;
        let dragged = graph.refs_width.is_some()
            || graph.author_width.is_some()
            || graph.date_width.is_some();
        Button::new("reset-graph-widths")
            .small()
            .outline()
            .label(t!("settings.graph_widths_reset").into_owned())
            .disabled(!dragged)
            .on_click(|_, _, cx| {
                SettingsStore::update(cx, |settings| {
                    let graph = &mut settings.graph;
                    (graph.refs_width, graph.author_width, graph.date_width) = (None, None, None);
                });
            })
    });
    SettingPage::new(t!("settings.graph").into_owned())
        .icon(AppIcon::Branch)
        .group(
            SettingGroup::new()
                .title(t!("settings.graph_columns").into_owned())
                .description(t!("settings.graph_columns_hint").into_owned())
                .item(SettingItem::new(
                    t!("settings.graph_show_author").into_owned(),
                    column(
                        |graph| graph.show_author,
                        |graph, on| graph.show_author = on,
                    ),
                ))
                .item(SettingItem::new(
                    t!("settings.graph_show_date").into_owned(),
                    column(|graph| graph.show_date, |graph, on| graph.show_date = on),
                ))
                .item(SettingItem::new(
                    t!("settings.graph_show_sha").into_owned(),
                    column(|graph| graph.show_sha, |graph, on| graph.show_sha = on),
                ))
                .item(
                    SettingItem::new(t!("settings.graph_widths").into_owned(), widths)
                        .description(t!("settings.graph_widths_hint").into_owned()),
                ),
        )
}

/// The user's other programs (`gitweft_core::external`), and the diff and merge tools git
/// runs, by name. `detected_terminal`: what an empty Terminal starts (`None` while it's being
/// looked for).
fn tools_page(detected_terminal: Option<Option<&str>>) -> SettingPage {
    let text = |get: fn(&ToolSettings) -> &String, set: fn(&mut ToolSettings, String)| {
        SettingField::input(
            move |cx| SharedString::from(get(&SettingsStore::settings(cx).tools).clone()),
            move |value, cx| {
                SettingsStore::update(cx, |settings| set(&mut settings.tools, value.to_string()))
            },
        )
        .default_value("")
    };
    let found = match detected_terminal {
        None => t!("settings.terminal_looking").into_owned(),
        Some(None) => t!("settings.terminal_none").into_owned(),
        Some(Some(name)) => name.to_owned(),
    };
    // A command line needs the width: the field goes under its label, full width (a
    // side-by-side field is 240 px and ran past the window).
    let item = |label: &str, hint: String, field| {
        SettingItem::new(t!(label).into_owned(), field)
            .description(hint)
            .vertical()
    };
    SettingPage::new(t!("settings.tools").into_owned())
        .icon(AppIcon::Folder)
        .group(
            SettingGroup::new()
                .title(t!("settings.tools_programs").into_owned())
                .description(t!("settings.tools_programs_hint").into_owned())
                .item(item(
                    "settings.terminal",
                    t!("settings.terminal_hint", found = found).into_owned(),
                    text(
                        |tools| &tools.terminal,
                        |tools, value| tools.terminal = value,
                    ),
                ))
                .item(item(
                    "settings.file_manager",
                    t!("settings.file_manager_hint").into_owned(),
                    text(
                        |tools| &tools.file_manager,
                        |tools, value| tools.file_manager = value,
                    ),
                ))
                .item(item(
                    "settings.editor",
                    t!("settings.editor_hint").into_owned(),
                    text(|tools| &tools.editor, |tools, value| tools.editor = value),
                )),
        )
        .group(
            SettingGroup::new()
                .title(t!("settings.git_tools").into_owned())
                .description(t!("settings.git_tools_hint").into_owned())
                .item(item(
                    "settings.diff_tool",
                    t!("settings.diff_tool_hint").into_owned(),
                    text(
                        |tools| &tools.diff_tool,
                        |tools, value| tools.diff_tool = value,
                    ),
                ))
                .item(item(
                    "settings.merge_tool",
                    t!("settings.merge_tool_hint").into_owned(),
                    text(
                        |tools| &tools.merge_tool,
                        |tools, value| tools.merge_tool = value,
                    ),
                )),
        )
}

fn diff_page() -> SettingPage {
    let layout = choice(
        vec![
            (
                DiffLayout::Unified,
                "unified",
                t!("settings.diff_unified").into_owned(),
            ),
            (
                DiffLayout::Split,
                "split",
                t!("settings.diff_split").into_owned(),
            ),
        ],
        |settings| settings.diff.layout,
        |settings, value| settings.diff.layout = value,
        DiffLayout::Unified,
    );
    let whitespace = SettingField::switch(
        |cx| SettingsStore::settings(cx).diff.ignore_whitespace,
        |value, cx| {
            SettingsStore::update(cx, |settings| settings.diff.ignore_whitespace = value);
        },
    )
    .default_value(false);
    let wrap = SettingField::switch(
        |cx| SettingsStore::settings(cx).diff.wrap_lines,
        |value, cx| {
            SettingsStore::update(cx, |settings| settings.diff.wrap_lines = value);
        },
    )
    .default_value(false);
    // Columns between tab stops in the code views (#80).
    let (tab_min, tab_default, tab_max) = gitweft_core::settings::TAB_WIDTH;
    let tab_width = SettingField::number_input(
        NumberFieldOptions {
            min: tab_min.into(),
            max: tab_max.into(),
            step: 1.0,
        },
        |cx| SettingsStore::settings(cx).diff.tab_width() as f64,
        move |value, cx| {
            SettingsStore::update(cx, |settings| {
                let (min, max) = (f64::from(tab_min), f64::from(tab_max));
                settings.diff.tab_width = Some(value.round().clamp(min, max) as u8);
            });
        },
    )
    .default_value(f64::from(tab_default));
    // Stored the other way round (`plain_moved_lines`), so a missing setting keeps the tint.
    let moved = SettingField::switch(
        |cx| !SettingsStore::settings(cx).diff.plain_moved_lines,
        |value, cx| {
            SettingsStore::update(cx, |settings| settings.diff.plain_moved_lines = !value);
        },
    )
    .default_value(true);
    SettingPage::new(t!("settings.diff").into_owned())
        .icon(AppIcon::File)
        .group(
            SettingGroup::new()
                .description(t!("settings.diff_hint").into_owned())
                .item(SettingItem::new(
                    t!("settings.diff_layout").into_owned(),
                    layout,
                ))
                .item(SettingItem::new(
                    t!("settings.ignore_whitespace").into_owned(),
                    whitespace,
                ))
                .item(
                    SettingItem::new(t!("settings.wrap_lines").into_owned(), wrap)
                        .description(t!("settings.wrap_lines_hint").into_owned()),
                )
                .item(
                    SettingItem::new(t!("settings.tab_width").into_owned(), tab_width)
                        .description(t!("settings.tab_width_hint").into_owned()),
                )
                .item(
                    SettingItem::new(t!("settings.moved_tint").into_owned(), moved)
                        .description(t!("settings.moved_tint_hint").into_owned()),
                ),
        )
}

/// The Colors page's groups, each token exactly once (a test checks).
const COLOR_GROUPS: &[(&str, &[ColorToken])] = &[
    (
        "surfaces",
        &[
            ColorToken::Background,
            ColorToken::Panel,
            ColorToken::Panel2,
            ColorToken::Line,
            ColorToken::Hover,
            ColorToken::Selection,
            ColorToken::Chip,
            ColorToken::Scrim,
        ],
    ),
    (
        "text",
        &[
            ColorToken::Foreground,
            ColorToken::Foreground2,
            ColorToken::Foreground3,
        ],
    ),
    (
        "accent",
        &[ColorToken::Accent, ColorToken::AccentForeground],
    ),
    (
        "lanes",
        &[
            ColorToken::Lane0,
            ColorToken::Lane1,
            ColorToken::Lane2,
            ColorToken::Lane3,
            ColorToken::Lane4,
            ColorToken::Lane5,
            ColorToken::Lane6,
            ColorToken::Lane7,
        ],
    ),
    (
        "changes",
        &[
            ColorToken::Add,
            ColorToken::Delete,
            ColorToken::Modified,
            ColorToken::AddBackground,
            ColorToken::DeleteBackground,
            ColorToken::MovedFromBackground,
            ColorToken::MovedToBackground,
            ColorToken::FindBackground,
            ColorToken::OursBackground,
            ColorToken::TheirsBackground,
        ],
    ),
];

fn group_title(group: &str) -> String {
    match group {
        "surfaces" => t!("colors.group_surfaces"),
        "text" => t!("colors.group_text"),
        "accent" => t!("colors.group_accent"),
        "lanes" => t!("colors.group_lanes"),
        _ => t!("colors.group_changes"),
    }
    .into_owned()
}

fn color_label(token: ColorToken) -> String {
    let lane = |n: usize| t!("colors.lane", n = n).into_owned();
    match token {
        ColorToken::Background => t!("colors.bg").into_owned(),
        ColorToken::Panel => t!("colors.panel").into_owned(),
        ColorToken::Panel2 => t!("colors.panel2").into_owned(),
        ColorToken::Line => t!("colors.line").into_owned(),
        ColorToken::Foreground => t!("colors.fg").into_owned(),
        ColorToken::Foreground2 => t!("colors.fg2").into_owned(),
        ColorToken::Foreground3 => t!("colors.fg3").into_owned(),
        ColorToken::Accent => t!("colors.acc").into_owned(),
        ColorToken::AccentForeground => t!("colors.acc_fg").into_owned(),
        ColorToken::Selection => t!("colors.sel").into_owned(),
        ColorToken::Hover => t!("colors.hover").into_owned(),
        ColorToken::Chip => t!("colors.chip").into_owned(),
        ColorToken::Lane0 => lane(1),
        ColorToken::Lane1 => lane(2),
        ColorToken::Lane2 => lane(3),
        ColorToken::Lane3 => lane(4),
        ColorToken::Lane4 => lane(5),
        ColorToken::Lane5 => lane(6),
        ColorToken::Lane6 => lane(7),
        ColorToken::Lane7 => lane(8),
        ColorToken::Add => t!("colors.add").into_owned(),
        ColorToken::Delete => t!("colors.del").into_owned(),
        ColorToken::Modified => t!("colors.mod").into_owned(),
        ColorToken::AddBackground => t!("colors.add_bg").into_owned(),
        ColorToken::DeleteBackground => t!("colors.del_bg").into_owned(),
        ColorToken::MovedFromBackground => t!("colors.moved_from_bg").into_owned(),
        ColorToken::MovedToBackground => t!("colors.moved_to_bg").into_owned(),
        ColorToken::FindBackground => t!("colors.find_bg").into_owned(),
        ColorToken::OursBackground => t!("colors.ours_bg").into_owned(),
        ColorToken::TheirsBackground => t!("colors.theirs_bg").into_owned(),
        ColorToken::Scrim => t!("colors.scrim").into_owned(),
    }
}

/// A small sample in the edited palette: a selected graph row with branch and tag pills,
/// the three text colors, and an added and a removed line.
fn preview(p: Palette, m: Metrics) -> impl IntoElement {
    let pill = |label: &'static str, color: Hsla, filled: bool| {
        div()
            .px_1p5()
            .rounded(m.dp(5.))
            .border_1()
            .border_color(color)
            .when(filled, |pill| pill.bg(color))
            .text_color(if filled { p.acc_fg } else { color })
            .text_size(m.text.xs)
            .child(label)
    };
    let line = |sign: &'static str, text: &'static str, color: Hsla, bg: Hsla| {
        h_flex()
            .gap_2()
            .px_2()
            .bg(bg)
            .text_color(color)
            .child(sign)
            .child(text)
    };
    v_flex()
        .w_full()
        .rounded(m.dp(6.))
        .border_1()
        .border_color(p.line)
        .bg(p.bg)
        .overflow_hidden()
        .child(
            h_flex()
                .gap_2()
                .px_2()
                .py_1()
                .bg(p.sel)
                .child(pill("main", p.lanes[0], true))
                .child(pill("origin/main", p.lanes[0], false))
                .child(
                    div()
                        .px_1p5()
                        .rounded(m.dp(5.))
                        .bg(p.chip)
                        .text_color(p.fg2)
                        .text_size(m.text.xs)
                        .child("v3.2.0"),
                )
                .child(
                    div()
                        .text_color(p.fg)
                        .child(t!("settings.preview_subject").into_owned()),
                )
                .child(
                    div()
                        .text_color(p.fg2)
                        .child(t!("settings.preview_author").into_owned()),
                )
                .child(div().text_color(p.fg3).child("fe064b7e")),
        )
        .child(
            h_flex().gap_2().px_2().py_1().bg(p.panel).children(
                p.lanes
                    .map(|lane| div().size(m.dp(10.)).rounded_full().bg(lane)),
            ),
        )
        .child(line("-", "return diffSince(prev.sha);", p.del, p.del_bg))
        .child(line("+", "return diffSince(prev?.sha);", p.add, p.add_bg))
}

#[cfg(test)]
mod tests {
    // Not `super::*`: GPUI's `test` macro would shadow `#[test]`.
    use super::COLOR_GROUPS;
    use gitweft_ui::ColorToken;

    #[test]
    fn every_color_is_on_the_colors_page_once() {
        let mut listed: Vec<&str> = COLOR_GROUPS
            .iter()
            .flat_map(|(_, tokens)| tokens.iter().map(|token| token.key()))
            .collect();
        let count = listed.len();
        listed.sort();
        listed.dedup();
        assert_eq!(listed.len(), count, "a color is listed twice");
        assert_eq!(count, ColorToken::ALL.len(), "a color is missing");
    }
}
