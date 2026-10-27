//! Centre panel listing the dangling commits `git fsck` finds: the tips of lost history
//! that no branch, tag or reflog reaches any more (a reset or an amend whose reflog
//! entries expired, a dropped stash). A row shows the commit's details; its menu
//! recovers it. The search reads every object, so it can take a while: it can be stopped.

use std::ops::Range;

use gitweft_git::ObjectId;
use gitweft_git::read::DanglingCommit;
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

pub(crate) const CONTEXT: &str = "DanglingView";

actions!(dangling, [CloseDangling]);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", CloseDangling, Some(CONTEXT))]);
}

#[derive(Clone)]
pub enum DanglingEvent {
    Close,
    /// Stop the search.
    Cancel,
    /// Search again (a recovered commit stays listed until then: fsck is too slow to run
    /// after every change).
    Refresh,
    /// Show the commit's details in the right panel.
    Select(ObjectId),
    CreateBranch(ObjectId),
    Checkout(ObjectId),
    /// Cherry-pick the commit onto the checked-out branch (with its summary).
    CherryPick(ObjectId, String),
    /// Apply a dropped stash's changes to the working tree.
    ApplyStash(ObjectId),
    BrowseFiles(ObjectId),
}

impl EventEmitter<DanglingEvent> for DanglingView {}

impl Focusable for DanglingView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

enum Content {
    Loading,
    /// The newest commits, and how many were found in all.
    Loaded {
        commits: Vec<DanglingCommit>,
        total: usize,
    },
    Failed(String),
}

pub struct DanglingView {
    content: Content,
    /// The row clicked last.
    selected: Option<usize>,
    scroll: UniformListScrollHandle,
    focus: FocusHandle,
    /// Take keyboard focus on the next frame, so Esc works.
    focus_on_render: bool,
    /// "Show all" was chosen: the next search lists every commit it finds.
    show_all: bool,
}

impl DanglingView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            content: Content::Loading,
            selected: None,
            scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
            focus_on_render: true,
            show_all: false,
        }
    }

    /// Whether every commit found is to be listed, not the newest few hundred.
    pub(crate) fn shows_all(&self) -> bool {
        self.show_all
    }

    pub fn set_result(
        &mut self,
        result: Result<(Vec<DanglingCommit>, usize), String>,
        cx: &mut Context<Self>,
    ) {
        self.content = match result {
            Ok((commits, total)) => Content::Loaded { commits, total },
            Err(message) => Content::Failed(message),
        };
        self.selected = None;
        cx.notify();
    }

    pub fn is_loading(&self) -> bool {
        matches!(self.content, Content::Loading)
    }

    /// A new search started.
    pub fn set_loading(&mut self, cx: &mut Context<Self>) {
        self.content = Content::Loading;
        self.selected = None;
        cx.notify();
    }

    /// Takes the keys again once a view shown on top of it closes.
    pub fn refocus(&mut self, cx: &mut Context<Self>) {
        self.focus_on_render = true;
        cx.notify();
    }

    fn close(&mut self, _: &CloseDangling, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DanglingEvent::Close);
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Content::Loaded { commits, .. } = &self.content else {
            return;
        };
        cx.emit(DanglingEvent::Select(commits[ix].id));
        self.selected = Some(ix);
        cx.notify();
    }

    /// The arrows move the cursor through the commits (each one shown in the details, as a
    /// click does) and keep it in view.
    fn move_cursor(&mut self, to: Step, cx: &mut Context<Self>) {
        let Content::Loaded { commits, .. } = &self.content else {
            return;
        };
        let Some(ix) = list_keys::step(commits.len(), self.selected, to) else {
            return;
        };
        self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        self.select(ix, cx);
    }

    /// A commit's menu, for its right-click menu and Shift+F10. A dropped stash is a
    /// merge of the work and the index: applied as a stash, not picked.
    fn row_items(commit: &DanglingCommit) -> Vec<(String, DanglingEvent)> {
        let id = commit.id;
        let recover = if commit.stash {
            (
                t!("dangling.apply_stash").into_owned(),
                DanglingEvent::ApplyStash(id),
            )
        } else {
            (
                t!("rewrite.cherry_pick").into_owned(),
                DanglingEvent::CherryPick(id, commit.summary.clone()),
            )
        };
        vec![
            (
                t!("branch.new_here").into_owned(),
                DanglingEvent::CreateBranch(id),
            ),
            (
                t!("branch.checkout_detached").into_owned(),
                DanglingEvent::Checkout(id),
            ),
            recover,
            (
                t!("tree.browse").into_owned(),
                DanglingEvent::BrowseFiles(id),
            ),
        ]
    }

    /// Shift+F10: the cursor's commit menu in the palette.
    fn row_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Content::Loaded { commits, .. }, Some(ix)) = (&self.content, self.selected) else {
            return;
        };
        let Some(commit) = commits.get(ix) else {
            return;
        };
        let detail = format!("{} {}", commit.id.to_hex_with_len(8), commit.summary);
        let commands = list_keys::event_commands(
            &cx.entity().downgrade(),
            &t!("palette.group.dangling"),
            &detail,
            Self::row_items(commit),
        );
        crate::palette::open(commands, window, cx);
    }

    fn render_rows(
        &mut self,
        range: Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Content::Loaded { commits, .. } = &self.content else {
            return Vec::new();
        };
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);
        let view = cx.entity().downgrade();
        range
            .map(|ix| {
                let commit = &commits[ix];
                let cells = commit_columns::cells(
                    CommitRow {
                        summary: commit.summary.clone(),
                        note: commit.stash.then(|| t!("dangling.stash_note").into_owned()),
                        author: commit.author.name.clone(),
                        date: format_time(commit.time),
                        id: commit.id,
                    },
                    &p,
                    &m,
                );
                let (view, items) = (view.clone(), Self::row_items(commit));
                list_row(("dangling-row", ix), self.selected == Some(ix), &p, &m)
                    .on_click(cx.listener(move |this, _, _, cx| this.select(ix, cx)))
                    .children(cells)
                    .context_menu(move |menu, _, _| list_keys::event_menu(menu, &view, &items))
                    .into_any_element()
            })
            .collect()
    }
}

