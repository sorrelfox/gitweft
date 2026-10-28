//! The window's top view: one tab per repository, the tab strip in the title bar, and the
//! dialog and notification layers (once per window).

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use gitweft_core::jobs::WorkerPool;
use gitweft_ui::truncated::truncated;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{
    Icon, Root, Sizable, TITLE_BAR_HEIGHT, TitleBar, WindowExt as _, h_flex, v_flex,
    window_paddings,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use gitweft_core::session::{Session, WindowSize};
use gitweft_core::settings::ThemeChoice;
use gitweft_ui::{AppIcon, LookExt, Metrics, Palette, SettingsStore};

use crate::keymap;
use crate::palette::PaletteCommand;
use crate::session::SessionStore;
use crate::tab_notes::Waiting;
use crate::workspace::{Workspace, WorkspaceEnv, WorkspaceEvent};

actions!(
    shell,
    [
        NewTab,
        CloseTab,
        NextTab,
        PreviousTab,
        ActivateTab1,
        ActivateTab2,
        ActivateTab3,
        ActivateTab4,
        ActivateTab5,
        ActivateTab6,
        ActivateTab7,
        ActivateTab8,
        ActivateLastTab,
        OpenCommandPalette,
        OpenRepository
    ]
);

pub(crate) const KEY_CONTEXT: &str = "Shell";

pub(crate) fn bind_keys(cx: &mut App) {
    let context = Some(KEY_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("secondary-t", NewTab, context),
        KeyBinding::new("secondary-w", CloseTab, context),
        // The binding added last is the one the palette shows.
        KeyBinding::new("ctrl-pagedown", NextTab, context),
        KeyBinding::new("ctrl-tab", NextTab, context),
        KeyBinding::new("ctrl-pageup", PreviousTab, context),
        KeyBinding::new("ctrl-shift-tab", PreviousTab, context),
        KeyBinding::new("secondary-1", ActivateTab1, context),
        KeyBinding::new("secondary-2", ActivateTab2, context),
        KeyBinding::new("secondary-3", ActivateTab3, context),
        KeyBinding::new("secondary-4", ActivateTab4, context),
        KeyBinding::new("secondary-5", ActivateTab5, context),
        KeyBinding::new("secondary-6", ActivateTab6, context),
        KeyBinding::new("secondary-7", ActivateTab7, context),
        KeyBinding::new("secondary-8", ActivateTab8, context),
        // As in browsers: the last tab, however many there are.
        KeyBinding::new("secondary-9", ActivateLastTab, context),
        KeyBinding::new("secondary-shift-p", OpenCommandPalette, context),
    ]);
    // Open… is Cmd+O in every Mac app; elsewhere it has no key.
    if cfg!(target_os = "macos") {
        cx.bind_keys([KeyBinding::new("cmd-o", OpenRepository, context)]);
    }
}

/// A palette command that calls a method of the shell.
fn shell_command(
    this: &WeakEntity<Shell>,
    group: &SharedString,
    label: impl Into<String>,
    action: impl Fn(&mut Shell, &mut Window, &mut Context<Shell>) + 'static,
) -> PaletteCommand {
    let this = this.clone();
    PaletteCommand::new(group.clone(), label, move |window, cx| {
        this.update(cx, |this, cx| action(this, window, cx)).ok();
    })
}

/// A tab: its workspace, the subscription that redraws the tab strip when it changes, and
/// the one that opens the tabs it asks for (a worktree).
struct TabEntry {
    workspace: Entity<Workspace>,
    _changes: Subscription,
    _events: Subscription,
}

/// Drag payload of a tab: its index. Dropped on another tab, it takes that tab's place.
struct TabDrag(usize);

/// What follows the pointer while a tab is dragged: its label.
struct TabDragLabel(String);

impl Render for TabDragLabel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (p, m) = (*cx.palette(), *cx.metrics());
        div()
            .px_2()
            .py_0p5()
            .rounded(m.radius)
            .border_1()
            .border_color(p.acc)
            .bg(p.panel)
            .text_color(p.fg)
            .text_size(m.text.small)
            .child(self.0.clone())
    }
}

pub struct Shell {
    /// Never empty: closing the last tab leaves a start screen.
    tabs: Vec<TabEntry>,
    active: usize,
    /// The tab strip's sideways scroll, and the active tab and tab count it last brought
    /// the active tab into view for.
    tab_scroll: ScrollHandle,
    tab_revealed: Cell<Option<(usize, usize)>>,
    /// Passed to every tab it opens.
    env: WorkspaceEnv,
    /// Shared by every tab's jobs.
    pool: Arc<WorkerPool>,
    window_active: bool,
    /// Last title pushed to the compositor, to avoid re-setting it every frame.
    window_title: String,
    /// `GITWEFT_FRAME_STATS=1`: when the frame being drawn started rendering (see
    /// `render`). GPUI's own `ZED_MEASUREMENTS` doesn't report the frames drawn while
    /// typing here (it logged only the cursor blink), so it can't answer "how long do
    /// frames take".
    frame_started: Option<Rc<Cell<Option<Instant>>>>,
    /// `GITWEFT_FRAME_STATS=1`: logs each keystroke with its time, to line keys up with
    /// the frames drawn for them.
    _keystrokes: Option<Subscription>,
    _appearance: Subscription,
    _settings: Subscription,
    _activation: Subscription,
    _focus_lost: Subscription,
}

impl Shell {
    /// Tab `ix`'s workspace (the headless UI tests).
    #[cfg(test)]
    pub(crate) fn workspace(&self, ix: usize) -> Entity<Workspace> {
        self.tabs[ix].workspace.clone()
    }

