//! Centre panel browsing the files of a commit. Folders expand in place; clicking a file
//! shows it on top of the tree, and a file's menu offers its blame and history. The
//! arrows move the cursor (the selected row), Enter acts as a click, Shift+F10 shows a
//! file's menu.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;

use gitweft_git::ObjectId;
use gitweft_git::gix::bstr::{BString, ByteSlice};
use gitweft_git::read::{TreeEntry, TreeEntryKind};
use gitweft_ui::current::mono_font;
use gitweft_ui::truncated::truncated;
use gitweft_ui::widgets::{centre_header, list_row, placeholder};
use gitweft_ui::{AppIcon, LookExt};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::{Icon, Sizable, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::list_keys::{self, FirstRow, LastRow, NextRow, OpenRow, PreviousRow, RowMenu, Step};

/// Each level of folders indents its rows by this much (design pixels).
const INDENT: f32 = 16.;
pub(crate) const CONTEXT: &str = "TreeView";

actions!(tree, [CloseTree, CollapseOrParent, ExpandOrChild]);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", CloseTree, Some(CONTEXT)),
        KeyBinding::new("left", CollapseOrParent, Some(CONTEXT)),
        KeyBinding::new("right", ExpandOrChild, Some(CONTEXT)),
    ]);
}

/// A commit's tree as nested nodes, plus the rows currently shown: the top level and the
/// contents of expanded folders. Plain data, so it is built on a background job.
pub struct TreeModel {
    nodes: Vec<Node>,
    roots: Vec<usize>,
    /// Node indices of the shown rows, in display order.
    visible: Vec<usize>,
}

pub struct Node {
    pub name: String,
    pub path: BString,
    pub kind: TreeEntryKind,
    pub depth: usize,
    pub expanded: bool,
    parent: Option<usize>,
    children: Vec<usize>,
}

impl TreeModel {
    /// `entries` must list each directory before its contents (as a breadth-first walk does).
    pub fn new(entries: Vec<TreeEntry>) -> Self {
        let mut nodes: Vec<Node> = Vec::with_capacity(entries.len());
        let mut index: HashMap<BString, usize> = HashMap::with_capacity(entries.len());
        let mut roots = Vec::new();
        for entry in entries {
            let (parent, name) = match entry.path.rfind_byte(b'/') {
                Some(slash) => (
                    index.get(entry.path[..slash].as_bstr()).copied(),
                    entry.path[slash + 1..].to_str_lossy().into_owned(),
                ),
                None => (None, entry.path.to_str_lossy().into_owned()),
            };
            let ix = nodes.len();
            index.insert(entry.path.clone(), ix);
            nodes.push(Node {
                name,
                path: entry.path,
                kind: entry.kind,
                depth: parent.map_or(0, |p| nodes[p].depth + 1),
                expanded: false,
                parent,
                children: Vec::new(),
            });
            match parent {
                Some(parent) => nodes[parent].children.push(ix),
                None => roots.push(ix),
            }
        }
        for ix in 0..nodes.len() {
            let mut children = std::mem::take(&mut nodes[ix].children);
            children.sort_by(|a, b| compare(&nodes[*a], &nodes[*b]));
            nodes[ix].children = children;
        }
        roots.sort_by(|a, b| compare(&nodes[*a], &nodes[*b]));
        let mut model = Self {
            nodes,
            roots,
            visible: Vec::new(),
        };
        model.rebuild();
        model
    }

    pub fn visible(&self) -> &[usize] {
        &self.visible
    }

    pub fn node(&self, ix: usize) -> &Node {
        &self.nodes[ix]
    }

    pub fn file_count(&self) -> usize {
        self.nodes
            .iter()
            .filter(|node| node.kind != TreeEntryKind::Directory)
            .count()
    }

    /// Expands or collapses a folder; a collapsed folder keeps its subfolders' state.
    pub fn toggle(&mut self, ix: usize) {
        let node = &mut self.nodes[ix];
        if node.kind == TreeEntryKind::Directory {
            node.expanded = !node.expanded;
            self.rebuild();
        }
    }

