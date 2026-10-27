//! Centre panel listing the commits that changed one file, newest first, following
//! renames. Clicking a revision opens the file's diff in that commit; the diff shows on
//! top, so closing it comes back to this list. The rows have the graph's columns. The
//! arrows move the cursor (the selected row), Enter opens its diff, Shift+F10 its menu.

use std::ops::Range;

use gitweft_git::ObjectId;
use gitweft_git::read::{ChangeKind, FileRevision};
use gitweft_ui::LookExt;
use gitweft_ui::widgets::{centre_header, list_row, placeholder};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::{Sizable, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::commit_columns::{self, CommitRow};
use crate::list_keys::{self, FirstRow, LastRow, NextRow, OpenRow, PreviousRow, RowMenu, Step};
use crate::util::format_time;

pub(crate) const CONTEXT: &str = "HistoryView";

actions!(history, [CloseHistory]);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", CloseHistory, Some(CONTEXT))]);
}

#[derive(Clone)]
pub enum HistoryEvent {
    Close,
    /// Show the file's diff in this revision.
    OpenDiff(FileRevision),
    /// Blame the file as of this revision.
    Blame(FileRevision),
    /// Select the commit in the graph.
    ShowCommit(ObjectId),
    /// The revision's change to the file in the user's diff tool.
    DiffTool(FileRevision),
    /// The file as it is in this revision, in the file view (#44).
    OpenFile(FileRevision),
    /// The revision's files (#44).
    BrowseFiles(ObjectId),
}

impl EventEmitter<HistoryEvent> for HistoryView {}

impl Focusable for HistoryView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

enum Content {
    Loading,
    Loaded(Vec<FileRevision>),
    Failed(String),
}

pub struct HistoryView {
    title: String,
    subtitle: String,
    content: Content,
    /// The revision whose diff was opened last.
    selected: Option<usize>,
    scroll: UniformListScrollHandle,
    focus: FocusHandle,
    /// Take keyboard focus on the next frame, so Esc works.
    focus_on_render: bool,
}

impl HistoryView {
    /// Whether its rows are read (the headless UI tests wait for it before keys).
    #[cfg(test)]
    pub(crate) fn is_loaded(&self) -> bool {
        matches!(self.content, Content::Loaded(_))
    }

    pub fn new(title: String, subtitle: String, cx: &mut Context<Self>) -> Self {
        Self {
            title,
            subtitle,
            content: Content::Loading,
            selected: None,
            scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
            focus_on_render: true,
        }
    }

    pub fn set_result(
        &mut self,
        result: Result<Vec<FileRevision>, String>,
        cx: &mut Context<Self>,
    ) {
        self.content = match result {
            Ok(revisions) => Content::Loaded(revisions),
            Err(message) => Content::Failed(message),
        };
        cx.notify();
    }

    /// Takes the keys again once a view shown on top of it closes.
    pub fn refocus(&mut self, cx: &mut Context<Self>) {
        self.focus_on_render = true;
        cx.notify();
    }

    fn close(&mut self, _: &CloseHistory, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(HistoryEvent::Close);
    }

    fn open(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Content::Loaded(revisions) = &self.content else {
            return;
        };
        let Some(revision) = revisions.get(ix) else {
            return;
        };
        cx.emit(HistoryEvent::OpenDiff(revision.clone()));
        self.selected = Some(ix);
        cx.notify();
    }

    fn revisions(&self) -> &[FileRevision] {
        match &self.content {
            Content::Loaded(revisions) => revisions,
            Content::Loading | Content::Failed(_) => &[],
        }
    }