    /// Opens one tab per path (the start screen when there are none) and shows tab
    /// `active`. `started` goes to the first tab for `GITWEFT_TRACE_STARTUP`.
    pub fn new(
        paths: Vec<PathBuf>,
        active: usize,
        started: Option<Instant>,
        env: WorkspaceEnv,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Follow the desktop's light/dark switch while running (`apply` only follows it
        // when the Theme setting is System).
        let appearance = cx.observe_window_appearance(window, |_, window, cx| {
            gitweft_ui::apply(window, cx);
        });
        // A settings change restyles the window and reaches every tab.
        let settings = cx.observe_global_in::<SettingsStore>(window, |this, window, cx| {
            gitweft_ui::apply(window, cx);
            for tab in &this.tabs {
                tab.workspace
                    .update(cx, |workspace, cx| workspace.settings_changed(cx));
            }
            cx.notify();
        });
        // Coming back to the window refreshes the active tab: a safety net next to the
        // file watcher (e.g. when watching is limited).
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            this.window_active = window.is_window_active();
            this.sync_visibility(cx);
        });
        // A view that closed while it had the keys can outlive its last frame (a palette
        // command still holds it), so the window's focus stays on it, nothing is drawn
        // there, and every key went nowhere. The shown tab takes them back.
        let focus_lost = cx.on_focus_lost(window, |this, window, cx| {
            this.active_workspace()
                .clone()
                .update(cx, |workspace, cx| workspace.take_lost_focus(window, cx));
        });
        let frame_stats = std::env::var_os("GITWEFT_FRAME_STATS").is_some();
        let keystrokes = frame_stats.then(|| {
            cx.observe_keystrokes(|_, event, _, _| {
                eprintln!(
                    "gitweft key: {} @{:.3}",
                    event.keystroke.key,
                    epoch_seconds()
                );
            })
        });
        let mut this = Self {
            tabs: Vec::new(),
            active: 0,
            tab_scroll: ScrollHandle::new(),
            tab_revealed: Cell::new(None),
            env,
            pool: WorkerPool::new(),
            window_active: true,
            window_title: String::new(),
            frame_started: frame_stats.then(|| Rc::new(Cell::new(None))),
            _keystrokes: keystrokes,
            _appearance: appearance,
            _settings: settings,
            _activation: activation,
            _focus_lost: focus_lost,
        };
        // The startup trace goes to the tab that will be shown: only the active tab is drawn,
        // and the trace fires on its first frame.
        let active = active.min(paths.len().max(1) - 1);
        if paths.is_empty() {
            this.add_tab(None, started, window, cx);
        }
        for (ix, path) in paths.into_iter().enumerate() {
            let started = started.filter(|_| ix == active);
            this.add_tab(Some(path), started, window, cx);
        }
        this.activate(active, window, cx);
        this
    }

    fn add_tab(
        &mut self,
        path: Option<PathBuf>,
        started: Option<Instant>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let pool = self.pool.clone();
        let env = self.env.clone();
        let workspace = cx.new(|cx| Workspace::new(path, started, pool, env, window, cx));
        // Inactive tabs aren't drawn, so without this their changes (a repository that
        // finished loading) wouldn't reach the tab strip.
        let changes = cx.observe(&workspace, |_, _, cx| cx.notify());
        let events = cx.subscribe_in(
            &workspace,
            window,
            |this, _, event: &WorkspaceEvent, window, cx| match event {
                // A tab that already shows it comes forward instead of a second one.
                WorkspaceEvent::OpenInNewTab(path) => {
                    let open = this
                        .tabs
                        .iter()
                        .position(|tab| tab.workspace.read(cx).shows(path));
                    let ix = match open {
                        Some(ix) => ix,
                        None => this.add_tab(Some(path.clone()), None, window, cx),
                    };
                    this.activate(ix, window, cx);
                }
            },
        );
        self.tabs.push(TabEntry {
            workspace,
            _changes: changes,
            _events: events,
        });
        self.tabs.len() - 1
    }

    fn active_workspace(&self) -> &Entity<Workspace> {
        &self.tabs[self.active].workspace
    }

    /// Shows tab `ix` and gives it the keyboard focus.
    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        self.active = ix;
        self.sync_visibility(cx);
        let focus = self.active_workspace().read(cx).focus_handle().clone();
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Tells each tab whether it is on screen, so the one coming into view refreshes.
    fn sync_visibility(&mut self, cx: &mut Context<Self>) {
        for (ix, tab) in self.tabs.iter().enumerate() {
            let visible = self.window_active && ix == self.active;
            tab.workspace
                .update(cx, |workspace, cx| workspace.set_visible(visible, cx));
        }
    }

    fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.add_tab(None, None, window, cx);
        self.activate(ix, window, cx);
    }

    /// Asks for a repository to open, in a start screen.
    fn open_repository(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.start_in_tab(window, cx, |workspace, _, cx| {
            workspace.choose_repository(cx)
        });
    }

    /// The next (or previous) tab, wrapping around.
    fn cycle(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let len = self.tabs.len();
        let ix = if forward {
            (self.active + 1) % len
        } else {
            (self.active + len - 1) % len
        };
        self.activate(ix, window, cx);
    }

    /// Moves tab `from` to `to`'s place (a tab dropped on another). The active tab stays
    /// the active one; the session records the new order with the next frame.
    fn move_tab(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        let len = self.tabs.len();
        if from == to || from >= len || to >= len {
            return;
        }
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.active = active_after_move(self.active, from, to);
        cx.notify();
    }

    /// Closes tab `ix`. A tab with a network operation or clone running asks first,
    /// since closing it cancels that.
    fn close_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(ix) else {
            return;
        };
        if !tab.workspace.read(cx).busy() {
            self.remove_tab(ix, window, cx);
            return;
        }
        let id = tab.workspace.entity_id();
        let message = t!("tabs.close_busy").into_owned();
        let detail = t!("tabs.close_busy_detail").into_owned();
        let answer = window.prompt(
            PromptLevel::Warning,
            &message,
            Some(&detail),
            &[
                PromptButton::ok(t!("tabs.close_anyway").into_owned()),
                PromptButton::cancel(t!("toolbar.cancel").into_owned()),
            ],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            this.update_in(cx, |this, window, cx| {
                // Other tabs may have been closed meanwhile.
                if let Some(ix) = this
                    .tabs
                    .iter()
                    .position(|tab| tab.workspace.entity_id() == id)
                {
                    this.remove_tab(ix, window, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Dropping the workspace stops its watcher and cancels what it was running. The tab
    /// to the right takes the place of a closed active tab; the last one leaves a start
    /// screen behind.
    fn remove_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let removed = self.tabs.remove(ix);
        crate::tab_notes::forget(removed.workspace.entity_id(), cx);
        if self.tabs.is_empty() {
            self.add_tab(None, None, window, cx);
        }
        if ix < self.active {
            self.active -= 1;
        }
        let active = self.active.min(self.tabs.len() - 1);
        self.activate(active, window, cx);
    }

    /// Runs `start` on a start screen: the active tab's when it shows one, else a new tab's.
    fn start_in_tab(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        start: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>),
    ) {
        let ix = if self.active_workspace().read(cx).is_start_screen() {
            self.active
        } else {
            self.add_tab(None, None, window, cx)
        };
        self.activate(ix, window, cx);
        let workspace = self.tabs[ix].workspace.clone();
        workspace.update(cx, |workspace, cx| start(workspace, window, cx));
    }

    /// Opens a repository: its tab comes forward when one shows it already.
    fn open_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let open = self
            .tabs
            .iter()
            .position(|tab| tab.workspace.read(cx).shows(&path));
        match open {
            Some(ix) => self.activate(ix, window, cx),
            None => self.start_in_tab(window, cx, move |workspace, _, cx| {
                workspace.open_repository(path, cx)
            }),
        }
    }

    /// Opens the paths another `gitweft` process sent (`lib.rs`), each as `open_path` does.
    pub(crate) fn open_paths(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for path in paths {
            self.open_path(path, window, cx);
        }
    }

    /// Records the tabs and the window's size for the next start (`session.rs`). Start
    /// screens and repositories that failed to open aren't reopened.
    fn record_session(&self, window: &Window, cx: &mut App) {
        let mut tabs = Vec::new();
        let mut active = None;
        for (ix, tab) in self.tabs.iter().enumerate() {
            if let Some(path) = tab.workspace.read(cx).session_path() {
                if ix == self.active {
                    active = Some(tabs.len());
                }
                tabs.push(path);
            }
        }
        // With the client-side shadow insets: a window opens at that size, insets included
        // (the inner bounds made it 40 px smaller at every start).
        let (bounds, maximized) = match window.window_bounds() {
            WindowBounds::Windowed(bounds) | WindowBounds::Fullscreen(bounds) => (bounds, false),
            WindowBounds::Maximized(bounds) => (bounds, true),
        };
        let window = WindowSize {
            width: f32::from(bounds.size.width),
            height: f32::from(bounds.size.height),
            maximized,
        };
        SessionStore::record(
            cx,
            Session {
                tabs,
                active,
                window: Some(window),
            },
        );
    }

    /// The command palette: the active tab's commands, then the window's.
    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut commands = self
            .active_workspace()
            .update(cx, |workspace, cx| workspace.palette_commands(cx));
        commands.extend(self.palette_commands(cx));
        crate::palette::open(commands, window, cx);
    }

    /// Tabs, repositories, settings and the theme, then the recent repositories.
    fn palette_commands(&self, cx: &mut Context<Self>) -> Vec<PaletteCommand> {
        let this = cx.entity().downgrade();
        let group: SharedString = t!("palette.group.window").into_owned().into();
        let mut commands = vec![
            shell_command(&this, &group, t!("palette.tab_new"), |this, window, cx| {
                this.new_tab(window, cx)
            })
            .key(NewTab, Some(KEY_CONTEXT)),
            shell_command(
                &this,
                &group,
                t!("palette.tab_close"),
                |this, window, cx| this.close_tab(this.active, window, cx),
            )
            .key(CloseTab, Some(KEY_CONTEXT)),
        ];
        if self.tabs.len() > 1 {
            commands.push(
                shell_command(&this, &group, t!("palette.tab_next"), |this, window, cx| {
                    this.cycle(true, window, cx)
                })
                .key(NextTab, Some(KEY_CONTEXT)),
            );
            commands.push(
                shell_command(
                    &this,
                    &group,
                    t!("palette.tab_previous"),
                    |this, window, cx| this.cycle(false, window, cx),
                )
                .key(PreviousTab, Some(KEY_CONTEXT)),
            );
            for (ix, tab) in self.tabs.iter().enumerate() {
                if ix != self.active {
                    let name = tab.workspace.read(cx).tab_label();
                    let label = t!("palette.tab_switch", name = name);
                    commands.push(shell_command(
                        &this,
                        &group,
                        label,
                        move |this, window, cx| this.activate(ix, window, cx),
                    ));
                }
            }
        }
        commands.push(
            shell_command(
                &this,
                &group,
                t!("start.open_repository"),
                |this, window, cx| this.open_repository(window, cx),
            )
            .key(OpenRepository, Some(KEY_CONTEXT)),
        );
        commands.push(shell_command(
            &this,
            &group,
            t!("start.create_repository"),
            |this, window, cx| {
                this.start_in_tab(window, cx, |workspace, _, cx| {
                    workspace.choose_new_repository(cx)
                })
            },
        ));
        commands.push(shell_command(
            &this,
            &group,
            t!("start.clone_repository"),
            |this, window, cx| {
                this.start_in_tab(window, cx, |workspace, window, cx| {
                    workspace.choose_clone(window, cx)
                })
            },
        ));
        commands.push(
            shell_command(&this, &group, t!("palette.settings"), |_, _, cx| {
                crate::settings_window::open(cx)
            })
            .key(crate::settings_window::OpenSettings, None),
        );
        let dark = cx.palette().dark;
        let theme = if dark {
            t!("title.light_theme")
        } else {
            t!("title.dark_theme")
        };
        commands.push(shell_command(&this, &group, theme, move |_, _, cx| {
            SettingsStore::update(cx, |settings| {
                settings.appearance.theme = if dark {
                    ThemeChoice::Light
                } else {
                    ThemeChoice::Dark
                };
            });
        }));
        // Each tab reads the recent list when it opens and when it remembers its own
        // repository, so together they know them all. The active tab's comes first.
        let recent: SharedString = t!("start.recent").into_owned().into();
        let mut paths: Vec<PathBuf> = Vec::new();
        let tabs = std::iter::once(self.active_workspace())
            .chain(self.tabs.iter().map(|tab| &tab.workspace));
        for workspace in tabs {
            for path in workspace.read(cx).recent_repositories() {
                if !paths.contains(path) {
                    paths.push(path.clone());
                }
            }
        }
        let active = self.active_workspace().read(cx);
        paths.retain(|path| !active.shows(path));
        for path in paths {
            let name = path.file_name().map_or_else(
                || path.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            );
            let label = t!("palette.open_recent", name = name);
            let detail = path.display().to_string();
            commands.push(
                shell_command(&this, &recent, label, move |this, window, cx| {
                    this.open_path(path.clone(), window, cx)
                })
                .detail(detail),
            );
        }
        commands
    }

    fn sync_window_title(&mut self, window: &mut Window, cx: &App) {
        let title = self.active_workspace().read(cx).window_title();
        if title != self.window_title {
            window.set_window_title(&title);
            self.window_title = title;
        }
    }

    /// The tabs (the active one raised onto the toolbar below), "+" for a new one, the
    /// active tab's branch and how far it is from its upstream, then Settings and the theme.
    /// The title bar; `room`: the most its content may take ([`title_bar_room`]).
    fn render_title_bar(&self, room: Pixels, cx: &mut Context<Self>) -> impl IntoElement {
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);
        let workspace = self.active_workspace().read(cx);
        let branch = workspace.title_branch();
        let head_color = p.lanes[workspace.head_lane(cx)];
        let status = workspace.status_line();
        let labels: Vec<String> = self
            .tabs
            .iter()
            .map(|tab| tab.workspace.read(cx).tab_label())
            .collect();
        let shown = (self.active, self.tabs.len());
        if self.tab_revealed.get() != Some(shown) {
            self.tab_revealed.set(Some(shown));
            self.tab_scroll.scroll_to_item(self.active);
        }
        let tabs: Vec<AnyElement> = labels
            .into_iter()
            .enumerate()
            .map(|(ix, label)| self.render_tab(ix, label, &p, &m, cx).into_any_element())
            .collect();
        let branch = match branch {
            Some((name, detail)) => h_flex()
                .gap(m.dp(6.))
                .min_w_0()
                .child(Icon::new(AppIcon::Branch).text_color(head_color))
                .child(truncated("head", name).text_color(p.fg2))
                .children(detail.map(|detail| div().text_color(p.fg3).child(format!("· {detail}"))))
                .into_any_element(),
            None => truncated("status", status)
                .text_color(p.fg3)
                .into_any_element(),
        };
        TitleBar::new().h(m.title_bar).child(
            h_flex()
                .h_full()
                .flex_1()
                .min_w_0()
                .max_w(room)
                // Tabs stand on the bottom edge; the rest is centred on its own.
                .items_end()
                .child(
                    // Tabs that don't fit even at their narrowest scroll sideways (the
                    // wheel, or the All tabs menu), never over the window's buttons, and
                    // the active one is scrolled into view. Only sideways: the active
                    // tab still reaches over the bottom border.
                    h_flex()
                        .id("tab-strip")
                        .h_full()
                        .items_end()
                        .min_w_0()
                        .flex_shrink(1.)
                        .overflow_x_scroll()
                        .track_scroll(&self.tab_scroll)
                        .children(tabs),
                )
                .when(self.tabs.len() > 1, |bar| {
                    bar.child(
                        div()
                            .h_full()
                            .flex()
                            .items_center()
                            .flex_shrink_0()
                            .pl(m.dp(4.))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(self.render_tab_list(cx)),
                    )
                })
                .child(
                    // Tabs stop the title bar from taking a press as a window drag;
                    // this button needs the same.
                    div()
                        .h_full()
                        .flex()
                        .items_center()
                        .flex_shrink_0()
                        .px(m.dp(4.))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(
                            Button::new("new-tab")
                                .ghost()
                                .small()
                                .icon(Icon::from(AppIcon::Plus))
                                .tooltip(
                                    t!("tabs.new", keys = keymap::action_key_text(&NewTab, cx))
                                        .into_owned(),
                                )
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.new_tab(window, cx)),
                                ),
                        ),
                )
                .child(
                    div()
                        .h_full()
                        .flex()
                        .items_center()
                        .px(m.dp(8.))
                        .min_w_0()
                        .text_size(m.text.small)
                        .child(branch),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .h_full()
                        .flex()
                        .items_center()
                        .flex_shrink_0()
                        .child(self.render_title_actions(cx)),
                ),
        )
    }

    /// Every tab in a menu, the active one checked: the way to a tab the strip squeezed.
    fn render_tab_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let labels: Vec<String> = self
            .tabs
            .iter()
            .map(|tab| tab.workspace.read(cx).tab_label())
            .collect();
        let active = self.active;
        let this = cx.entity().downgrade();
        Button::new("tab-list")
            .ghost()
            .small()
            .icon(Icon::from(AppIcon::ChevronDown))
            // No tooltip: it would stay over the menu's first item (as the other menu
            // chevrons, the toolbar's, have none).
            .dropdown_menu(move |menu, _, cx| {
                let menu = menu.scrollable(true).max_h(cx.metrics().dp(480.));
                labels.iter().enumerate().fold(menu, |menu, (ix, label)| {
                    let this = this.clone();
                    let item = PopupMenuItem::new(label.clone())
                        .checked(ix == active)
                        .on_click(move |_, window, cx| {
                            this.update(cx, |this, cx| this.activate(ix, window, cx))
                                .ok();
                        });
                    menu.item(item)
                })
            })
    }

    /// A tab: the active one is a raised panel joining the toolbar under it. Middle-click
    /// closes it, as in browsers; dragged onto another tab, it takes that one's place.
    fn render_tab(
        &self,
        ix: usize,
        label: String,
        p: &Palette,
        m: &Metrics,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = ix == self.active;
        let (fg, fg2, fg3, hover) = (p.fg, p.fg2, p.fg3, p.hover);
        let workspace = &self.tabs[ix].workspace;
        // What it keeps while in the background (a question in the modified color: it
        // waits for an answer), and an open that failed.
        let waiting = match crate::tab_notes::waiting(workspace.entity_id(), cx) {
            _ if active => None,
            Waiting::Nothing => None,
            Waiting::Notes => Some(p.acc),
            Waiting::Question => Some(p.modified),
        };
        let failed = workspace.read(cx).open_failed();
        let (del, acc) = (p.del, p.acc);
        let dragged = label.clone();
        h_flex()
            .id(("tab", ix))
            .debug_selector(|| format!("tab-{ix}"))
            .h(m.title_bar - m.dp(6.))
            // Many tabs share the strip: each gives way down to a short label.
            .flex_shrink(1.)
            .min_w(m.dp(72.))
            .gap(m.dp(8.))
            .pl(m.dp(14.))
            .pr(m.dp(6.))
            .rounded_t(m.radius)
            .border_1()
            .border_b_0()
            // Over the title bar's bottom border, so the tab joins the toolbar.
            .mb(px(-1.))
            .map(|tab| {
                if active {
                    tab.bg(p.panel)
                        .border_color(p.line)
                        .text_color(fg)
                        .font_weight(FontWeight::MEDIUM)
                } else {
                    tab.border_color(transparent_black())
                        .text_color(fg3)
                        .hover(move |style| style.text_color(fg2))
                }
            })
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, window, cx| this.activate(ix, window, cx)))
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |this, _, window, cx| this.close_tab(ix, window, cx)),
            )
            .on_drag(TabDrag(ix), move |_, _, _, cx| {
                let label = dragged.clone();
                cx.new(|_| TabDragLabel(label))
            })
            .drag_over::<TabDrag>(move |style, _, _, _| style.border_color(acc))
            .on_drop(cx.listener(move |this, drag: &TabDrag, _, cx| this.move_tab(drag.0, ix, cx)))
            .when_some(waiting, |tab, color| {
                tab.child(gitweft_ui::widgets::dot(color, m.dp(6.)))
            })
            .child(
                truncated("label", label)
                    .max_w(m.dp(200.))
                    .when(failed, |label| label.text_color(del)),
            )
            .child(
                gitweft_ui::widgets::LinkButton::icon(
                    ("close-tab", ix),
                    AppIcon::Close,
                    fg3,
                    m.dp(18.),
                )
                .size(m.dp(12.))
                .hover_bg(hover)
                .hover_color(fg)
                .on_click(cx.listener(move |this, _, window, cx| this.close_tab(ix, window, cx))),
            )
    }

    /// Settings, and the sun/moon button: it switches to the other theme and remembers
    /// the choice (the Theme setting becomes Light or Dark instead of following the
    /// desktop).
    fn render_title_actions(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let dark = cx.palette().dark;
        let (icon, tip) = if dark {
            (AppIcon::Sun, t!("title.light_theme"))
        } else {
            (AppIcon::Moon, t!("title.dark_theme"))
        };
        // Like "+": the title bar would take the press for a window drag.
        h_flex()
            .gap_1()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                Button::new("open-settings")
                    .ghost()
                    .small()
                    .icon(Icon::from(AppIcon::Settings))
                    .tooltip(
                        t!(
                            "title.settings",
                            keys =
                                keymap::action_key_text(&crate::settings_window::OpenSettings, cx)
                        )
                        .into_owned(),
                    )
                    .on_click(|_, _, cx| crate::settings_window::open(cx)),
            )
            .child(
                Button::new("theme-toggle")
                    .ghost()
                    .small()
                    .icon(Icon::from(icon))
                    .tooltip(tip.into_owned())
                    .on_click(move |_, _, cx| {
                        SettingsStore::update(cx, |settings| {
                            settings.appearance.theme = if dark {
                                ThemeChoice::Light
                            } else {
                                ThemeChoice::Dark
                            };
                        });
                    }),
            )
    }
}

