//! The settings window's pages and their layout: ours, in place of
//! gpui-kit's `Settings`. The kit's sidebar listed a page's groups as entries that only
//! scrolled one long page, so on a page shorter than the window they all showed the same
//! thing, and it counted untitled groups differently from the page it scrolled, so the
//! Colors entries showed the group before theirs. Here the sidebar holds pages and their
//! sub-pages, nested as deep as needed (Appearance → Colors → Surfaces), and an entry
//! shows its own page. The builders follow the kit's names, so the pages read as before.

use std::rc::Rc;

use gitweft_ui::truncated::truncated;
use gitweft_ui::widgets::scroll_into_view;
use gitweft_ui::{AppIcon, LookExt, Metrics, Palette};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{
    Input, InputEvent, InputState, NumberInput, NumberInputEvent, StepAction,
};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

/// A number field's limits and step.
#[derive(Clone, Debug)]
pub(crate) struct NumberFieldOptions {
    pub min: f64,
    pub max: f64,
    pub step: f64,
}

impl Default for NumberFieldOptions {
    fn default() -> Self {
        Self {
            min: f64::MIN,
            max: f64::MAX,
            step: 1.0,
        }
    }
}

type Render = Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>;
type Reset = (Rc<dyn Fn(&App) -> bool>, Rc<dyn Fn(&mut Window, &mut App)>);
type Getter<T> = Rc<dyn Fn(&App) -> T>;
type Setter<T> = Rc<dyn Fn(T, &mut App)>;
/// What a click on a sidebar entry does with the page's path.
pub(crate) type OnSelect = Rc<dyn Fn(Vec<usize>, &mut Window, &mut App)>;

#[derive(Clone)]
enum Kind {
    Switch,
    Input,
    Dropdown {
        options: Vec<(SharedString, SharedString)>,
        scrollable: bool,
    },
    Number(NumberFieldOptions),
    Element(Render),
}

/// A setting's control: how it reads and writes its value, and its default.
pub(crate) struct SettingField<T> {
    kind: Kind,
    get: Getter<T>,
    set: Setter<T>,
    default: Option<T>,
    reset: Option<Reset>,
}

impl<T> SettingField<T> {
    fn new(
        kind: Kind,
        get: impl Fn(&App) -> T + 'static,
        set: impl Fn(T, &mut App) + 'static,
    ) -> Self {
        Self {
            kind,
            get: Rc::new(get),
            set: Rc::new(set),
            default: None,
            reset: None,
        }
    }

    /// The value a reset puts back (and without which the field isn't reset).
    pub(crate) fn default_value(mut self, value: impl Into<T>) -> Self {
        self.default = Some(value.into());
        self
    }

    /// A reset of its own: whether the field differs from its default, and how to put it
    /// back (a field whose value isn't one the page reads, such as a color picker's).
    pub(crate) fn on_reset(
        mut self,
        is_changed: impl Fn(&App) -> bool + 'static,
        reset: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        self.reset = Some((Rc::new(is_changed), Rc::new(reset)));
        self
    }
}

impl SettingField<bool> {
    pub(crate) fn switch(
        get: impl Fn(&App) -> bool + 'static,
        set: impl Fn(bool, &mut App) + 'static,
    ) -> Self {
        Self::new(Kind::Switch, get, set)
    }
}

impl SettingField<SharedString> {
    pub(crate) fn input(
        get: impl Fn(&App) -> SharedString + 'static,
        set: impl Fn(SharedString, &mut App) + 'static,
    ) -> Self {
        Self::new(Kind::Input, get, set)
    }

    /// A choice among `options` (value, label).
    pub(crate) fn dropdown(
        options: Vec<(SharedString, SharedString)>,
        get: impl Fn(&App) -> SharedString + 'static,
        set: impl Fn(SharedString, &mut App) + 'static,
    ) -> Self {
        let kind = Kind::Dropdown {
            options,
            scrollable: false,
        };
        Self::new(kind, get, set)
    }

    /// A choice among many `options`: its menu scrolls.
    pub(crate) fn scrollable_dropdown(
        options: Vec<(SharedString, SharedString)>,
        get: impl Fn(&App) -> SharedString + 'static,
        set: impl Fn(SharedString, &mut App) + 'static,
    ) -> Self {
        let kind = Kind::Dropdown {
            options,
            scrollable: true,
        };
        Self::new(kind, get, set)
    }

