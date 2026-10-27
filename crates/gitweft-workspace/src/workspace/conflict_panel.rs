//! The right panel's "Conflicted files" mode. While an operation is stopped on conflicts
//! and the working changes are shown, it lists the files this step stopped on (with the
//! conflicts left in each, or "resolved"), says what's left to do, and commits the merge
//! with git's prepared message as edited here (other operations continue).

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;

use gitweft_core::conflicts::{self, ConflictRecord};
use gitweft_diff::conflict::{conflict_count, parse_conflicts_sized};
use gitweft_git::conflict_files::{binary_merge_paths, conflict_marker_size};
use gitweft_git::merge::{prepared_conflicts, prepared_message};

use gpui_kit::component::menu::ContextMenuExt as _;

use super::*;
use crate::staging_view::link;

/// Files bigger than this aren't read to count their conflicts (they say "conflicted").
const MAX_COUNTED_BYTES: u64 = 8 << 20;

/// Why a conflicted file can't be merged line by line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WholeNote {
    DeletedHere,
    DeletedByThem,
    OnlyHere,
    OnlyTheirs,
    DeletedBoth,
    Binary,
    /// `.gitattributes` gives it git's binary merge driver (`merge=binary`, `-merge`,
    /// `binary`): git kept our version, without markers.
    BinaryAttributes,
    /// A symbolic link: its target can't hold markers.
    Symlink,
}

impl WholeNote {
    /// From git's two-letter status code; binary files are found by their content.
    fn from_code(code: [u8; 2]) -> Option<Self> {
        match &code {
            b"DU" => Some(Self::DeletedHere),
            b"UD" => Some(Self::DeletedByThem),
            b"AU" => Some(Self::OnlyHere),
            b"UA" => Some(Self::OnlyTheirs),
            b"DD" => Some(Self::DeletedBoth),
            _ => None,
        }
    }

    fn label(self) -> String {
        match self {
            Self::DeletedHere => t!("conflict.note_deleted_here"),
            Self::DeletedByThem => t!("conflict.note_deleted_by_them"),
            Self::OnlyHere => t!("conflict.note_only_here"),
            Self::OnlyTheirs => t!("conflict.note_only_theirs"),
            Self::DeletedBoth => t!("conflict.note_deleted_both"),
            Self::Binary => t!("conflict.note_binary"),
            Self::BinaryAttributes => t!("conflict.note_binary_attributes"),
            Self::Symlink => t!("conflict.note_symlink"),
        }
        .into_owned()
    }
}

/// Git's rule for binary content: a NUL byte in the first 8000 bytes.
pub(super) fn looks_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8000)].contains(&0)
}

/// What the status job reads for the panel.
#[derive(Default)]
pub(super) struct ConflictRead {
    /// Conflicts left in each conflicted file.
    counts: HashMap<BString, usize>,
    /// The conflicted files that can't be merged line by line, and why.
    notes: HashMap<BString, WholeNote>,
    /// The files git listed as conflicted when the operation stopped (`MERGE_MSG`), so
    /// the ones resolved before the app was opened are listed too.
    listed: Vec<BString>,
    /// A merge's prepared message (`MERGE_MSG` without its comments).
    prepared: Option<String>,
    /// What we kept of the files an operation stopped on (`gitweft_core::conflicts`).
    recorded: Option<ConflictRecord>,
    /// Of the files the panel may list, the ones in the working tree.
    on_disk: HashSet<BString>,
}