/// Wall-clock seconds, for `GITWEFT_FRAME_STATS` lines that scripts line up with their own
/// timestamps.
fn epoch_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64())
}

/// Handles a tab key. Tab keys do nothing while a dialog is open: it belongs to the
/// active tab, and switching or closing would leave it floating over another one.
fn on_tab_key<A: Action>(
    element: Div,
    cx: &mut Context<Shell>,
    run: impl Fn(&mut Shell, &mut Window, &mut Context<Shell>) + 'static,
) -> Div {
    element.on_action(cx.listener(move |this, _: &A, window, cx| {
        if !window.has_active_dialog(cx) {
            run(this, window, cx);
        }
    }))
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // What the tab now shown kept while it was in the background comes out.
        crate::tab_notes::set_active(self.active_workspace().entity_id(), cx);
        self.sync_window_title(window, cx);
        self.record_session(window, cx);
        // `GITWEFT_FRAME_STATS=1`: a frame starts when the shell renders and ends when this
        // marker, the last element of the shell, paints (render + layout + paint).
        let frame_marker = self.frame_started.clone().map(|started| {
            started.set(Some(Instant::now()));
            canvas(
                |_, _, _| {},
                move |_, _, _, _| {
                    if let Some(start) = started.take() {
                        let ms = start.elapsed().as_secs_f64() * 1000.0;
                        eprintln!("gitweft frame: {ms:.3} ms @{:.3}", epoch_seconds());
                    }
                },
            )
            .absolute()
            .size_0()
        });
        let active = self.active_workspace().clone();
        let mut root = v_flex().size_full().relative().key_context(KEY_CONTEXT);
        root = on_tab_key::<NewTab>(root, cx, |this, window, cx| this.new_tab(window, cx));
        root = on_tab_key::<CloseTab>(root, cx, |this, window, cx| {
            this.close_tab(this.active, window, cx)
        });
        root = on_tab_key::<NextTab>(root, cx, |this, window, cx| this.cycle(true, window, cx));
        root =
            on_tab_key::<PreviousTab>(root, cx, |this, window, cx| this.cycle(false, window, cx));
        root =
            on_tab_key::<ActivateTab1>(root, cx, |this, window, cx| this.activate(0, window, cx));
        root =
            on_tab_key::<ActivateTab2>(root, cx, |this, window, cx| this.activate(1, window, cx));
        root =
            on_tab_key::<ActivateTab3>(root, cx, |this, window, cx| this.activate(2, window, cx));
        root =
            on_tab_key::<ActivateTab4>(root, cx, |this, window, cx| this.activate(3, window, cx));
        root =
            on_tab_key::<ActivateTab5>(root, cx, |this, window, cx| this.activate(4, window, cx));
        root =
            on_tab_key::<ActivateTab6>(root, cx, |this, window, cx| this.activate(5, window, cx));
        root =
            on_tab_key::<ActivateTab7>(root, cx, |this, window, cx| this.activate(6, window, cx));
        root =
            on_tab_key::<ActivateTab8>(root, cx, |this, window, cx| this.activate(7, window, cx));
        root = on_tab_key::<ActivateLastTab>(root, cx, |this, window, cx| {
            this.activate(this.tabs.len() - 1, window, cx)
        });
        root = on_tab_key::<OpenCommandPalette>(root, cx, |this, window, cx| {
            this.open_palette(window, cx)
        });
        root = on_tab_key::<OpenRepository>(root, cx, |this, window, cx| {
            this.open_repository(window, cx)
        });
        let look = cx.look().clone();
        root.bg(look.palette.bg)
            .text_color(look.palette.fg)
            .font_family(look.fonts.ui.clone())
            .text_size(look.metrics.text.ui)
            .child(self.render_title_bar(title_bar_room(window), cx))
            .child(div().flex_1().min_h_0().child(active))
            // Dialogs (credential prompts) and notifications draw above every tab.
            .children(Root::render_dialog_layer(window, cx))
            .children(Root::render_notification_layer(window, cx))
            // Ours: every toast shown, stacked.
            .child(crate::toast::render_layer(cx))
            .child(gitweft_ui::widgets::pointer_leave_marker())
            .children(frame_marker)
    }
}