    /// A control drawn by `render`.
    pub(crate) fn render<E: IntoElement + 'static>(
        render: impl Fn(&mut Window, &mut App) -> E + 'static,
    ) -> Self {
        let render: Render = Rc::new(move |window, cx| render(window, cx).into_any_element());
        Self::new(
            Kind::Element(render),
            |_| SharedString::default(),
            |_, _| {},
        )
    }
}

impl SettingField<f64> {
    pub(crate) fn number_input(
        options: NumberFieldOptions,
        get: impl Fn(&App) -> f64 + 'static,
        set: impl Fn(f64, &mut App) + 'static,
    ) -> Self {
        Self::new(Kind::Number(options), get, set)
    }
}

/// A field whatever its value's type.
pub(crate) trait AnyField {
    /// Its control; `key` names its state (unique in the window). `wide`: as wide as the
    /// page (under its label).
    fn render(
        &self,
        key: SharedString,
        wide: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement;
    fn is_changed(&self, cx: &App) -> bool;
    fn reset(&self, window: &mut Window, cx: &mut App);
}

/// The types a field's value can have, and how each is shown.
pub(crate) trait FieldValue: Clone + PartialEq + 'static {
    fn control(
        field: &SettingField<Self>,
        key: SharedString,
        wide: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement;
}

impl<T: FieldValue> AnyField for SettingField<T> {
    fn render(
        &self,
        key: SharedString,
        wide: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        if let Kind::Element(render) = &self.kind {
            return render(window, cx);
        }
        T::control(self, key, wide, window, cx)
    }

    fn is_changed(&self, cx: &App) -> bool {
        match (&self.reset, &self.default) {
            (Some((is_changed, _)), _) => is_changed(cx),
            (None, Some(default)) => (self.get)(cx) != *default,
            (None, None) => false,
        }
    }

    fn reset(&self, window: &mut Window, cx: &mut App) {
        match (&self.reset, &self.default) {
            (Some((_, reset)), _) => reset(window, cx),
            (None, Some(default)) => (self.set)(default.clone(), cx),
            (None, None) => {}
        }
    }
}

impl FieldValue for bool {
    fn control(
        field: &SettingField<bool>,
        key: SharedString,
        _: bool,
        _: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let set = field.set.clone();
        Switch::new(ElementId::Name(key))
            .checked((field.get)(cx))
            .on_click(move |checked: &bool, _, cx| set(*checked, cx))
            .into_any_element()
    }
}

impl FieldValue for SharedString {
    fn control(
        field: &SettingField<SharedString>,
        key: SharedString,
        wide: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let m = *cx.metrics();
        let value = (field.get)(cx);
        match &field.kind {
            Kind::Dropdown {
                options,
                scrollable,
            } => {
                let label = options
                    .iter()
                    .find(|(option, _)| *option == value)
                    .map_or_else(|| value.clone(), |(_, label)| label.clone());
                let (options, scrollable, set) = (options.clone(), *scrollable, field.set.clone());
                Button::new(ElementId::Name(key))
                    .label(label)
                    .dropdown_caret(true)
                    .outline()
                    .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                        let menu = options.iter().fold(menu, |menu, (option, label)| {
                            let (option, set) = (option.clone(), set.clone());
                            menu.item(
                                PopupMenuItem::new(label.clone())
                                    .checked(option == value)
                                    .on_click(move |_, _, cx| set(option.clone(), cx)),
                            )
                        });
                        menu.scrollable(scrollable)
                    })
                    .into_any_element()
            }
            _ => {
                let set = field.set.clone();
                let state = window.use_keyed_state(key, cx, {
                    let value = value.clone();
                    move |window, cx| {
                        let input = cx.new(|cx| InputState::new(window, cx).default_value(value));
                        let subscription = cx.subscribe(&input, move |_, input, event, cx| {
                            if let InputEvent::Change = event {
                                let value = input.read(cx).value();
                                set(value, cx);
                            }
                        });
                        TextState {
                            input,
                            _subscription: subscription,
                        }
                    }
                });
                // A change made elsewhere shows here.
                let input = state.read(cx).input.clone();
                if input.read(cx).value() != value {
                    input.update(cx, |input, cx| input.set_value(value, window, cx));
                }
                Input::new(&input)
                    .map(|input| {
                        if wide {
                            input.w_full()
                        } else {
                            input.w(m.dp(240.))
                        }
                    })
                    .into_any_element()
            }
        }
    }
}