/// Reads what the panel shows for `status`, on the status job's thread. `known`: the
/// files the panel lists now.
pub(super) fn read_conflicts(
    git: &GitCli,
    repo: &gitweft_git::gix::Repository,
    workdir: &Path,
    status: &Status,
    operation: Option<Operation>,
    known: Vec<BString>,
    cancel: &CancelToken,
) -> ConflictRead {
    let recorded = conflicts::read(&conflicts::conflicts_file(workdir));
    if operation.is_none_or(|op| op == Operation::Bisect) {
        return ConflictRead {
            recorded,
            ..ConflictRead::default()
        };
    }
    let mut counts = HashMap::new();
    let mut notes = HashMap::new();
    let mut by_content = Vec::new();
    for entry in status.conflicts() {
        match WholeNote::from_code(entry.code) {
            Some(note) => {
                notes.insert(entry.path.clone(), note);
            }
            None => by_content.push(entry.path.clone()),
        }
    }
    // Git's binary merge driver leaves our version with no markers to count.
    let by_attributes = binary_merge_paths(git, workdir, &by_content, cancel).unwrap_or_default();
    for path in by_content {
        if by_attributes.contains(&path) {
            notes.insert(path, WholeNote::BinaryAttributes);
            continue;
        }
        let file = workdir.join(OsStr::from_bytes(&path));
        // Reading a link would read its target: a conflicted link has no markers.
        if file
            .symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_symlink())
        {
            notes.insert(path, WholeNote::Symlink);
            continue;
        }
        let bytes = std::fs::metadata(&file)
            .ok()
            .filter(|meta| meta.len() <= MAX_COUNTED_BYTES)
            .and_then(|_| std::fs::read(&file).ok());
        match bytes {
            Some(bytes) if looks_binary(&bytes) => {
                notes.insert(path, WholeNote::Binary);
            }
            Some(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                // Markers as long as the file's `conflict-marker-size` says.
                let size = conflict_marker_size(git, workdir, &path, cancel);
                counts.insert(path, conflict_count(&parse_conflicts_sized(&text, size)));
            }
            None => {
                counts.insert(path, 0);
            }
        }
    }
    let prepared = (operation == Some(Operation::Merge))
        .then(|| prepared_message(repo, git.version()))
        .flatten();
    let listed = prepared_conflicts(repo, git.version());
    let recorded_files = recorded.iter().flat_map(|record| &record.files);
    let on_disk = known
        .into_iter()
        .chain(listed.iter().cloned())
        .chain(recorded_files.map(|path| BString::from(path.as_slice())))
        .filter(|path| {
            workdir
                .join(OsStr::from_bytes(path))
                .symlink_metadata()
                .is_ok()
        })
        .collect();
    ConflictRead {
        counts,
        notes,
        listed,
        prepared,
        recorded,
        on_disk,
    }
}

/// How the record file names a step: the operation and HEAD's commit.
fn step_name(key: StepKey) -> Option<String> {
    let (operation, head) = key?;
    let name = match operation {
        Operation::Merge => "merge",
        Operation::Rebase => "rebase",
        Operation::CherryPick => "cherry-pick",
        Operation::Revert => "revert",
        Operation::ApplyMailbox => "am",
        Operation::Bisect => "bisect",
    };
    let head = head.map_or_else(|| "-".to_owned(), |oid| oid.to_string());
    Some(format!("{name} {head}"))
}

/// What the record file should hold for `set`: nothing when no operation lists files.
fn wanted_record(set: &ConflictSet) -> Option<ConflictRecord> {
    let step = step_name(set.key).filter(|_| !set.files.is_empty())?;
    Some(ConflictRecord {
        step,
        files: set.files.iter().map(|path| path.to_vec()).collect(),
    })
}

/// The files of the record that belong to step `key` (a record of another step is stale).
fn recorded_files(record: Option<ConflictRecord>, key: StepKey) -> Vec<BString> {
    let step = step_name(key);
    record
        .filter(|record| Some(&record.step) == step.as_ref())
        .map(|record| record.files.into_iter().map(BString::from).collect())
        .unwrap_or_default()
}

/// The operation and HEAD commit a list of conflicted files belongs to.
type StepKey = Option<(Operation, Option<ObjectId>)>;

/// The files an operation stopped on, kept while it stays at the same step.
#[derive(Debug, Default)]
struct ConflictSet {
    /// A new step of a rebase or a cherry-pick sequence moves HEAD and starts a new list.
    key: StepKey,
    /// Every file conflicted at this step, in the order git first listed them; resolved
    /// ones stay.
    files: Vec<BString>,
    /// Conflicts left in each file that's still conflicted.
    counts: HashMap<BString, usize>,
    /// The still-conflicted files that can't be merged line by line, and why.
    notes: HashMap<BString, WholeNote>,
}

impl ConflictSet {
    /// Takes a new status's conflicted files; returns whether the operation or its step
    /// changed (the list started again).
    fn update(
        &mut self,
        key: StepKey,
        conflicted: impl IntoIterator<Item = BString>,
        counts: HashMap<BString, usize>,
    ) -> bool {
        let new_step = key != self.key;
        if new_step {
            self.key = key;
            self.files.clear();
        }
        if self.key.is_none() {
            self.counts.clear();
            return new_step;
        }
        for path in conflicted {
            if !self.files.contains(&path) {
                self.files.push(path);
            }
        }
        self.counts = counts;
        new_step
    }
}