/// The kit title bar's left padding (`TITLE_BAR_LEFT_PADDING` in gpui-kit, private; 12 px
/// on Linux).
const KIT_TITLE_BAR_PADDING: Pixels = px(12.); // unscaled: the kit's own
/// The window frame's border, drawn on each side not tiled against the screen edge
/// (`BORDER_SIZE` in gpui-kit's `window_border`, private).
const KIT_FRAME_BORDER: Pixels = px(1.);

/// How wide the title bar's content may be: the window, less its frame (the shadow insets
/// and border of a window that draws its own decorations: the viewport includes them), the
/// kit's padding and the window buttons it draws (`TITLE_BAR_HEIGHT` wide each). The kit
/// keeps the content at its natural width (`flex_shrink_0`), so without this cap a crowded
/// tab strip pushed the window buttons out of the window.
fn title_bar_room(window: &Window) -> Pixels {
    let (frame, buttons) = match window.window_decorations() {
        Decorations::Server => (px(0.), 0),
        Decorations::Client { tiling } => {
            let paddings = window_paddings(window);
            let border = |tiled: bool| if tiled { px(0.) } else { KIT_FRAME_BORDER };
            let supported = window.window_controls();
            (
                paddings.left + paddings.right + border(tiling.left) + border(tiling.right),
                1 + usize::from(supported.minimize) + usize::from(supported.maximize),
            )
        }
    };
    let room = window.viewport_size().width
        - frame
        - KIT_TITLE_BAR_PADDING
        - TITLE_BAR_HEIGHT * buttons as f32;
    room.max(px(0.))
}