    /// The arrows move the cursor (the selected row) and keep it in view; only Enter or a
    /// click opens the diff, which would cover the list.
    fn move_cursor(&mut self, to: Step, cx: &mut Context<Self>) {
        self.selected = list_keys::step(self.revisions().len(), self.selected, to);
        if let Some(ix) = self.selected {
            self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    /// A revision's menu, for its right-click menu and Shift+F10.
    fn row_items(revision: &FileRevision) -> Vec<(String, HistoryEvent)> {
        let mut items = Vec::new();
        // A commit that deleted the file has nothing to blame.
        if revision.kind != ChangeKind::Deleted {
            let blame = HistoryEvent::Blame(revision.clone());
            items.push((t!("history.blame_here").into_owned(), blame));
        }
        let show = HistoryEvent::ShowCommit(revision.id);
        items.push((t!("history.show_in_graph").into_owned(), show));
        let tool = HistoryEvent::DiffTool(revision.clone());
        items.push((t!("external.diff_tool").into_owned(), tool));
        if revision.kind != ChangeKind::Deleted {
            let file = HistoryEvent::OpenFile(revision.clone());
            items.push((t!("history.open_file_here").into_owned(), file));
        }
        let browse = HistoryEvent::BrowseFiles(revision.id);
        items.push((t!("tree.browse").into_owned(), browse));
        items
    }

    /// Shift+F10: the cursor's row menu in the palette.
    fn row_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(revision) = self.selected.and_then(|ix| self.revisions().get(ix)) else {
            return;
        };
        let detail = format!("{} {}", revision.id.to_hex_with_len(8), revision.summary);
        let commands = list_keys::event_commands(
            &cx.entity().downgrade(),
            &t!("palette.group.history"),
            &detail,
            Self::row_items(revision),
        );
        crate::palette::open(commands, window, cx);
    }

    fn render_rows(
        &mut self,
        range: Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Content::Loaded(revisions) = &self.content else {
            return Vec::new();
        };
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);
        let view = cx.entity().downgrade();
        range
            .map(|ix| {
                let revision = &revisions[ix];
                let renamed = revision
                    .old_path
                    .as_ref()
                    .map(|old| t!("history.renamed_from", path = old.to_string()).into_owned());
                let cells = commit_columns::cells(
                    CommitRow {
                        summary: revision.summary.clone(),
                        note: renamed,
                        author: revision.author.name.clone(),
                        date: format_time(revision.author.time),
                        id: revision.id,
                    },
                    &p,
                    &m,
                );
                let row = list_row(("history-row", ix), self.selected == Some(ix), &p, &m)
                    .on_click(cx.listener(move |this, _, _, cx| this.open(ix, cx)))
                    .children(cells);
                let (view, items) = (view.clone(), Self::row_items(revision));
                row.context_menu(move |menu, _, _| list_keys::event_menu(menu, &view, &items))
                    .into_any_element()
            })
            .collect()
    }
}

impl Render for HistoryView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.focus_on_render) {
            self.focus.focus(window, cx);
        }
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);

        let count = match &self.content {
            Content::Loaded(revisions) => {
                Some(tn!("history.count", count = revisions.len()).into_owned())
            }
            Content::Loading | Content::Failed(_) => None,
        };
        let header = centre_header(self.title.clone(), self.subtitle.clone(), &p, &m)
            .when_some(count, |this, count| {
                this.child(div().text_size(m.text.xs).text_color(p.fg3).child(count))
            })
            .child(
                Button::new("close-history")
                    .small()
                    .ghost()
                    .label(t!("diff.close").into_owned())
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(HistoryEvent::Close))),
            );

        let body = match &self.content {
            Content::Loading => placeholder(t!("repo.loading").into_owned(), p.fg3, &m),
            Content::Failed(message) => placeholder(message.clone(), p.del, &m),
            Content::Loaded(revisions) if revisions.is_empty() => {
                placeholder(t!("history.empty").into_owned(), p.fg3, &m)
            }
            Content::Loaded(revisions) => v_flex()
                .flex_1()
                .min_h_0()
                .child(commit_columns::header(&p, &m))
                .child(
                    uniform_list(
                        "history-rows",
                        revisions.len(),
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
                    this.open(ix, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &RowMenu, window, cx| this.row_menu(window, cx)))
            .child(header)
            .child(body)
    }
}