impl FieldValue for f64 {
    fn control(
        field: &SettingField<f64>,
        key: SharedString,
        wide: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let m = *cx.metrics();
        let value = (field.get)(cx);
        let options = match &field.kind {
            Kind::Number(options) => options.clone(),
            _ => NumberFieldOptions::default(),
        };
        let (set, step_set) = (field.set.clone(), field.set.clone());
        let state = window.use_keyed_state(key, cx, move |window, cx| {
            let input = cx.new(|cx| InputState::new(window, cx).default_value(value.to_string()));
            let step_options = options.clone();
            let subscriptions = vec![
                // The arrows: a step up or down, within the limits.
                cx.subscribe_in(
                    &input,
                    window,
                    move |state: &mut NumberState, input, event: &NumberInputEvent, window, cx| {
                        let NumberInputEvent::Step(action) = event;
                        let Ok(now) = input.read(cx).value().parse::<f64>() else {
                            return;
                        };
                        let step = match action {
                            StepAction::Increment => step_options.step,
                            StepAction::Decrement => -step_options.step,
                        };
                        let next = (now + step).clamp(step_options.min, step_options.max);
                        input.update(cx, |input, cx| {
                            input.set_value(next.to_string(), window, cx)
                        });
                        state.shown = next;
                        step_set(next, cx);
                    },
                ),
                // Typed: a number is kept within the limits.
                cx.subscribe_in(
                    &input,
                    window,
                    move |state: &mut NumberState, input, event: &InputEvent, window, cx| {
                        let InputEvent::Change = event else {
                            return;
                        };
                        let typed = input.read(cx).value();
                        if typed == state.shown.to_string() {
                            return;
                        }
                        let Ok(number) = typed.parse::<f64>() else {
                            return;
                        };
                        let kept = number.clamp(options.min, options.max);
                        state.shown = kept;
                        set(kept, cx);
                        if kept != number {
                            input.update(cx, |input, cx| {
                                input.set_value(kept.to_string(), window, cx)
                            });
                        }
                    },
                ),
            ];
            NumberState {
                input,
                shown: value,
                _subscriptions: subscriptions,
            }
        });
        // A change made elsewhere (a reset) shows here.
        state.update(cx, |state, cx| {
            if state.shown != value {
                state.shown = value;
                state.input.update(cx, |input, cx| {
                    input.set_value(value.to_string(), window, cx)
                });
            }
        });
        let input = state.read(cx).input.clone();
        NumberInput::new(&input)
            .map(|input| {
                if wide {
                    input.w_full()
                } else {
                    input.w(m.dp(128.))
                }
            })
            .into_any_element()
    }
}

struct TextState {
    input: Entity<InputState>,
    _subscription: Subscription,
}

struct NumberState {
    input: Entity<InputState>,
    /// The value shown, to tell a change made here from one made elsewhere.
    shown: f64,
    _subscriptions: Vec<Subscription>,
}

/// One setting: a label (and a hint under it) with its control, or a whole-width element.
#[derive(Clone)]
pub(crate) enum SettingItem {
    Item {
        title: SharedString,
        description: Option<SharedString>,
        keywords: Vec<SharedString>,
        /// The control under the label, as wide as the page.
        vertical: bool,
        field: Rc<dyn AnyField>,
    },
    Element(Render),
}

impl SettingItem {
    pub(crate) fn new<T: FieldValue>(
        title: impl Into<SharedString>,
        field: SettingField<T>,
    ) -> Self {
        SettingItem::Item {
            title: title.into(),
            description: None,
            keywords: Vec::new(),
            vertical: false,
            field: Rc::new(field),
        }
    }