/// Where the active tab ends up when tab `from` moves to `to`'s place.
fn active_after_move(active: usize, from: usize, to: usize) -> usize {
    if active == from {
        to
    } else if from < active && active <= to {
        active - 1
    } else if to <= active && active < from {
        active + 1
    } else {
        active
    }
}

#[cfg(test)]
mod tests {
    use super::active_after_move;

    #[test]
    fn the_active_tab_follows_a_move() {
        // Tabs a b c d, c active.
        assert_eq!(active_after_move(2, 2, 0), 0); // c itself to the front
        assert_eq!(active_after_move(2, 0, 3), 1); // a to the end: c one left
        assert_eq!(active_after_move(2, 3, 0), 3); // d to the front: c one right
        assert_eq!(active_after_move(2, 0, 1), 2); // a and b swap: c stays
        assert_eq!(active_after_move(2, 3, 2), 3); // d onto c: c one right
    }
}

/// The tab strip in a GPUI test window (through `WorkspaceEnv`): a tab dragged onto
/// another takes its place, and the tab that was active stays active.
#[cfg(test)]
mod view_tests {
    use gitweft_testkit::Sandbox;
    use gitweft_ui::SettingsStore;
    use gpui_kit::component::Root;
    // Rust's own `#[test]`: `gpui_kit::*` (through `super::*`) brings GPUI's `test` macro.
    use std::prelude::v1::test;

