//! Centre panel listing what Gitweft moved to its trash from this repository: each
//! discard, clean or hard reset with its files, newest first. A file (or a whole entry)
//! can be put back, and entries deleted for good.

use std::ops::Range;

use gitweft_core::trash::{TrashAction, TrashEntry, TrashFileKind};
use gitweft_git::gix::bstr::BString;
use gitweft_ui::LookExt;
use gitweft_ui::truncated::truncated;
use gitweft_ui::widgets::{centre_header, list_row, path_label, placeholder, status_badge};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::{Disableable, Sizable, StyledExt, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::list_keys::{
    self, DeleteRow, FirstRow, LastRow, NextRow, OpenRow, PreviousRow, RowMenu, Step,
};
use crate::staging_view::link;
use crate::util::format_time;

pub(crate) const CONTEXT: &str = "TrashView";
/// A file row's indent under its entry, in design px.
const FILE_INDENT: f32 = 16.;

actions!(trash, [CloseTrash]);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", CloseTrash, Some(CONTEXT))]);
}

#[derive(Clone)]
pub enum TrashEvent {
    Close,
    /// Put these files of the entry back into the working tree.
    Restore {
        entry: TrashEntry,
        paths: Vec<BString>,
    },
    /// Delete the entry for good (asks first).
    Delete(TrashEntry),
    /// Delete every entry of this repository (asks first).
    Empty(Vec<TrashEntry>),
    /// Delete every folder from before records (asks first).
    DeleteOlder(Vec<TrashEntry>),
    /// Show a folder from before records (its repository unknown) in the file manager.
    ShowFolder(std::path::PathBuf),
}

impl EventEmitter<TrashEvent> for TrashView {}

impl Focusable for TrashView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

enum Content {
    Loading,
    Loaded(Vec<TrashEntry>),
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    /// An entry: what put it there, when, its files and size.
    Entry(usize),
    /// File `.1` of entry `.0`.
    File(usize, usize),
}

pub struct TrashView {
    content: Content,
    rows: Vec<Row>,
    /// The row the keys act on (a click puts it there too).
    cursor: Option<usize>,
    scroll: UniformListScrollHandle,
    focus: FocusHandle,
    /// Take keyboard focus on the next frame, so Esc works.
    focus_on_render: bool,
}

/// A size as people read it: "812 B", "12 KB", "3.4 MB".
pub(crate) fn size_text(bytes: u64) -> String {
    const KB: f64 = 1024.;
    let value = bytes as f64;
    if value < KB {
        t!("trash.size_b", n = bytes).into_owned()
    } else if value < KB * KB {
        t!("trash.size_kb", n = format!("{:.0}", value / KB)).into_owned()
    } else {
        t!("trash.size_mb", n = format!("{:.1}", value / KB / KB)).into_owned()
    }
}

fn action_label(action: TrashAction) -> String {
    match action {
        TrashAction::Discard => t!("trash.action_discard"),
        TrashAction::Clean => t!("trash.action_clean"),
        TrashAction::Reset => t!("trash.action_reset"),
        TrashAction::Aside => t!("trash.action_aside"),
        _ => t!("trash.action_unknown"),
    }
    .into_owned()
}