    /// An element as wide as the page (a preview).
    pub(crate) fn render<E: IntoElement + 'static>(
        render: impl Fn(&mut Window, &mut App) -> E + 'static,
    ) -> Self {
        SettingItem::Element(Rc::new(move |window, cx| {
            render(window, cx).into_any_element()
        }))
    }

    pub(crate) fn description(mut self, text: impl Into<SharedString>) -> Self {
        if let SettingItem::Item { description, .. } = &mut self {
            *description = Some(text.into());
        }
        self
    }

    /// The control under the label, as wide as the page (a command line).
    pub(crate) fn vertical(mut self) -> Self {
        if let SettingItem::Item { vertical, .. } = &mut self {
            *vertical = true;
        }
        self
    }

    /// More words a search finds it by (a color's key, a command's name).
    pub(crate) fn keywords<S: Into<SharedString>>(
        mut self,
        words: impl IntoIterator<Item = S>,
    ) -> Self {
        if let SettingItem::Item { keywords, .. } = &mut self {
            keywords.extend(words.into_iter().map(Into::into));
        }
        self
    }

    /// Whether a search for `query` (lowercase) finds it: its label, hint or keywords.
    fn matches(&self, query: &str) -> bool {
        match self {
            SettingItem::Item {
                title,
                description,
                keywords,
                ..
            } => std::iter::once(title)
                .chain(description)
                .chain(keywords)
                .any(|text| text.to_lowercase().contains(query)),
            SettingItem::Element(_) => false,
        }
    }

    fn is_changed(&self, cx: &App) -> bool {
        match self {
            SettingItem::Item { field, .. } => field.is_changed(cx),
            SettingItem::Element(_) => false,
        }
    }
}

/// Settings shown together, under an optional title.
#[derive(Clone, Default)]
pub(crate) struct SettingGroup {
    title: Option<SharedString>,
    description: Option<SharedString>,
    items: Vec<SettingItem>,
}

impl SettingGroup {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn title(mut self, title: impl Into<SharedString>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub(crate) fn description(mut self, text: impl Into<SharedString>) -> Self {
        self.description = Some(text.into());
        self
    }

    pub(crate) fn item(mut self, item: SettingItem) -> Self {
        self.items.push(item);
        self
    }

    pub(crate) fn items(mut self, items: impl IntoIterator<Item = SettingItem>) -> Self {
        self.items.extend(items);
        self
    }
}

/// A page: its groups, and the pages under it in the sidebar.
#[derive(Clone)]
pub(crate) struct SettingPage {
    title: SharedString,
    icon: Option<AppIcon>,
    description: Option<SharedString>,
    groups: Vec<SettingGroup>,
    children: Vec<SettingPage>,
}

impl SettingPage {
    pub(crate) fn new(title: impl Into<SharedString>) -> Self {
        Self {
            title: title.into(),
            icon: None,
            description: None,
            groups: Vec::new(),
            children: Vec::new(),
        }
    }

    pub(crate) fn icon(mut self, icon: AppIcon) -> Self {
        self.icon = Some(icon);
        self
    }

    pub(crate) fn description(mut self, text: impl Into<SharedString>) -> Self {
        self.description = Some(text.into());
        self
    }

    pub(crate) fn group(mut self, group: SettingGroup) -> Self {
        self.groups.push(group);
        self
    }

    /// A page under this one in the sidebar.
    pub(crate) fn child(mut self, page: SettingPage) -> Self {
        self.children.push(page);
        self
    }

    fn is_changed(&self, cx: &App) -> bool {
        self.groups
            .iter()
            .flat_map(|group| &group.items)
            .any(|item| item.is_changed(cx))
    }

    fn reset(&self, window: &mut Window, cx: &mut App) {
        for item in self.groups.iter().flat_map(|group| &group.items) {
            if let SettingItem::Item { field, .. } = item
                && field.is_changed(cx)
            {
                field.reset(window, cx);
            }
        }
    }
}

/// The page at `path` (indexes from the top level down).
fn page_at<'a>(pages: &'a [SettingPage], path: &[usize]) -> Option<&'a SettingPage> {
    let (&first, rest) = path.split_first()?;
    let page = pages.get(first)?;
    if rest.is_empty() {
        Some(page)
    } else {
        page_at(&page.children, rest)
    }
}

/// Where a click on the page at `path` goes: the page itself, or, for one that only holds
/// pages (Appearance), its first page with settings.
pub(crate) fn shown_path(pages: &[SettingPage], path: &[usize]) -> Vec<usize> {
    let mut path = path.to_vec();
    while let Some(page) = page_at(pages, &path) {
        if !page.groups.is_empty() || page.children.is_empty() {
            break;
        }
        path.push(0);
    }
    path
}

/// The sidebar's selection and search, kept by the window.
#[derive(Clone)]
pub(crate) struct SettingsNav {
    /// The page shown, as indexes from the top level down.
    pub selected: Vec<usize>,
    pub search: Entity<InputState>,
    pub scroll: ScrollHandle,
}