/// A prepared message's summary line and its body.
fn split_message(message: &str) -> (String, String) {
    let message = message.trim();
    match message.split_once('\n') {
        Some((summary, body)) => (summary.trim().to_owned(), body.trim().to_owned()),
        None => (message.to_owned(), String::new()),
    }
}

/// The panel's state, owned by the workspace.
pub(super) struct ConflictPanel {
    set: ConflictSet,
    /// The merge commit's summary, prefilled from git's prepared message.
    message: Entity<InputState>,
    /// The prepared message's body (after the summary line), committed with it.
    body: String,
    /// A summary to put in `message` on the next frame (setting it needs the window).
    pending_summary: Option<String>,
    /// The user asked for every working change (the staging lists) instead.
    show_all: bool,
    /// Of the listed files, the ones in the working tree (a resolved file with no staged
    /// change opens at HEAD; one that's gone opens nothing).
    on_disk: HashSet<BString>,
    /// What the record file holds; `None` until the first status read it.
    saved: Option<Option<ConflictRecord>>,
    _message_events: Subscription,
}

impl ConflictPanel {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let message = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("conflict.panel_message_placeholder").into_owned())
        });
        // Typing enables or disables the button; Ctrl+Enter commits, as in the commit box.
        let events = cx.subscribe_in(
            &message,
            window,
            |this: &mut Workspace, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter {
                    secondary: true, ..
                } if this.conflict_panel.active() == Some(Operation::Merge)
                    && this.conflict_commit_ready(cx) =>
                {
                    this.commit_merge(window, cx)
                }
                InputEvent::Change => cx.notify(),
                _ => {}
            },
        );
        Self {
            set: ConflictSet::default(),
            message,
            body: String::new(),
            pending_summary: None,
            show_all: false,
            on_disk: HashSet::new(),
            saved: None,
            _message_events: events,
        }
    }

    /// Forgets the operation (another repository is opened).
    pub(super) fn reset(&mut self) {
        self.set = ConflictSet::default();
        self.body.clear();
        self.show_all = false;
        self.on_disk.clear();
        self.saved = None;
    }

    /// The files listed now, resolved ones included.
    pub(super) fn files(&self) -> &[BString] {
        &self.set.files
    }

    /// What the record file must now hold, when that changed: `Some(None)` removes it.
    pub(super) fn record_change(&mut self) -> Option<Option<ConflictRecord>> {
        let saved = self.saved.as_ref()?;
        let wanted = wanted_record(&self.set);
        if *saved == wanted {
            return None;
        }
        self.saved = Some(wanted.clone());
        Some(wanted)
    }

    /// Takes a new status, the operation read with it and what the status job read for
    /// the panel; returns whether anything the panel shows changed.
    pub(super) fn update(
        &mut self,
        operation: Option<Operation>,
        status: &Status,
        read: ConflictRead,
    ) -> bool {
        let key = operation
            .filter(|op| *op != Operation::Bisect)
            .map(|op| (op, status.branch.oid));
        let before = (
            self.set.files.len(),
            self.set.counts.clone(),
            self.set.notes.clone(),
            self.on_disk.clone(),
        );
        if self.saved.is_none() {
            self.saved = Some(read.recorded.clone());
        }
        // What we kept of this step (from before a restart), then git's list (its order),
        // then anything conflicted that neither has.
        let conflicted = recorded_files(read.recorded, key)
            .into_iter()
            .chain(read.listed)
            .chain(status.conflicts().map(|entry| entry.path.clone()));
        self.on_disk = read.on_disk;
        let new_step = self.set.update(key, conflicted, read.counts);
        self.set.notes = if self.set.key.is_some() {
            read.notes
        } else {
            HashMap::new()
        };
        if new_step {
            // A new operation or step: git's message for it, and the panel comes back.
            let (summary, body) = read
                .prepared
                .as_deref()
                .map(split_message)
                .unwrap_or_default();
            self.pending_summary = Some(summary);
            self.body = body;
            self.show_all = false;
        }
        new_step
            || before
                != (
                    self.set.files.len(),
                    self.set.counts.clone(),
                    self.set.notes.clone(),
                    self.on_disk.clone(),
                )
    }

    /// Puts a new prepared summary into the input (needs the window).
    pub(super) fn apply_pending(&mut self, window: &mut Window, cx: &mut App) {
        if let Some(summary) = self.pending_summary.take() {
            self.message
                .update(cx, |input, cx| input.set_value(summary, window, cx));
        }
    }

    /// The merge commit's message: the summary typed here, then the prepared body.
    fn message_text(&self, cx: &App) -> String {
        let summary = self.message.read(cx).value().trim().to_owned();
        if summary.is_empty() || self.body.is_empty() {
            summary
        } else {
            format!("{summary}\n\n{}", self.body)
        }
    }

    /// A merge is in progress and the panel holds a message for it.
    pub(super) fn has_message(&self, cx: &App) -> bool {
        matches!(self.set.key, Some((Operation::Merge, _))) && !self.message_text(cx).is_empty()
    }

    /// The operation whose conflicted files the panel lists (none listed: `None`).
    fn active(&self) -> Option<Operation> {
        self.set
            .key
            .filter(|_| !self.set.files.is_empty())
            .map(|(operation, _)| operation)
    }

    /// How many files this step stopped on, resolved ones included.
    pub(super) fn file_count(&self) -> usize {
        self.set.files.len()
    }
}