impl TrashView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            content: Content::Loading,
            rows: Vec::new(),
            cursor: None,
            scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
            focus_on_render: true,
        }
    }

    pub fn set_result(&mut self, result: Result<Vec<TrashEntry>, String>, cx: &mut Context<Self>) {
        self.content = match result {
            Ok(entries) => Content::Loaded(entries),
            Err(message) => Content::Failed(message),
        };
        self.rows = match &self.content {
            Content::Loaded(entries) => entries
                .iter()
                .enumerate()
                .flat_map(|(ix, entry)| {
                    std::iter::once(Row::Entry(ix))
                        .chain((0..entry.files.len()).map(move |file| Row::File(ix, file)))
                })
                .collect(),
            Content::Loading | Content::Failed(_) => Vec::new(),
        };
        self.cursor = None;
        cx.notify();
    }

    /// Takes the keys again once a view shown on top of it closes.
    pub fn refocus(&mut self, cx: &mut Context<Self>) {
        self.focus_on_render = true;
        cx.notify();
    }

    fn close(&mut self, _: &CloseTrash, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(TrashEvent::Close);
    }

    fn entries(&self) -> &[TrashEntry] {
        match &self.content {
            Content::Loaded(entries) => entries,
            Content::Loading | Content::Failed(_) => &[],
        }
    }

    /// What a row offers, for its right-click menu, Shift+F10 and Enter (its first item,
    /// #27): an entry is restored whole (a folder from before records is shown in the file
    /// manager instead) or deleted; a file is restored on its own.
    fn row_items(&self, row: usize) -> Vec<(String, TrashEvent)> {
        let entries = self.entries();
        match self.rows.get(row) {
            Some(&Row::Entry(ix)) => {
                let entry = &entries[ix];
                let first = if entry.legacy {
                    (
                        t!("trash.show_folder").into_owned(),
                        TrashEvent::ShowFolder(entry.dir.clone()),
                    )
                } else {
                    let paths = entry.files.iter().map(|file| file.path.clone()).collect();
                    (
                        t!("trash.restore_all").into_owned(),
                        TrashEvent::Restore {
                            entry: entry.clone(),
                            paths,
                        },
                    )
                };
                let delete = (
                    t!("trash.delete").into_owned(),
                    TrashEvent::Delete(entry.clone()),
                );
                vec![first, delete]
            }
            Some(&Row::File(ix, file)) if !entries[ix].legacy => {
                let entry = &entries[ix];
                let restore = TrashEvent::Restore {
                    entry: entry.clone(),
                    paths: vec![entry.files[file].path.clone()],
                };
                vec![(t!("trash.restore").into_owned(), restore)]
            }
            Some(Row::File(..)) | None => Vec::new(),
        }
    }

    /// What a row is, for the palette: an entry's title and time, a file's path.
    fn row_detail(&self, row: usize) -> String {
        let entries = self.entries();
        match self.rows.get(row) {
            Some(&Row::Entry(ix)) => {
                let entry = &entries[ix];
                let time = format_time((entry.time_ms / 1000) as i64);
                format!("{} {time}", entry_title(entry))
            }
            Some(&Row::File(ix, file)) => entries[ix].files[file].path.to_string(),
            None => String::new(),
        }
    }

    /// The arrows move the cursor through the rows and keep it in view.
    fn move_cursor(&mut self, to: Step, cx: &mut Context<Self>) {
        let Some(row) = list_keys::step(self.rows.len(), self.cursor, to) else {
            return;
        };
        self.cursor = Some(row);
        self.scroll.scroll_to_item(row, ScrollStrategy::Nearest);
        cx.notify();
    }

    /// Enter: the cursor row's first action (restore, or show the folder).
    fn open_row(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.cursor else {
            return;
        };
        if let Some((_, event)) = self.row_items(row).into_iter().next() {
            cx.emit(event);
        }
    }

    /// Delete: the cursor's entry (a file's entry too) is deleted for good, after asking.
    fn delete_row(&mut self, cx: &mut Context<Self>) {
        let Some(&(Row::Entry(ix) | Row::File(ix, _))) =
            self.cursor.and_then(|row| self.rows.get(row))
        else {
            return;
        };
        if let Some(entry) = self.entries().get(ix) {
            cx.emit(TrashEvent::Delete(entry.clone()));
        }
    }

    /// Shift+F10: the cursor row's menu in the palette.
    fn row_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.cursor else {
            return;
        };
        let items = self.row_items(row);
        if items.is_empty() {
            return;
        }
        let commands = list_keys::event_commands(
            &cx.entity().downgrade(),
            &t!("palette.group.trash"),
            &self.row_detail(row),
            items,
        );
        crate::palette::open(commands, window, cx);
    }

    fn render_rows(
        &mut self,
        range: Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);
        let view = cx.entity().downgrade();
        let emit_view = view.clone();
        let emit = move |event: TrashEvent| {
            let view = emit_view.clone();
            let event = std::cell::RefCell::new(Some(event));
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                if let Some(event) = event.borrow_mut().take() {
                    view.update(cx, |_, cx| cx.emit(event)).ok();
                }
            }
        };
        let entries = self.entries();
        range
            .map(|row| match self.rows[row] {
                Row::Entry(ix) => {
                    let entry = &entries[ix];
                    let files = tn!("trash.files", count = entry.files.len()).into_owned();
                    let size = size_text(entry.size_bytes);
                    let detail = t!("common.middle_dot", first = files, second = size).into_owned();
                    let paths: Vec<BString> =
                        entry.files.iter().map(|file| file.path.clone()).collect();
                    let title = entry_title(entry);
                    let cursor = self.cursor == Some(row);
                    list_row(("trash-row", row), cursor, &p, &m)
                        .when(!cursor, |this| this.bg(p.panel2))
                        .on_click(cx.listener(move |this, _, _, cx| this.put_cursor(row, cx)))
                        // The title gives way when the row is short of room (an older
                        // folder's is long): the time and the count are short and stay whole.
                        .child(
                            truncated("title", title)
                                .min_w(m.dp(48.))
                                .font_semibold()
                                .text_color(p.fg),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_color(p.fg2)
                                .child(format_time((entry.time_ms / 1000) as i64)),
                        )
                        .child(div().flex_shrink_0().text_color(p.fg3).child(detail))
                        .child(div().flex_1())
                        // A folder from before records says nothing about its repository:
                        // its files can be looked at and moved by hand.
                        .when(!entry.legacy, |this| {
                            this.child(link(
                                ("trash-restore-all", row),
                                t!("trash.restore_all").into_owned(),
                                p.acc,
                                m.text.small,
                                emit(TrashEvent::Restore {
                                    entry: entry.clone(),
                                    paths,
                                }),
                            ))
                        })
                        .when(entry.legacy, |this| {
                            this.child(link(
                                ("trash-show", row),
                                t!("trash.show_folder").into_owned(),
                                p.acc,
                                m.text.small,
                                emit(TrashEvent::ShowFolder(entry.dir.clone())),
                            ))
                        })
                        .child(link(
                            ("trash-delete", row),
                            t!("trash.delete").into_owned(),
                            p.fg3,
                            m.text.small,
                            emit(TrashEvent::Delete(entry.clone())),
                        ))
                        .context_menu(row_menu(view.clone(), row))
                        .into_any_element()
                }
                Row::File(ix, file) => {
                    let entry = &entries[ix];
                    let item = &entry.files[file];
                    let (letter, color) = match item.kind {
                        TrashFileKind::Tracked { .. } => ("M", p.modified),
                        TrashFileKind::Untracked => ("A", p.add),
                    };
                    list_row(("trash-row", row), self.cursor == Some(row), &p, &m)
                        .pl(px((12. + FILE_INDENT) * m.zoom))
                        .on_click(cx.listener(move |this, _, _, cx| this.put_cursor(row, cx)))
                        .child(status_badge(letter, color, &m))
                        .child(path_label(&item.path.to_string(), &p, &m))
                        .child(div().flex_1())
                        .when(!entry.legacy, |this| {
                            this.child(link(
                                ("trash-restore", row),
                                t!("trash.restore").into_owned(),
                                p.acc,
                                m.text.small,
                                emit(TrashEvent::Restore {
                                    entry: entry.clone(),
                                    paths: vec![item.path.clone()],
                                }),
                            ))
                        })
                        .context_menu(row_menu(view.clone(), row))
                        .into_any_element()
                }
            })
            .collect()
    }

    fn put_cursor(&mut self, row: usize, cx: &mut Context<Self>) {
        self.cursor = Some(row);
        cx.notify();
    }
}