    use super::*;

    /// A window (a `Root` around the shell, as the app's) with a tab on each of two
    /// sandbox repositories, the second active and drawn. Every file the shell reads is
    /// the sandbox's, never the user's.
    fn shell<'a>(
        cx: &'a mut TestAppContext,
        sb: &Sandbox,
    ) -> (Entity<Shell>, &'a mut VisualTestContext) {
        let paths: Vec<PathBuf> = ["one", "two"]
            .into_iter()
            .map(|name| {
                let repo = sb.init_repo(name);
                sb.commit_file(&repo, "f.txt", "one\n", "c1");
                repo
            })
            .collect();
        let dir = sb.path().join("data");
        std::fs::create_dir_all(&dir).unwrap();
        cx.update(|cx| {
            gpui_kit::init(cx);
            SettingsStore::init_with_file(cx, &sb.path().join("settings.json"));
        });
        let mut shell = None;
        let (_, window) = cx.add_window_view(|window, cx| {
            let env = WorkspaceEnv::isolated(&dir);
            let view = cx.new(|cx| Shell::new(paths, 1, None, env, window, cx));
            shell = Some(view.clone());
            Root::new(view, window, cx)
        });
        window.run_until_parked();
        window.update(|window, cx| window.draw(cx).clear(cx));
        window.run_until_parked();
        (shell.expect("the shell was built"), window)
    }

    /// The tabs as they stand, by the identity of the workspace each shows.
    fn tabs(shell: &Entity<Shell>, window: &mut VisualTestContext) -> (Vec<EntityId>, usize) {
        window.update(|_, cx| {
            let shell = shell.read(cx);
            let ids = shell
                .tabs
                .iter()
                .map(|tab| tab.workspace.entity_id())
                .collect();
            (ids, shell.active)
        })
    }

    /// Drags tab `from` onto tab `to` as a hand would: pressed, moved past GPUI's drag
    /// threshold, over the target, released there.
    fn drag(window: &mut VisualTestContext, from: usize, to: usize) {
        // `debug_bounds` takes a `&'static str`, so the names are spelled out.
        const TABS: [&str; 2] = ["tab-0", "tab-1"];
        let centre = |window: &mut VisualTestContext, ix: usize| {
            window
                .debug_bounds(TABS[ix])
                .expect("the tab is drawn")
                .center()
        };
        let (start, end) = (centre(window, from), centre(window, to));
        let none = Modifiers::none();
        window.simulate_mouse_down(start, MouseButton::Left, none);
        window.simulate_mouse_move(start + point(px(0.), px(5.)), MouseButton::Left, none);
        window.simulate_mouse_move(end, MouseButton::Left, none);
        window.simulate_mouse_up(end, MouseButton::Left, none);
        window.update(|window, cx| window.draw(cx).clear(cx));
        window.run_until_parked();
    }

    #[test]
    fn a_tab_dropped_on_another_takes_its_place_and_the_active_tab_stays_active() {
        let sb = Sandbox::new();
        let mut cx = TestAppContext::single();
        let (shell, window) = shell(&mut cx, &sb);
        let (before, active) = tabs(&shell, window);
        assert_eq!(active, 1);

        drag(window, 0, 1);

        let (after, active) = tabs(&shell, window);
        assert_eq!(after, [before[1], before[0]], "the first tab moved behind");
        assert_eq!(after[active], before[1], "the same tab is still active");
    }

    /// The startup trace fires on the first frame of the tab it was given, and only the
    /// active tab is drawn: with a session whose active tab isn't the first, the first tab
    /// got it and the trace never fired.
    #[test]
    fn the_startup_trace_goes_to_the_active_tab() {
        let sb = Sandbox::new();
        let mut cx = TestAppContext::single();
        let paths: Vec<PathBuf> = ["one", "two"]
            .into_iter()
            .map(|name| {
                let repo = sb.init_repo(name);
                sb.commit_file(&repo, "f.txt", "one\n", "c1");
                repo
            })
            .collect();
        let dir = sb.path().join("data");
        std::fs::create_dir_all(&dir).unwrap();
        cx.update(|cx| {
            gpui_kit::init(cx);
            SettingsStore::init_with_file(cx, &sb.path().join("settings.json"));
        });
        let mut shell = None;
        let (_, window) = cx.add_window_view(|window, cx| {
            let env = WorkspaceEnv::isolated(&dir);
            let started = Some(Instant::now());
            let view = cx.new(|cx| Shell::new(paths, 1, started, env, window, cx));
            shell = Some(view.clone());
            Root::new(view, window, cx)
        });
        let shell = shell.expect("the shell was built");
        // Both repositories load and the active tab draws, which traces its first frame. The
        // loads run on worker threads: draw until the trace is done, or give up after 5 s.
        let tracing = |window: &mut VisualTestContext| -> Vec<bool> {
            window.update(|_, cx| {
                shell
                    .read(cx)
                    .tabs
                    .iter()
                    .map(|tab| tab.workspace.read(cx).traces_startup())
                    .collect()
            })
        };
        for _ in 0..200 {
            window.run_until_parked();
            window.update(|window, cx| window.draw(cx).clear(cx));
            window.run_until_parked();
            if tracing(window) == [false, false] {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let tracing = tracing(window);
        // The active tab traced; the first, never drawn, must not be holding the trace.
        assert_eq!(tracing, [false, false]);
    }

    /// Every command the Keyboard page can bind shows its binding where the palette lists
    /// it: an entry with the same label names the same action in the same key context.
    /// The repository has what the state-dependent entries need: staged and
    /// unstaged changes, a stash, a commit. Entries with a detail act on a row, a file or a
    /// commit, and aren't the bindable command.
    #[test]
    fn palette_entries_show_the_keys_of_their_bindable_commands() {
        let sb = Sandbox::new();
        let mut cx = TestAppContext::single();
        let repo = sb.init_repo("r");
        sb.commit_file(&repo, "f.txt", "one\n", "c1");
        std::fs::write(repo.join("f.txt"), "stashed\n").unwrap();
        sb.run(&repo, &["stash"]);
        std::fs::write(repo.join("new.txt"), "new\n").unwrap();
        sb.run(&repo, &["add", "new.txt"]);
        std::fs::write(repo.join("f.txt"), "changed\n").unwrap();
        let dir = sb.path().join("data");
        std::fs::create_dir_all(&dir).unwrap();
        cx.update(|cx| {
            gpui_kit::init(cx);
            SettingsStore::init_with_file(cx, &sb.path().join("settings.json"));
        });
        let mut shell = None;
        let (_, window) = cx.add_window_view(|window, cx| {
            let env = WorkspaceEnv::isolated(&dir);
            let view = cx.new(|cx| Shell::new(vec![repo], 0, None, env, window, cx));
            shell = Some(view.clone());
            Root::new(view, window, cx)
        });
        let shell = shell.expect("the shell was built");
        // The repository loads on worker threads: wait for its status and stash list.
        let loaded = |window: &mut VisualTestContext| {
            window.update(|_, cx| {
                let workspace = shell.read(cx).tabs[0].workspace.clone();
                let commands = workspace.update(cx, |workspace, cx| workspace.palette_commands(cx));
                let (pop, unstage) = (t!("palette.pop"), t!("staging.unstage_all"));
                [pop, unstage]
                    .iter()
                    .all(|label| commands.iter().any(|c| c.label() == label.as_ref()))
            })
        };
        for _ in 0..200 {
            window.run_until_parked();
            window.update(|window, cx| window.draw(cx).clear(cx));
            if loaded(window) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(loaded(window), "the repository loaded");

        let checked: Vec<String> = window.update(|_, cx| {
            let workspace = shell.read(cx).tabs[0].workspace.clone();
            let commands = workspace.update(cx, |workspace, cx| workspace.palette_commands(cx));
            let mut checked = Vec::new();
            for bindable in crate::keymap::bindable() {
                // An entry with a detail acts on that (a sidebar row's "New branch…" starts
                // at the row): another command under the same words.
                let same = |c: &&PaletteCommand| c.label() == bindable.label && !c.has_detail();
                for command in commands.iter().filter(same) {
                    let (action, context) = command
                        .shown_key()
                        .unwrap_or_else(|| panic!("{:?} shows no key", bindable.label));
                    assert!(
                        action.partial_eq(bindable.action.as_ref()),
                        "{:?} shows {} for {}",
                        bindable.label,
                        action.name(),
                        bindable.name()
                    );
                    assert!(
                        context.is_none_or(|context| bindable.contexts.contains(&context)),
                        "{:?} in {context:?}",
                        bindable.label
                    );
                    checked.push(bindable.name().to_owned());
                }
            }
            checked
        });
        for name in [
            "workspace::Fetch",
            "workspace::Pull",
            "workspace::Push",
            "workspace::ForcePush",
            "workspace::StageAll",
            "workspace::UnstageAll",
            "workspace::StashChanges",
            "workspace::PopStash",
            "workspace::NewBranch",
            "workspace::NewTag",
            "workspace::AddRemote",
            "workspace::AddWorktree",
            "workspace::ApplyPatches",
            "workspace::CleanUntracked",
        ] {
            assert!(
                checked.iter().any(|c| c == name),
                "{name} not listed: {checked:?}"
            );
        }
    }

    /// Through `WorkspaceEnv`, the repositories a tab opens go to the list the shell was given,
    /// never to the one under the user's own data dir.
    #[test]
    fn the_repositories_opened_are_remembered_in_the_given_file() {
        let sb = Sandbox::new();
        let mut cx = TestAppContext::single();
        let (_shell, window) = shell(&mut cx, &sb);
        // Both are there once both tabs loaded, on worker threads; which is first depends on
        // which tab finished first.
        let names = || {
            let listed = gitweft_core::recent::load(&sb.path().join("data/recent-repositories"));
            let mut names: Vec<String> = listed
                .iter()
                .filter_map(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };
        for _ in 0..200 {
            window.run_until_parked();
            if names().len() == 2 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert_eq!(names(), ["one", "two"]);
        assert!(
            !gitweft_core::recent::recent_file().starts_with(sb.path()),
            "the app's own file is somewhere else, so this proves the seam"
        );
    }
}