    /// Left on node `ix` (#31): an expanded folder collapses and keeps the cursor; anything
    /// else sends the cursor to its folder. Returns where the cursor goes.
    pub fn collapse_or_parent(&mut self, ix: usize) -> usize {
        let node = &self.nodes[ix];
        if node.kind == TreeEntryKind::Directory && node.expanded {
            self.toggle(ix);
            ix
        } else {
            node.parent.unwrap_or(ix)
        }
    }

    /// Right on node `ix` (#31): a collapsed folder expands and keeps the cursor; an expanded
    /// one sends it to its first entry; a file stays. Returns where the cursor goes.
    pub fn expand_or_child(&mut self, ix: usize) -> usize {
        let node = &self.nodes[ix];
        if node.kind != TreeEntryKind::Directory {
            return ix;
        }
        if !node.expanded {
            self.toggle(ix);
            return ix;
        }
        node.children.first().copied().unwrap_or(ix)
    }

    fn rebuild(&mut self) {
        self.visible.clear();
        let mut stack: Vec<usize> = self.roots.iter().rev().copied().collect();
        while let Some(ix) = stack.pop() {
            self.visible.push(ix);
            let node = &self.nodes[ix];
            if node.expanded {
                stack.extend(node.children.iter().rev());
            }
        }
    }
}

/// Folders first, then by name, ignoring case.
fn compare(a: &Node, b: &Node) -> Ordering {
    let is_file = |node: &Node| node.kind != TreeEntryKind::Directory;
    is_file(a)
        .cmp(&is_file(b))
        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        .then_with(|| a.name.cmp(&b.name))
}

#[derive(Clone)]
pub enum TreeEvent {
    Close,
    OpenFile {
        commit: ObjectId,
        path: BString,
    },
    Blame {
        commit: ObjectId,
        path: BString,
    },
    History {
        commit: ObjectId,
        path: BString,
    },
    /// The path's working-tree copy: a file in the editor.
    Editor {
        path: BString,
    },
    /// The path's working-tree copy in the file manager (a file selected in its folder).
    FileManager {
        path: BString,
    },
    /// Start a bisect of only the commits that change this file or folder.
    Bisect {
        path: BString,
    },
    /// A terminal in the path's working-tree folder.
    Terminal {
        path: BString,
    },
}

impl EventEmitter<TreeEvent> for TreeView {}

impl Focusable for TreeView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

enum Content {
    Loading,
    Loaded(TreeModel),
    Failed(String),
}

pub struct TreeView {
    commit: ObjectId,
    /// The repository's working tree (none when bare): where the external programs find a
    /// path's copy.
    workdir: Option<PathBuf>,
    content: Content,
    /// The node clicked last.
    selected: Option<usize>,
    scroll: UniformListScrollHandle,
    focus: FocusHandle,
    /// Take keyboard focus on the next frame, so Esc works.
    focus_on_render: bool,
}

impl TreeView {
    /// Whether its rows are read (the headless UI tests wait for it before keys).
    #[cfg(test)]
    pub(crate) fn is_loaded(&self) -> bool {
        matches!(self.content, Content::Loaded(_))
    }

    pub fn new(commit: ObjectId, workdir: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
        Self {
            commit,
            workdir,
            content: Content::Loading,
            selected: None,
            scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
            focus_on_render: true,
        }
    }

    pub fn set_result(&mut self, result: Result<TreeModel, String>, cx: &mut Context<Self>) {
        self.content = match result {
            Ok(model) => Content::Loaded(model),
            Err(message) => Content::Failed(message),
        };
        cx.notify();
    }

    /// Takes the keys again once a view shown on top of it closes.
    pub fn refocus(&mut self, cx: &mut Context<Self>) {
        self.focus_on_render = true;
        cx.notify();
    }

    fn close(&mut self, _: &CloseTree, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(TreeEvent::Close);
    }