/// The entries of this repository, and the folders from before records. Empty trash takes
/// only the first: a folder from before records is listed on every repository's page, and
/// may hold another repository's files (#19).
fn split_older(entries: &[TrashEntry]) -> (Vec<TrashEntry>, Vec<TrashEntry>) {
    entries.iter().cloned().partition(|entry| !entry.legacy)
}

/// An entry's title: what put it there, or that it's a folder from before records (what put
/// it there and from where is unknown).
fn entry_title(entry: &TrashEntry) -> String {
    if entry.legacy {
        t!("trash.action_legacy").into_owned()
    } else {
        action_label(entry.action)
    }
}

/// A row's right-click menu: its items are made when it opens (an entry can hold thousands
/// of files, too many to copy for every row drawn).
fn row_menu(
    view: WeakEntity<TrashView>,
    row: usize,
) -> impl Fn(
    gpui_kit::component::menu::PopupMenu,
    &mut Window,
    &mut Context<gpui_kit::component::menu::PopupMenu>,
) -> gpui_kit::component::menu::PopupMenu
+ 'static {
    move |menu, _, cx| {
        let items = view
            .upgrade()
            .map(|this| this.read(cx).row_items(row))
            .unwrap_or_default();
        list_keys::event_menu(menu, &view, &items)
    }
}