impl Workspace {
    /// The right panel while the working changes are shown: the conflicted files while an
    /// operation is stopped on some, else the staging lists.
    pub(super) fn render_working_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(operation) = self.conflict_panel.active() else {
            return self.staging_view.clone().into_any_element();
        };
        if self.conflict_panel.show_all {
            return v_flex()
                .size_full()
                .child(self.render_conflict_back_bar(cx))
                .child(div().flex_1().min_h_0().child(self.staging_view.clone()))
                .into_any_element();
        }
        self.render_conflict_panel(operation, cx)
    }

    /// Above the staging lists when the user left the conflicted files: the way back.
    fn render_conflict_back_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);
        let z = |value: f32| px(value * m.zoom);
        h_flex()
            .h(z(32.))
            .flex_shrink_0()
            .px(z(12.))
            .gap(z(8.))
            .bg(p.panel2)
            .border_b_1()
            .border_color(p.line)
            .child(Icon::new(AppIcon::Merge).small().text_color(p.modified))
            .child(link(
                "conflict-panel-back",
                t!("conflict.panel_back").into_owned(),
                p.acc,
                m.text.small,
                cx.listener(|this, _, _, cx| {
                    this.conflict_panel.show_all = false;
                    cx.notify();
                }),
            ))
    }

    fn render_conflict_panel(&self, operation: Operation, cx: &mut Context<Self>) -> AnyElement {
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);
        let z = |value: f32| px(value * m.zoom);
        let empty = Status::default();
        let status = self.status.as_ref().unwrap_or(&empty);
        let conflicted: HashSet<&BString> = status.conflicts().map(|entry| &entry.path).collect();
        let unresolved = conflicted.len();
        let open = self
            .conflict_view
            .as_ref()
            .map(|view| view.read(cx).path().clone());
        let set = &self.conflict_panel.set;
        let on_disk = &self.conflict_panel.on_disk;
        let head = status.branch.oid;
        let row_height = m.file_row + z(6.);

        let rows: Vec<AnyElement> = set
            .files
            .iter()
            .enumerate()
            .map(|(ix, path)| {
                let still = conflicted.contains(path);
                // A resolved file opens its staged diff (taking our side may leave none).
                let staged = (!still)
                    .then(|| status.staged().find(|entry| entry.path == *path).cloned())
                    .flatten();
                let note = still.then(|| set.notes.get(path).copied()).flatten();
                // Resolved with no staged change: HEAD's version (ours, taken whole), or
                // no file at all.
                let unchanged = !still && staged.is_none();
                let gone = unchanged && !on_disk.contains(path);
                let at_head = head.filter(|_| unchanged && !gone);
                let label = if let Some(note) = note {
                    note.label().into()
                } else if still {
                    match set.counts.get(path).copied().unwrap_or(0) {
                        0 => t!("conflict.panel_conflicted"),
                        count => tn!("conflict.panel_count", count = count),
                    }
                } else if gone {
                    t!("conflict.panel_resolved_deleted")
                } else if unchanged {
                    t!("conflict.panel_resolved_as_head")
                } else {
                    t!("conflict.panel_resolved")
                }
                .into_owned();
                let selected = open.as_ref() == Some(path);
                let clickable = still || staged.is_some() || at_head.is_some();
                let for_click = path.clone();
                let row = h_flex()
                    .id(("conflict-file", ix))
                    // How the headless UI tests find the row (a no-op outside them).
                    .debug_selector(|| format!("conflict-file-{ix}"))
                    .h(row_height)
                    .flex_shrink_0()
                    .px(z(8.))
                    .gap(z(8.))
                    .rounded(m.radius)
                    .text_size(m.text.small)
                    .when(selected, |row| row.bg(p.sel))
                    .when(clickable && !selected, |row| {
                        row.hover(move |style| style.bg(p.hover))
                    })
                    .when(clickable, |row| {
                        row.cursor_pointer()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if still {
                                    this.open_conflict(for_click.clone(), window, cx);
                                } else if let Some(entry) = &staged {
                                    this.open_status_diff(entry, true, cx);
                                } else if let Some(head) = at_head {
                                    // Nothing staged to diff: the file as it is, HEAD's.
                                    this.open_file(head, for_click.clone(), cx);
                                }
                            }))
                    })
                    .child(widgets::dot(if still { p.modified } else { p.add }, z(8.)))
                    .child(widgets::path_label(&path.to_string(), &p, &m))
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(m.text.xs)
                            .text_color(p.fg3)
                            .child(label),
                    );
                if !still {
                    return row.into_any_element();
                }
                // A conflicted file also opens in git's merge tool, or in the editor.
                let (this, path) = (cx.entity().downgrade(), path.clone());
                row.context_menu(move |menu, _, _| {
                    let (merge, edit) =
                        ((this.clone(), path.clone()), (this.clone(), path.clone()));
                    menu.item(
                        PopupMenuItem::new(t!("external.merge_tool").into_owned()).on_click(
                            move |_, window, cx| {
                                let (this, path) = &merge;
                                this.update(cx, |w, cx| w.run_merge_tool(path.clone(), window, cx))
                                    .ok();
                            },
                        ),
                    )
                    .item(
                        PopupMenuItem::new(t!("external.open_editor").into_owned()).on_click(
                            move |_, window, cx| {
                                let (this, path) = &edit;
                                this.update(cx, |w, cx| {
                                    let program = external::Program::Editor;
                                    w.open_working_file(path.as_slice(), program, window, cx)
                                })
                                .ok();
                            },
                        ),
                    )
                })
                .into_any_element()
            })
            .collect();

        let merge = operation == Operation::Merge;
        let (hint_title, hint) = if merge {
            (
                t!("conflict.panel_hint_title_merge"),
                t!("conflict.panel_hint_merge"),
            )
        } else {
            (
                t!("conflict.panel_hint_title_op"),
                t!("conflict.panel_hint_op"),
            )
        };
        let card = v_flex()
            .flex_shrink_0()
            .mx(z(12.))
            .my(z(8.))
            .p(z(12.))
            .gap(z(4.))
            .rounded(m.radius)
            .bg(p.panel2)
            .border_1()
            .border_color(p.line)
            .text_size(m.text.small)
            .line_height(relative(1.5))
            .text_color(p.fg2)
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.fg)
                    .child(hint_title.into_owned()),
            )
            .child(div().child(hint.into_owned()));

        let label = match (merge, unresolved) {
            (true, 0) => t!("conflict.commit_merge"),
            (true, count) => tn!("conflict.panel_commit_unresolved", count = count),
            (false, 0) => t!("op.continue"),
            (false, count) => tn!("conflict.panel_continue_unresolved", count = count),
        }
        .into_owned();
        let bottom = v_flex()
            .flex_shrink_0()
            .p(z(12.))
            .gap(z(8.))
            .border_t_1()
            .border_color(p.line)
            // The kit's own `Input::h` sizes only multi-line inputs.
            .when(merge, |this| {
                this.child(Styled::h(Input::new(&self.conflict_panel.message), z(32.)).bg(p.bg))
            })
            .child(
                Button::new("conflict-panel-commit")
                    .primary()
                    .h(z(34.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .label(label)
                    .disabled(!self.conflict_commit_ready(cx))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if merge {
                            this.commit_merge(window, cx);
                        } else {
                            this.run_operation(operation, OperationAction::Continue, window, cx);
                        }
                    })),
            );

        v_flex()
            .size_full()
            .text_size(m.text.ui)
            .child(
                h_flex()
                    .h(m.section)
                    .flex_shrink_0()
                    .px(z(12.))
                    .gap(z(8.))
                    .border_b_1()
                    .border_color(p.line)
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(t!("conflict.panel_title").into_owned()),
                    )
                    .child(widgets::section_count(set.files.len(), &p, &m))
                    .child(div().flex_1())
                    .child(link(
                        "conflict-panel-all",
                        t!("conflict.panel_all").into_owned(),
                        p.fg3,
                        m.text.small,
                        cx.listener(|this, _, _, cx| {
                            this.conflict_panel.show_all = true;
                            cx.notify();
                        }),
                    )),
            )
            .child(
                v_flex()
                    .id("conflict-panel-files")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(v_flex().flex_shrink_0().p(z(6.)).children(rows))
                    .child(card),
            )
            .child(bottom)
            .into_any_element()
    }

    /// Writes (or removes) the record of the files an operation stopped on, in its own
    /// lane: a newer record replaces one still waiting.
    pub(super) fn save_conflict_record(
        &mut self,
        record: Option<ConflictRecord>,
        cx: &mut Context<Self>,
    ) {
        let RepoState::Loaded(repo) = &self.state else {
            return;
        };
        let Some(workdir) = repo.workdir.clone() else {
            return;
        };
        let file = conflicts::conflicts_file(&workdir);
        let job = self
            .jobs
            .spawn(Lane::Coalesce("conflict-record"), move |_| {
                let written = match &record {
                    Some(record) => conflicts::write(&file, record),
                    None => conflicts::remove(&file),
                };
                written
                    .err()
                    .map(|err| format!("{}: {err}", file.display()))
            });
        cx.spawn(async move |this, cx| {
            // Without the record, a restart forgets which files the operation stopped on.
            if let Some(Some(err)) = job.join().await {
                let text = t!("conflict.record_failed", error = err).into_owned();
                this.update(cx, |_, cx| crate::tab_notes::note_failure(text, cx))
                    .ok();
            }
        })
        .detach();
    }

    /// Whether the panel's button can go: nothing is left conflicted, no remote operation
    /// runs, and a merge has a message.
    fn conflict_commit_ready(&self, cx: &App) -> bool {
        let Some(operation) = self.conflict_panel.active() else {
            return false;
        };
        let unresolved = self
            .status
            .as_ref()
            .map_or(0, |status| status.conflicts().count());
        unresolved == 0
            && self.remote_op.is_none()
            && (operation != Operation::Merge || !self.conflict_panel.message_text(cx).is_empty())
    }

    /// Commits the merge with the panel's message: git's prepared one, as edited here.
    /// Undo takes the merge back like "Continue" would.
    pub(super) fn commit_merge(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let message = self.conflict_panel.message_text(cx);
        if message.is_empty() {
            return;
        }
        let name = operation_label(Operation::Merge);
        let done = t!("op.continued", op = name).into_owned();
        let record = Record::checked_out(t!("undo.op_merge_commit").into_owned(), Worktree::Follow);
        self.run_recorded_op(
            t!("op.failed", op = name).into_owned(),
            None,
            Some(record),
            move |git, cwd, cancel| {
                gitweft_git::ops::commit(git, cwd, &message, CommitOptions::default(), cancel)?;
                Ok(done)
            },
            window,
            cx,
        );
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use gitweft_git::ObjectId;
    use gitweft_git::gix::bstr::BString;
    use gitweft_git::merge::Operation;

    use gitweft_core::conflicts::ConflictRecord;

    use super::{
        ConflictSet, WholeNote, looks_binary, recorded_files, split_message, step_name,
        wanted_record,
    };

    #[test]
    fn the_record_follows_the_step_and_a_stale_one_is_ignored() {
        let head = ObjectId::from_hex(b"0123456789abcdef0123456789abcdef01234567").unwrap();
        let merge = Some((Operation::Merge, Some(head)));
        assert_eq!(
            step_name(merge).as_deref(),
            Some("merge 0123456789abcdef0123456789abcdef01234567")
        );
        assert_eq!(
            step_name(Some((Operation::CherryPick, None))).as_deref(),
            Some("cherry-pick -")
        );
        assert_eq!(step_name(None), None);

        let mut set = ConflictSet::default();
        assert_eq!(wanted_record(&set), None);
        // After a restart the kept files come first, resolved ones included.
        let kept = ConflictRecord {
            step: step_name(merge).unwrap(),
            files: vec![b"a".to_vec(), b"\xffb".to_vec()],
        };
        let mut conflicted = recorded_files(Some(kept.clone()), merge);
        conflicted.extend(paths(&["c", "a"]));
        set.update(merge, conflicted, HashMap::new());
        assert_eq!(
            set.files,
            vec![
                BString::from("a"),
                BString::from(&b"\xffb"[..]),
                BString::from("c")
            ]
        );
        assert_eq!(
            wanted_record(&set).unwrap().files,
            vec![b"a".to_vec(), b"\xffb".to_vec(), b"c".to_vec()]
        );
        // Another step's record is stale.
        let rebase = Some((Operation::Rebase, Some(head)));
        assert!(recorded_files(Some(kept), rebase).is_empty());
        assert!(recorded_files(None, merge).is_empty());
        // No operation: nothing to keep.
        set.update(None, paths(&[]), HashMap::new());
        assert_eq!(wanted_record(&set), None);
    }

    #[test]
    fn whole_file_conflicts_from_the_status_code_and_the_content() {
        assert_eq!(WholeNote::from_code(*b"DU"), Some(WholeNote::DeletedHere));
        assert_eq!(WholeNote::from_code(*b"UD"), Some(WholeNote::DeletedByThem));
        assert_eq!(WholeNote::from_code(*b"AU"), Some(WholeNote::OnlyHere));
        assert_eq!(WholeNote::from_code(*b"UA"), Some(WholeNote::OnlyTheirs));
        assert_eq!(WholeNote::from_code(*b"DD"), Some(WholeNote::DeletedBoth));
        assert_eq!(WholeNote::from_code(*b"UU"), None);
        assert_eq!(WholeNote::from_code(*b"AA"), None);
        assert!(looks_binary(b"PNG\0data"));
        assert!(!looks_binary("plain text, ünïcode".as_bytes()));
        // Only the first 8000 bytes count, as in git.
        let mut late = vec![b'a'; 9000];
        late[8500] = 0;
        assert!(!looks_binary(&late));
    }

    fn paths(names: &[&str]) -> Vec<BString> {
        names.iter().map(|name| BString::from(*name)).collect()
    }

    #[test]
    fn resolved_files_stay_listed_until_the_step_changes() {
        let mut set = ConflictSet::default();
        let merge = Some((Operation::Merge, None));
        let counts = HashMap::from([(BString::from("b"), 2)]);
        assert!(set.update(merge, paths(&["a", "b"]), HashMap::new()));
        // "a" was resolved: it stays, in its place.
        assert!(!set.update(merge, paths(&["b"]), counts.clone()));
        assert_eq!(set.files, paths(&["a", "b"]));
        assert_eq!(set.counts, counts);
        // A file conflicted later joins at the end.
        set.update(merge, paths(&["c", "b"]), HashMap::new());
        assert_eq!(set.files, paths(&["a", "b", "c"]));

        // The next step of a rebase (HEAD moved) starts a new list.
        let head = ObjectId::from_hex(b"0123456789abcdef0123456789abcdef01234567").unwrap();
        let step = Some((Operation::Rebase, Some(head)));
        assert!(set.update(step, paths(&["d"]), HashMap::new()));
        assert_eq!(set.files, paths(&["d"]));

        // The operation is over: nothing is listed, whatever the status says.
        assert!(set.update(None, paths(&["d"]), counts));
        assert!(set.files.is_empty() && set.counts.is_empty());
        assert!(!set.update(None, paths(&["e"]), HashMap::new()));
        assert!(set.files.is_empty());
    }

    #[test]
    fn a_prepared_message_splits_into_its_summary_and_body() {
        assert_eq!(
            split_message("Merge branch 'topic'\n"),
            ("Merge branch 'topic'".to_owned(), String::new())
        );
        assert_eq!(
            split_message("Merge branch 'topic'\n\n* topic:\n  Fix it\n"),
            (
                "Merge branch 'topic'".to_owned(),
                "* topic:\n  Fix it".to_owned()
            )
        );
        assert_eq!(split_message(""), (String::new(), String::new()));
    }
}
