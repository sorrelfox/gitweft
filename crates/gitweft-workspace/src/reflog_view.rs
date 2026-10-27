//! Centre panel listing where a ref pointed, newest first (its reflog). Commits that only
//! the reflog still remembers, lost to a reset, an amend or a rebase, are marked, and a
//! row's menu recovers one as a branch or checks it out. The arrows move the cursor (the
//! selected entry, shown in the details), Shift+F10 shows its menu.

use std::collections::HashSet;
use std::ops::Range;

use gitweft_git::ObjectId;
use gitweft_git::read::ReflogEntry;
use gitweft_ui::current::mono_font;
use gitweft_ui::truncated::{Truncated, truncated};
use gitweft_ui::widgets::{centre_header, list_header, list_row, placeholder};
use gitweft_ui::{LookExt, Metrics, Palette};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::{Sizable, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::graph_view::{DEFAULT_DATE_WIDTH, MIN_MESSAGE_WIDTH, SHA_WIDTH};
use crate::list_keys::{self, FirstRow, LastRow, NextRow, OpenRow, PreviousRow, RowMenu, Step};
use crate::util::format_time;

/// The entry column (`HEAD@{12}`), in design pixels.
const ENTRY_WIDTH: f32 = 110.;
pub(crate) const CONTEXT: &str = "ReflogView";

actions!(reflog, [CloseReflog]);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", CloseReflog, Some(CONTEXT))]);
}

#[derive(Clone)]
pub enum ReflogEvent {
    Close,
    /// Show the commit's details in the right panel.
    Select(ObjectId),
    CreateBranch(ObjectId),
    Checkout(ObjectId),
    /// Move the checked-out branch to the entry's commit (as the graph's resets do).
    Reset(ObjectId, gitweft_git::rewrite::ResetMode),
    ShowInGraph(ObjectId),
    /// Look for the commits even the reflogs lost (`git fsck`).
    ShowDangling,
    /// Cherry-pick the entry's commit onto the checked-out branch (with its summary).
    CherryPick(ObjectId, String),
    /// The entry's commit's files.
    BrowseFiles(ObjectId),
}

impl EventEmitter<ReflogEvent> for ReflogView {}

impl Focusable for ReflogView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// A reflog and the commits in it that no ref reaches.
pub struct Reflog {
    pub entries: Vec<ReflogEntry>,
    pub lost: HashSet<ObjectId>,
}

enum Content {
    Loading,
    Loaded(Reflog),
    Failed(String),
}

/// The columns in this frame (zoomed): the entry, the action taking the room left, and
/// the date and SHA at the graph's widths.
#[derive(Clone, Copy)]
struct Columns {
    entry: Pixels,
    min_action: Pixels,
    date: Pixels,
    sha: Pixels,
}

impl Columns {
    fn new(m: &Metrics) -> Self {
        let z = |value: f32| px(value * m.zoom);
        Self {
            entry: z(ENTRY_WIDTH),
            min_action: z(MIN_MESSAGE_WIDTH),
            date: z(DEFAULT_DATE_WIDTH),
            sha: z(SHA_WIDTH),
        }
    }

    /// A column that keeps its width; its text is cut to fit, whole on hover.
    fn fixed(id: &'static str, width: Pixels, text: impl Into<SharedString>) -> Truncated {
        truncated(id, text).w(width).flex_shrink_0()
    }

    /// Entry, Action, Date, SHA.
    fn header(self, p: &Palette, m: &Metrics) -> Div {
        list_header(p, m)
            .child(Self::fixed(
                "entry",
                self.entry,
                t!("reflog.column_entry").into_owned(),
            ))
            .child(
                truncated("column-action", t!("reflog.column_action").into_owned())
                    .flex_1()
                    .min_w(self.min_action),
            )
            .child(Self::fixed(
                "date",
                self.date,
                t!("graph.column.date").into_owned(),
            ))
            .child(Self::fixed(
                "sha",
                self.sha,
                t!("graph.column.sha").into_owned(),
            ))
    }
}

/// "lost": a commit only the reflog remembers, as a pill in the modified color.
fn lost_pill(p: &Palette, m: &Metrics) -> Div {
    let z = |value: f32| px(value * m.zoom);
    div()
        .flex_shrink_0()
        .h(m.pill)
        .px(z(6.))
        .flex()
        .items_center()
        .rounded(z(5.))
        .bg(p.modified.opacity(0.16))
        .text_color(p.modified)
        .font_family(mono_font())
        .text_size(m.text.mono_small)
        .child(t!("reflog.lost").into_owned())
}