impl Render for TrashView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.focus_on_render) {
            self.focus.focus(window, cx);
        }
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);

        let entries = self.entries();
        // The count and size are this repository's; folders from an older version (listed
        // last, their repository unknown) say so on their own rows.
        let own: Vec<_> = entries.iter().filter(|entry| !entry.legacy).collect();
        let size: u64 = own.iter().map(|entry| entry.size_bytes).sum();
        let subtitle = match (own.len(), entries.len()) {
            (_, 0) => String::new(),
            (0, older) => tn!("trash.subtitle_older", count = older).into_owned(),
            (count, _) => tn!("trash.subtitle", count = count, size = size_text(size)).into_owned(),
        };
        let (own, older) = split_older(entries);
        let header = centre_header(t!("trash.title").into_owned(), subtitle, &p, &m)
            .when(!older.is_empty(), |this| {
                this.child(
                    Button::new("delete-older")
                        .small()
                        .ghost()
                        .label(t!("trash.delete_older").into_owned())
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(TrashEvent::DeleteOlder(older.clone()))
                        })),
                )
            })
            .child(
                Button::new("empty-trash")
                    .small()
                    .ghost()
                    .label(t!("trash.empty_trash").into_owned())
                    .disabled(own.is_empty())
                    .on_click(
                        cx.listener(move |_, _, _, cx| cx.emit(TrashEvent::Empty(own.clone()))),
                    ),
            )
            .child(
                Button::new("close-trash")
                    .small()
                    .ghost()
                    .label(t!("diff.close").into_owned())
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(TrashEvent::Close))),
            );

        let body = match &self.content {
            Content::Loading => placeholder(t!("repo.loading").into_owned(), p.fg3, &m),
            Content::Failed(message) => placeholder(message.clone(), p.del, &m),
            Content::Loaded(entries) if entries.is_empty() => {
                placeholder(t!("trash.empty").into_owned(), p.fg3, &m)
            }
            Content::Loaded(_) => v_flex().flex_1().min_h_0().child(
                uniform_list(
                    "trash-rows",
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
            .on_action(cx.listener(|this, _: &OpenRow, _, cx| this.open_row(cx)))
            .on_action(cx.listener(|this, _: &DeleteRow, _, cx| this.delete_row(cx)))
            .on_action(cx.listener(|this, _: &RowMenu, window, cx| this.row_menu(window, cx)))
            .child(header)
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::split_older;
    use gitweft_core::trash::{TrashAction, TrashEntry};
    use std::path::PathBuf;

    fn entry(name: &str, legacy: bool) -> TrashEntry {
        TrashEntry {
            dir: PathBuf::from(name),
            files_dir: PathBuf::from(name),
            root: (!legacy).then(|| PathBuf::from("/repo")),
            time_ms: 0,
            action: TrashAction::Discard,
            files: Vec::new(),
            size_bytes: 0,
            legacy,
        }
    }

    /// Empty trash takes this repository's entries only; the folders from before records,
    /// listed on every repository's page, go by their own button (#19).
    #[test]
    fn empty_trash_leaves_the_folders_whose_repository_is_unknown() {
        let entries = [entry("a", false), entry("old", true), entry("b", false)];
        let (own, older) = split_older(&entries);
        let names = |list: &[TrashEntry]| -> Vec<PathBuf> {
            list.iter().map(|entry| entry.dir.clone()).collect()
        };
        assert_eq!(names(&own), [PathBuf::from("a"), PathBuf::from("b")]);
        assert_eq!(names(&older), [PathBuf::from("old")]);
    }
}