impl Render for DanglingView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.focus_on_render) {
            self.focus.focus(window, cx);
        }
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);

        let count = match &self.content {
            Content::Loaded { commits, total } if commits.len() < *total => tn!(
                "dangling.count_capped",
                count = *total,
                shown = commits.len()
            )
            .into_owned(),
            Content::Loaded { total, .. } => tn!("dangling.count", count = *total).into_owned(),
            Content::Loading => t!("dangling.searching").into_owned(),
            Content::Failed(_) => String::new(),
        };
        let loading = self.is_loading();
        let capped =
            matches!(&self.content, Content::Loaded { commits, total } if commits.len() < *total);
        let header = centre_header(t!("dangling.title").into_owned(), count, &p, &m)
            // The rest of a capped list: the search again, uncapped.
            .when(capped, |this| {
                this.child(
                    Button::new("show-all-dangling")
                        .small()
                        .ghost()
                        .label(t!("list.show_all").into_owned())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_all = true;
                            cx.emit(DanglingEvent::Refresh);
                        })),
                )
            })
            .when(loading, |this| {
                this.child(
                    Button::new("cancel-dangling")
                        .small()
                        .ghost()
                        .label(t!("toolbar.cancel").into_owned())
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(DanglingEvent::Cancel))),
                )
            })
            .when(!loading, |this| {
                this.child(
                    Button::new("refresh-dangling")
                        .small()
                        .ghost()
                        .label(t!("dangling.search_again").into_owned())
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(DanglingEvent::Refresh))),
                )
            })
            .child(
                Button::new("close-dangling")
                    .small()
                    .ghost()
                    .label(t!("diff.close").into_owned())
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DanglingEvent::Close))),
            );

        let body = match &self.content {
            Content::Loading => placeholder(t!("repo.loading").into_owned(), p.fg3, &m),
            Content::Failed(message) => placeholder(message.clone(), p.del, &m),
            Content::Loaded { commits, .. } if commits.is_empty() => {
                placeholder(t!("dangling.empty").into_owned(), p.fg3, &m)
            }
            Content::Loaded { commits, .. } => v_flex()
                .flex_1()
                .min_h_0()
                .child(commit_columns::header(&p, &m))
                .child(
                    uniform_list(
                        "dangling-rows",
                        commits.len(),
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