    /// A folder expands or collapses; a file opens. A submodule has nothing to show here.
    fn click(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Content::Loaded(model) = &mut self.content else {
            return;
        };
        self.selected = Some(ix);
        let (kind, path) = (model.node(ix).kind, model.node(ix).path.clone());
        match kind {
            TreeEntryKind::Directory => model.toggle(ix),
            TreeEntryKind::Submodule => {}
            TreeEntryKind::File | TreeEntryKind::Executable | TreeEntryKind::Symlink => {
                cx.emit(TreeEvent::OpenFile {
                    commit: self.commit,
                    path,
                });
            }
        }
        cx.notify();
    }

    /// Left and Right: collapse or expand the folder under the cursor, or go to its folder
    /// or its first entry (#31).
    fn fold(&mut self, expand: bool, cx: &mut Context<Self>) {
        let (Content::Loaded(model), Some(ix)) = (&mut self.content, self.selected) else {
            return;
        };
        let target = if expand {
            model.expand_or_child(ix)
        } else {
            model.collapse_or_parent(ix)
        };
        self.selected = Some(target);
        if let Some(row) = model.visible().iter().position(|&shown| shown == target) {
            self.scroll.scroll_to_item(row, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    /// The arrows move the cursor through the rows shown and keep it in view.
    fn move_cursor(&mut self, to: Step, cx: &mut Context<Self>) {
        let Content::Loaded(model) = &self.content else {
            return;
        };
        let visible = model.visible();
        // A node hidden by a collapsed folder starts over.
        let current = self
            .selected
            .and_then(|ix| visible.iter().position(|&shown| shown == ix));
        let Some(row) = list_keys::step(visible.len(), current, to) else {
            return;
        };
        self.selected = Some(visible[row]);
        self.scroll.scroll_to_item(row, ScrollStrategy::Nearest);
        cx.notify();
    }

    /// A file's menu, for its right-click menu and Shift+F10.
    fn row_items(&self, node: &Node) -> Vec<(String, TreeEvent)> {
        row_items(&node.path, node.kind, self.commit, self.workdir.as_deref())
    }

    /// Shift+F10: the cursor's file menu in the palette.
    fn row_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Content::Loaded(model), Some(ix)) = (&self.content, self.selected) else {
            return;
        };
        let node = model.node(ix);
        let items = self.row_items(node);
        if items.is_empty() {
            return;
        }
        let commands = list_keys::event_commands(
            &cx.entity().downgrade(),
            &t!("palette.group.tree"),
            &node.path.to_string(),
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
        let Content::Loaded(model) = &self.content else {
            return Vec::new();
        };
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);
        let z = |value: f32| px(value * m.zoom);
        let mono = mono_font();
        let view = cx.entity().downgrade();
        range
            .map(|row| {
                let ix = model.visible()[row];
                let node = model.node(ix);
                let is_dir = node.kind == TreeEntryKind::Directory;
                let chevron = if is_dir {
                    let icon = if node.expanded {
                        AppIcon::ChevronDown
                    } else {
                        AppIcon::ChevronRight
                    };
                    Icon::new(icon)
                        .size(z(14.))
                        .text_color(p.fg3)
                        .into_any_element()
                } else {
                    div().w(z(14.)).flex_shrink_0().into_any_element()
                };
                let (icon, icon_color) = if is_dir {
                    (AppIcon::Folder, p.fg2)
                } else {
                    (AppIcon::File, p.fg3)
                };
                // A list row, but as high as the other file lists' rows and indented by depth.
                let row_element = list_row(("tree-row", ix), self.selected == Some(ix), &p, &m)
                    .h(m.file_row)
                    .pl(z(12. + node.depth as f32 * INDENT))
                    .gap(z(6.))
                    .on_click(cx.listener(move |this, _, _, cx| this.click(ix, cx)))
                    .child(chevron)
                    .child(Icon::new(icon).size(z(15.)).text_color(icon_color))
                    .child(
                        truncated("name", node.name.clone())
                            .font_family(mono.clone())
                            .text_size(m.text.mono)
                            .text_color(p.fg),
                    )
                    .when(node.kind == TreeEntryKind::Submodule, |this| {
                        this.child(
                            div()
                                .flex_shrink_0()
                                .text_size(m.text.xs)
                                .text_color(p.fg3)
                                .child(t!("tree.submodule").into_owned()),
                        )
                    });
                // Built when the menu opens: it looks at the working tree (#24), which a
                // frame mustn't.
                let view = view.clone();
                let (path, kind, commit) = (node.path.clone(), node.kind, self.commit);
                let workdir = self.workdir.clone();
                row_element
                    .context_menu(move |menu, _, _| {
                        let items = row_items(&path, kind, commit, workdir.as_deref());
                        list_keys::event_menu(menu, &view, &items)
                    })
                    .into_any_element()
            })
            .collect()
    }
}

impl Render for TreeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.focus_on_render) {
            self.focus.focus(window, cx);
        }
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);

        let count = match &self.content {
            Content::Loaded(model) => tn!("tree.count", count = model.file_count()).into_owned(),
            Content::Loading | Content::Failed(_) => String::new(),
        };
        let title = t!("tree.title", sha = self.commit.to_hex_with_len(8)).into_owned();
        let header = centre_header(title, count, &p, &m).child(
            Button::new("close-tree")
                .small()
                .ghost()
                .label(t!("diff.close").into_owned())
                .on_click(cx.listener(|_, _, _, cx| cx.emit(TreeEvent::Close))),
        );

        let body = match &self.content {
            Content::Loading => {
                placeholder(t!("repo.loading").into_owned(), p.fg3, &m).into_any_element()
            }
            Content::Failed(message) => placeholder(message.clone(), p.del, &m).into_any_element(),
            // A commit with no files (#44), like its peers' empty lists.
            Content::Loaded(model) if model.visible().is_empty() => {
                placeholder(t!("tree.empty").into_owned(), p.fg3, &m).into_any_element()
            }
            Content::Loaded(model) => uniform_list(
                "tree-rows",
                model.visible().len(),
                cx.processor(Self::render_rows),
            )
            .track_scroll(&self.scroll)
            .flex_1()
            .py(px(4. * m.zoom))
            .into_any_element(),
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
                    this.click(ix, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &RowMenu, window, cx| this.row_menu(window, cx)))
            .on_action(cx.listener(|this, _: &CollapseOrParent, _, cx| this.fold(false, cx)))
            .on_action(cx.listener(|this, _: &ExpandOrChild, _, cx| this.fold(true, cx)))
            .child(header)
            .child(body)
    }
}