pub struct ReflogView {
    /// The ref as shown (`HEAD`, `main`); also the prefix of the `@{n}` selectors.
    name: String,
    content: Content,
    only_lost: bool,
    /// Entry indices of the shown rows.
    rows: Vec<usize>,
    /// The entry clicked last.
    selected: Option<usize>,
    scroll: UniformListScrollHandle,
    focus: FocusHandle,
    /// Take keyboard focus on the next frame, so Esc works.
    focus_on_render: bool,
}

impl ReflogView {
    /// Whether its rows are read (the headless UI tests wait for it before keys).
    #[cfg(test)]
    pub(crate) fn is_loaded(&self) -> bool {
        matches!(self.content, Content::Loaded(_))
    }

    pub fn new(name: String, cx: &mut Context<Self>) -> Self {
        Self {
            name,
            content: Content::Loading,
            only_lost: false,
            rows: Vec::new(),
            selected: None,
            scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
            focus_on_render: true,
        }
    }

    pub fn set_result(&mut self, result: Result<Reflog, String>, cx: &mut Context<Self>) {
        self.content = match result {
            Ok(reflog) => Content::Loaded(reflog),
            Err(message) => Content::Failed(message),
        };
        // A reload shifts the entries (new ones come first), so the clicked one is gone.
        self.selected = None;
        self.rebuild_rows();
        cx.notify();
    }

    /// Takes the keys again once a view shown on top of it closes.
    pub fn refocus(&mut self, cx: &mut Context<Self>) {
        self.focus_on_render = true;
        cx.notify();
    }

    fn rebuild_rows(&mut self) {
        self.rows = match &self.content {
            Content::Loaded(reflog) => (0..reflog.entries.len())
                .filter(|&ix| !self.only_lost || reflog.lost.contains(&reflog.entries[ix].id))
                .collect(),
            Content::Loading | Content::Failed(_) => Vec::new(),
        };
    }

    fn close(&mut self, _: &CloseReflog, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(ReflogEvent::Close);
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Content::Loaded(reflog) = &self.content else {
            return;
        };
        cx.emit(ReflogEvent::Select(reflog.entries[ix].id));
        self.selected = Some(ix);
        cx.notify();
    }

    /// The arrows move the cursor through the rows shown (each one shown in the details,
    /// as a click does) and keep it in view.
    fn move_cursor(&mut self, to: Step, cx: &mut Context<Self>) {
        let current = self
            .selected
            .and_then(|ix| self.rows.iter().position(|&row| row == ix));
        let Some(row) = list_keys::step(self.rows.len(), current, to) else {
            return;
        };
        self.scroll.scroll_to_item(row, ScrollStrategy::Nearest);
        self.select(self.rows[row], cx);
    }

    /// An entry's menu, for its right-click menu and Shift+F10.
    fn row_items(id: ObjectId, summary: &str) -> Vec<(String, ReflogEvent)> {
        use gitweft_git::rewrite::ResetMode;
        let mut items = vec![
            (
                t!("branch.new_here").into_owned(),
                ReflogEvent::CreateBranch(id),
            ),
            (
                t!("branch.checkout_detached").into_owned(),
                ReflogEvent::Checkout(id),
            ),
        ];
        // The branch back (or on) to where this entry had it: the graph's three resets.
        for (mode, label) in [
            (ResetMode::Soft, t!("palette.reset_soft")),
            (ResetMode::Mixed, t!("palette.reset_mixed")),
            (ResetMode::Hard, t!("palette.reset_hard")),
        ] {
            items.push((label.into_owned(), ReflogEvent::Reset(id, mode)));
        }
        items.push((
            t!("history.show_in_graph").into_owned(),
            ReflogEvent::ShowInGraph(id),
        ));
        // As the dangling commits' rows have them.
        items.push((
            t!("rewrite.cherry_pick").into_owned(),
            ReflogEvent::CherryPick(id, summary.to_owned()),
        ));
        items.push((t!("tree.browse").into_owned(), ReflogEvent::BrowseFiles(id)));
        items
    }