impl SettingsNav {
    pub(crate) fn new(window: &mut Window, cx: &mut App) -> Self {
        let search = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("settings.search_placeholder").into_owned())
        });
        Self {
            selected: vec![0],
            search,
            scroll: ScrollHandle::new(),
        }
    }

    /// Shows the page at `path` from its top.
    pub(crate) fn select(&mut self, pages: &[SettingPage], path: &[usize]) {
        self.selected = shown_path(pages, path);
        self.scroll.set_offset(point(px(0.), px(0.)));
    }
}

/// The sidebar and the page shown (or a search's findings). `on_select(path)` is called
/// with the page clicked. An element drawn after the window's own render, as the fields
/// read the window (the Colors page's mode).
#[derive(IntoElement)]
pub(crate) struct SettingsLayout {
    pub pages: Rc<Vec<SettingPage>>,
    pub nav: SettingsNav,
    pub sidebar_width: Pixels,
    pub on_select: OnSelect,
}

impl RenderOnce for SettingsLayout {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        render_settings(
            &self.pages,
            &self.nav,
            self.sidebar_width,
            self.on_select,
            window,
            cx,
        )
    }
}

fn render_settings(
    pages: &[SettingPage],
    nav: &SettingsNav,
    sidebar_width: Pixels,
    on_select: OnSelect,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let (p, m) = (*cx.palette(), *cx.metrics());
    let query = nav.search.read(cx).value().trim().to_lowercase();
    let mut entries = Vec::new();
    sidebar_entries(pages, &[], &nav.selected, &on_select, &p, &m, &mut entries);
    let sidebar = v_flex()
        .w(sidebar_width)
        .flex_shrink_0()
        .h_full()
        .p(m.dp(8.))
        .gap(m.dp(2.))
        .border_r_1()
        .border_color(p.line)
        .child(
            div()
                .debug_selector(|| "settings-search".into())
                .pb(m.dp(6.))
                .child(
                    Input::new(&nav.search)
                        .prefix(Icon::new(AppIcon::Search))
                        .small(),
                ),
        )
        .child(
            v_flex()
                .id("settings-nav")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .gap(m.dp(2.))
                .children(entries),
        );
    let content = if query.is_empty() {
        match page_at(pages, &nav.selected) {
            Some(page) => {
                render_page(page, &nav.selected, &nav.scroll, &p, &m, window, cx).into_any_element()
            }
            None => div().into_any_element(),
        }
    } else {
        render_search(pages, &query, &nav.scroll, &p, &m, window, cx).into_any_element()
    };
    h_flex()
        .size_full()
        .child(sidebar)
        .child(
            div()
                .id("settings-page")
                .debug_selector(|| "settings-page".into())
                .flex_1()
                .min_w_0()
                .h_full()
                .overflow_y_scroll()
                .track_scroll(&nav.scroll)
                .child(content),
        )
        .into_any_element()
}

/// The sidebar's rows for `pages` (at `path`), each page's own pages under it while it
/// is selected or holds the selected one.
fn sidebar_entries(
    pages: &[SettingPage],
    path: &[usize],
    selected: &[usize],
    on_select: &OnSelect,
    p: &Palette,
    m: &Metrics,
    entries: &mut Vec<AnyElement>,
) {
    for (ix, page) in pages.iter().enumerate() {
        let mut here = path.to_vec();
        here.push(ix);
        let on_path = selected.starts_with(&here);
        let is_selected = selected == here.as_slice();
        let depth = here.len() - 1;
        let name = SharedString::from(format!("settings-nav-{}", page.title));
        let (select, clicked) = (on_select.clone(), here.clone());
        entries.push(
            h_flex()
                .id(ElementId::Name(name.clone()))
                .debug_selector(move || name.to_string())
                .h(m.sidebar_row)
                .flex_shrink_0()
                .pl(m.dp(8. + 14. * depth as f32))
                .pr(m.dp(8.))
                .gap(m.dp(8.))
                .rounded(m.radius)
                .cursor_pointer()
                .text_color(if is_selected { p.fg } else { p.fg2 })
                .when(is_selected, |row| row.bg(p.sel))
                .when(!is_selected, |row| row.hover(|style| style.bg(p.hover)))
                .when_some(page.icon, |row, icon| {
                    row.child(Icon::new(icon).text_color(if is_selected { p.acc } else { p.fg3 }))
                })
                .child(truncated("title", page.title.clone()).flex_1())
                .when(!page.children.is_empty(), |row| {
                    let chevron = if on_path {
                        AppIcon::ChevronDown
                    } else {
                        AppIcon::ChevronRight
                    };
                    row.child(Icon::new(chevron).small().text_color(p.fg3))
                })
                .on_click(move |_, window, cx| select(clicked.clone(), window, cx))
                .into_any_element(),
        );
        if on_path {
            sidebar_entries(&page.children, &here, selected, on_select, p, m, entries);
        }
    }
}