/// The menu of the tree row at `path` (right click, Shift+F10). Every row can start a
/// bisect; a file also opens its blame and history; a folder or file that's in the
/// working tree (the tree may be an older commit's) opens in the user's programs. It
/// looks at the working tree, so it's built when a menu opens, never in a frame (#24).
fn row_items(
    path: &BString,
    kind: TreeEntryKind,
    commit: ObjectId,
    workdir: Option<&std::path::Path>,
) -> Vec<(String, TreeEvent)> {
    let copy = workdir.map(|dir| dir.join(gitweft_core::external::path_from_bytes(path)));
    let bisect = (
        t!("bisect.start_path").into_owned(),
        TreeEvent::Bisect { path: path.clone() },
    );
    if matches!(kind, TreeEntryKind::Directory | TreeEntryKind::Submodule) {
        let mut items = vec![bisect];
        if copy.is_some_and(|copy| copy.is_dir()) {
            items.push((
                t!("external.open_terminal").into_owned(),
                TreeEvent::Terminal { path: path.clone() },
            ));
            items.push((
                t!("external.open_file_manager").into_owned(),
                TreeEvent::FileManager { path: path.clone() },
            ));
        }
        return items;
    }
    let mut items = vec![
        (
            t!("blame.open").into_owned(),
            TreeEvent::Blame {
                commit,
                path: path.clone(),
            },
        ),
        (
            t!("history.open").into_owned(),
            TreeEvent::History {
                commit,
                path: path.clone(),
            },
        ),
        bisect,
    ];
    if copy.is_some_and(|copy| copy.is_file()) {
        items.push((
            t!("external.open_working_copy").into_owned(),
            TreeEvent::Editor { path: path.clone() },
        ));
        items.push((
            t!("external.show_in_folder").into_owned(),
            TreeEvent::FileManager { path: path.clone() },
        ));
    }
    items
}