    /// Shift+F10: the cursor's entry menu in the palette.
    fn row_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Content::Loaded(reflog), Some(ix)) = (&self.content, self.selected) else {
            return;
        };
        let entry = &reflog.entries[ix];
        let detail = format!("{}@{{{}}} {}", self.name, entry.index, entry.action);
        let commands = list_keys::event_commands(
            &cx.entity().downgrade(),
            &t!("palette.group.reflog"),
            &detail,
            Self::row_items(entry.id, &entry.summary),
        );
        crate::palette::open(commands, window, cx);
    }

    fn render_rows(
        &mut self,
        range: Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Content::Loaded(reflog) = &self.content else {
            return Vec::new();
        };
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);
        let columns = Columns::new(&m);
        let mono = mono_font();
        let view = cx.entity().downgrade();
        range
            .map(|row| {
                let ix = self.rows[row];
                let entry = &reflog.entries[ix];
                let lost = reflog.lost.contains(&entry.id);
                let id = entry.id;
                let row_element = list_row(("reflog-row", ix), self.selected == Some(ix), &p, &m)
                    .on_click(cx.listener(move |this, _, _, cx| this.select(ix, cx)))
                    .child(
                        Columns::fixed(
                            "entry",
                            columns.entry,
                            format!("{}@{{{}}}", self.name, entry.index),
                        )
                        .font_family(mono.clone())
                        .text_size(m.text.mono)
                        .text_color(p.fg2),
                    )
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w(columns.min_action)
                            .overflow_hidden()
                            .gap(px(8. * m.zoom))
                            .child(truncated("action", entry.action.clone()).text_color(p.fg))
                            .when(lost, |this| this.child(lost_pill(&p, &m))),
                    )
                    .child(
                        Columns::fixed("date", columns.date, format_time(entry.time))
                            .font_family(mono.clone())
                            .text_size(m.text.mono)
                            .text_color(p.fg2),
                    )
                    .child(
                        Columns::fixed("sha", columns.sha, id.to_hex_with_len(8).to_string())
                            .font_family(mono.clone())
                            .text_size(m.text.mono)
                            .text_color(p.fg3),
                    );
                let (view, items) = (view.clone(), Self::row_items(id, &entry.summary));
                row_element
                    .context_menu(move |menu, _, _| list_keys::event_menu(menu, &view, &items))
                    .into_any_element()
            })
            .collect()
    }
}

impl Render for ReflogView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.focus_on_render) {
            self.focus.focus(window, cx);
        }
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);

        let count = match &self.content {
            Content::Loaded(_) => tn!("reflog.count", count = self.rows.len()).into_owned(),
            Content::Loading | Content::Failed(_) => String::new(),
        };
        let title = t!("reflog.title", name = self.name.clone()).into_owned();
        let header = centre_header(title, count, &p, &m)
            .child(
                Checkbox::new("only-lost")
                    .small()
                    .label(t!("reflog.only_lost").into_owned())
                    .checked(self.only_lost)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.only_lost = *checked;
                        this.rebuild_rows();
                        cx.notify();
                    })),
            )
            .child(
                Button::new("reflog-dangling")
                    .small()
                    .ghost()
                    .label(t!("dangling.title").into_owned())
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(ReflogEvent::ShowDangling))),
            )
            .child(
                Button::new("close-reflog")
                    .small()
                    .ghost()
                    .label(t!("diff.close").into_owned())
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(ReflogEvent::Close))),
            );

        let body = match &self.content {
            Content::Loading => placeholder(t!("repo.loading").into_owned(), p.fg3, &m),
            Content::Failed(message) => placeholder(message.clone(), p.del, &m),
            Content::Loaded(_) if self.rows.is_empty() => {
                placeholder(t!("reflog.empty").into_owned(), p.fg3, &m)
            }
            Content::Loaded(_) => v_flex()
                .flex_1()
                .min_h_0()
                .child(Columns::new(&m).header(&p, &m))
                .child(
                    uniform_list(
                        "reflog-rows",
                        self.rows.len(),
                        cx.processor(Self::render_rows),
                    )
                    .track_scroll(&self.scroll)
                    .flex_1(),
                ),
        };

        v_flex()
            .size_full()
            .text_size(m.text.ui)
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::close))
            .on_action(cx.listener(|this, _: &NextRow, _, cx| this.move_cursor(Step::Next, cx)))
            .on_action(
                cx.listener(|this, _: &PreviousRow, _, cx| this.move_cursor(Step::Previous, cx)),
            )
            .on_action(cx.listener(|this, _: &FirstRow, _, cx| this.move_cursor(Step::First, cx)))
            .on_action(cx.listener(|this, _: &LastRow, _, cx| this.move_cursor(Step::Last, cx)))
            .on_action(cx.listener(|this, _: &OpenRow, _, cx| {
                if let Some(ix) = this.selected {
                    this.select(ix, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &RowMenu, window, cx| this.row_menu(window, cx)))
            .child(header)
            .child(body)
    }
}