/// A page: its title (with a reset while anything on it differs from the default), its
/// hint, and its groups.
fn render_page(
    page: &SettingPage,
    path: &[usize],
    scroll: &ScrollHandle,
    p: &Palette,
    m: &Metrics,
    window: &mut Window,
    cx: &mut App,
) -> impl IntoElement {
    let key = path_key(path);
    let groups: Vec<AnyElement> = page
        .groups
        .iter()
        .enumerate()
        .map(|(ix, group)| {
            let items: Vec<&SettingItem> = group.items.iter().collect();
            render_group(
                group,
                &items,
                &format!("{key}-{ix}"),
                scroll,
                p,
                m,
                window,
                cx,
            )
            .into_any_element()
        })
        .collect();
    let reset = page.is_changed(cx).then(|| {
        let page = page.clone();
        Button::new("settings-reset-page")
            .icon(Icon::new(AppIcon::Undo))
            .ghost()
            .small()
            .tooltip(t!("settings.reset_page").into_owned())
            .on_click(move |_, window, cx| page.reset(window, cx))
    });
    let title = page.title.clone();
    v_flex()
        .p(m.dp(20.))
        .gap(m.dp(16.))
        .child(
            v_flex()
                .gap(m.dp(4.))
                .child(
                    h_flex()
                        .gap(m.dp(8.))
                        .child(
                            div()
                                .debug_selector(move || format!("settings-page-{title}"))
                                .text_size(m.dp(18.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(page.title.clone()),
                        )
                        .child(div().flex_1())
                        .children(reset),
                )
                .when_some(page.description.clone(), |this, text| {
                    this.child(
                        div()
                            .text_size(m.text.small)
                            .text_color(p.fg3)
                            .whitespace_normal()
                            .child(text),
                    )
                }),
        )
        .children(groups)
}

/// Every item a search for `query` finds, under its page's path and its group.
fn render_search(
    pages: &[SettingPage],
    query: &str,
    scroll: &ScrollHandle,
    p: &Palette,
    m: &Metrics,
    window: &mut Window,
    cx: &mut App,
) -> impl IntoElement {
    let mut found = Vec::new();
    search_pages(pages, &[], &[], query, scroll, p, m, window, cx, &mut found);
    let none = found.is_empty();
    v_flex()
        .p(m.dp(20.))
        .gap(m.dp(16.))
        .child(
            div()
                .text_size(m.dp(18.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(t!("settings.search_results").into_owned()),
        )
        .when(none, |this| {
            this.child(
                div()
                    .text_color(p.fg3)
                    .child(t!("settings.search_none").into_owned()),
            )
        })
        .children(found)
}

#[allow(clippy::too_many_arguments)]
fn search_pages(
    pages: &[SettingPage],
    path: &[usize],
    titles: &[SharedString],
    query: &str,
    scroll: &ScrollHandle,
    p: &Palette,
    m: &Metrics,
    window: &mut Window,
    cx: &mut App,
    found: &mut Vec<AnyElement>,
) {
    for (ix, page) in pages.iter().enumerate() {
        let mut here = path.to_vec();
        here.push(ix);
        let mut names = titles.to_vec();
        names.push(page.title.clone());
        for (group_ix, group) in page.groups.iter().enumerate() {
            let items: Vec<&SettingItem> = group
                .items
                .iter()
                .filter(|item| item.matches(query))
                .collect();
            if items.is_empty() {
                continue;
            }
            // Where it is: "Appearance › Colors › Surfaces".
            let place: Vec<&str> = names.iter().map(|name| name.as_ref()).collect();
            let place = place.join(" › ");
            let key = format!("{}-{group_ix}", path_key(&here));
            let shown = place.clone();
            found.push(
                v_flex()
                    .debug_selector(move || format!("settings-found-{shown}"))
                    .gap(m.dp(6.))
                    .child(div().text_size(m.text.xs).text_color(p.fg3).child(place))
                    .child(render_group(group, &items, &key, scroll, p, m, window, cx))
                    .into_any_element(),
            );
        }
        search_pages(
            &page.children,
            &here,
            &names,
            query,
            scroll,
            p,
            m,
            window,
            cx,
            found,
        );
    }
}

/// A group's title and hint, then its `items` in a box, one row each.
#[allow(clippy::too_many_arguments)]
fn render_group(
    group: &SettingGroup,
    items: &[&SettingItem],
    key: &str,
    scroll: &ScrollHandle,
    p: &Palette,
    m: &Metrics,
    window: &mut Window,
    cx: &mut App,
) -> impl IntoElement {
    let rows: Vec<AnyElement> = items
        .iter()
        .enumerate()
        .map(|(ix, item)| {
            let (focus, entered) = row_focus(format!("settings-row-{key}-{ix}"), window, cx);
            let title = match item {
                SettingItem::Item { title, .. } => Some(title.clone()),
                SettingItem::Element(_) => None,
            };
            let row = div()
                .w_full()
                .px(m.dp(14.))
                .py(m.dp(10.))
                .track_focus(&focus)
                .when_some(title, |row, title| {
                    row.debug_selector(move || format!("settings-row-{title}"))
                })
                .when(ix > 0, |row| row.border_t_1().border_color(p.line));
            let row = match item {
                SettingItem::Item {
                    title,
                    description,
                    field,
                    vertical,
                    ..
                } => {
                    let key = format!("field-{key}-{ix}").into();
                    let control = field.render(key, *vertical, window, cx);
                    let line = if *vertical {
                        v_flex().gap(m.dp(8.))
                    } else {
                        h_flex().gap(m.dp(16.)).justify_between()
                    };
                    row.child(
                        line.child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap(m.dp(2.))
                                .child(div().child(title.clone()))
                                .when_some(description.clone(), |this, text| {
                                    this.child(
                                        div()
                                            .text_size(m.text.small)
                                            .text_color(p.fg3)
                                            .whitespace_normal()
                                            .child(text),
                                    )
                                }),
                        )
                        .child(
                            div()
                                .when(*vertical, |this| this.w_full())
                                .when(!*vertical, |this| this.flex_shrink_0())
                                .child(control),
                        ),
                    )
                    .into_any_element()
                }
                SettingItem::Element(render) => row.child(render(window, cx)).into_any_element(),
            };
            let scroll = scroll.clone();
            div()
                .on_children_prepainted(move |bounds, window, _| {
                    if let (true, Some(row)) = (entered, bounds.first()) {
                        scroll_into_view(&scroll, *row, window);
                    }
                })
                .child(row)
                .into_any_element()
        })
        .collect();
    let title = group.title.clone();
    v_flex()
        .gap(m.dp(8.))
        .when_some(title, |this, title| {
            let shown = title.clone();
            this.child(
                div()
                    .debug_selector(move || format!("settings-group-{shown}"))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
        })
        .when_some(group.description.clone(), |this, text| {
            this.child(
                div()
                    .text_size(m.text.small)
                    .text_color(p.fg3)
                    .whitespace_normal()
                    .child(text),
            )
        })
        .child(
            v_flex()
                .border_1()
                .border_color(p.line)
                .rounded(m.radius_lg)
                .bg(p.panel2)
                .children(rows),
        )
}

/// A row's focus handle, and whether the focus has just come into the row. The handle is no
/// tab stop: it only tells whether the focused control is inside. GPUI doesn't scroll the
/// focus into view, so Tab could land on a control below the page's fold and Enter act on
/// something unseen. The row is scrolled to only on the way in, so the wheel stays
/// free while a control keeps the focus (a switch keeps it after a click).
fn row_focus(id: String, window: &mut Window, cx: &mut App) -> (FocusHandle, bool) {
    let state = window.use_keyed_state(ElementId::Name(id.into()), cx, |_, cx| {
        (cx.focus_handle(), false)
    });
    let (focus, was_inside) = state.read(cx).clone();
    let inside = focus.contains_focused(window, cx);
    if inside != was_inside {
        state.update(cx, |state, _| state.1 = inside);
    }
    (focus, inside && !was_inside)
}

/// A page's path as text, to name its fields' state.
fn path_key(path: &[usize]) -> String {
    path.iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(".")
}