#[cfg(test)]
mod tests {
    use super::{TreeEntry, TreeEntryKind, TreeModel};
    use gitweft_git::ObjectId;
    use gitweft_git::gix::hash::Kind;

    fn entry(path: &str, kind: TreeEntryKind) -> TreeEntry {
        TreeEntry {
            path: path.into(),
            kind,
            id: ObjectId::null(Kind::Sha1),
        }
    }

    fn shown(model: &TreeModel) -> Vec<String> {
        model
            .visible()
            .iter()
            .map(|&ix| {
                let node = model.node(ix);
                format!("{}{}", "  ".repeat(node.depth), node.name)
            })
            .collect()
    }

    fn find(model: &TreeModel, path: &str) -> usize {
        (0..model.nodes.len())
            .find(|&ix| model.node(ix).path == path)
            .unwrap()
    }

    /// Left and Right on the tree (#31).
    #[test]
    fn left_collapses_or_goes_up_and_right_expands_or_goes_in() {
        let mut model = TreeModel::new(vec![
            entry("src", TreeEntryKind::Directory),
            entry("README", TreeEntryKind::File),
            entry("src/ui", TreeEntryKind::Directory),
            entry("src/main.rs", TreeEntryKind::File),
            entry("src/ui/view.rs", TreeEntryKind::File),
        ]);
        let (src, ui, main, view) = (
            find(&model, "src"),
            find(&model, "src/ui"),
            find(&model, "src/main.rs"),
            find(&model, "src/ui/view.rs"),
        );
        let readme = find(&model, "README");

        // Right on a closed folder opens it and stays; again, goes to its first entry.
        assert_eq!(model.expand_or_child(src), src);
        assert_eq!(shown(&model), ["src", "  ui", "  main.rs", "README"]);
        assert_eq!(model.expand_or_child(src), ui);
        assert_eq!(model.expand_or_child(ui), ui);
        assert_eq!(model.expand_or_child(ui), view);
        // A file stays put.
        assert_eq!(model.expand_or_child(view), view);

        // Left on a file goes to its folder; on an open folder, closes it and stays.
        assert_eq!(model.collapse_or_parent(view), ui);
        assert_eq!(model.collapse_or_parent(ui), ui);
        assert_eq!(shown(&model), ["src", "  ui", "  main.rs", "README"]);
        assert_eq!(model.collapse_or_parent(main), src);
        assert_eq!(model.collapse_or_parent(src), src);
        assert_eq!(shown(&model), ["src", "README"]);
        // At the top level, a closed folder or a file has nowhere to go.
        assert_eq!(model.collapse_or_parent(src), src);
        assert_eq!(model.collapse_or_parent(readme), readme);
    }

    #[test]
    fn folders_first_then_names_ignoring_case_and_expand_in_place() {
        use TreeEntryKind::*;
        // Breadth-first, as gix walks a tree.
        let mut model = TreeModel::new(vec![
            entry("b.txt", File),
            entry("B.md", File),
            entry("a", Directory),
            entry("a/z.rs", File),
            entry("a/y", Directory),
            entry("a/y/x.rs", File),
        ]);
        assert_eq!(shown(&model), ["a", "B.md", "b.txt"]);
        assert_eq!(model.file_count(), 4);

        model.toggle(find(&model, "a"));
        assert_eq!(shown(&model), ["a", "  y", "  z.rs", "B.md", "b.txt"]);
        model.toggle(find(&model, "a/y"));
        assert_eq!(
            shown(&model),
            ["a", "  y", "    x.rs", "  z.rs", "B.md", "b.txt"]
        );

        // Collapsing hides the contents; the subfolder stays expanded for next time.
        model.toggle(find(&model, "a"));
        assert_eq!(shown(&model), ["a", "B.md", "b.txt"]);
        model.toggle(find(&model, "a"));
        assert_eq!(shown(&model).len(), 6);

        // Toggling a file does nothing.
        model.toggle(find(&model, "b.txt"));
        assert_eq!(shown(&model).len(), 6);
    }
}
