//! Centre panel editing an interactive rebase: the commits after a base, oldest first
//! (the order git applies them), each with an action (pick, reword, squash, fixup,
//! edit, drop), and optionally a pause after it. ↑/↓ on a row, Alt+↑/↓ on the selected
//! one, or dragging a row onto another move it; a reword edits the message in place;
//! Autosquash moves `fixup!`/`squash!` commits under the commits they name. Start hands
//! the plan to the workspace, which runs it.
//!
//! By keyboard (#29): the arrows, Home and End move the selected row (shown in the
//! details), P/R/E/S/F/D set its action (git's todo letters; a reword's message box keeps
//! its letters: text input comes before bindings), Shift+F10 lists its commands, Escape
//! leaves a message box, then closes (asking when the plan was changed).

use std::cell::RefCell;
use std::rc::Rc;

use gitweft_git::ObjectId;
use gitweft_git::rebase::{
    Outcome, RebasePlan, TodoAction, TodoItem, TodoProblem, autosquash, check, combined_message,
    outcome as plan_outcome, squashes_into,
};
use gitweft_ui::current::mono_font;
use gitweft_ui::truncated::truncated;
use gitweft_ui::widgets::{centre_header, dot, list_row};
use gitweft_ui::{LookExt, Metrics, Palette};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{Disableable, Icon, Sizable, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::list_keys::{self, FirstRow, LastRow, NextRow, OpenRow, PreviousRow, RowMenu, Step};
use crate::palette::{PaletteCommand, view_command};

pub(crate) const CONTEXT: &str = "RebaseView";
const ACTIONS: [TodoAction; 6] = [
    TodoAction::Pick,
    TodoAction::Reword,
    TodoAction::Edit,
    TodoAction::Squash,
    TodoAction::Fixup,
    TodoAction::Drop,
];

actions!(
    rebase,
    [
        MoveUp,
        MoveDown,
        PickRow,
        RewordRow,
        EditRow,
        SquashRow,
        FixupRow,
        DropRow,
        CloseRebase
    ]
);

pub fn bind_keys(cx: &mut App) {
    let context = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("alt-up", MoveUp, context),
        KeyBinding::new("alt-down", MoveDown, context),
        // git's todo letters on the selected row (#29).
        KeyBinding::new("p", PickRow, context),
        KeyBinding::new("r", RewordRow, context),
        KeyBinding::new("e", EditRow, context),
        KeyBinding::new("s", SquashRow, context),
        KeyBinding::new("f", FixupRow, context),
        KeyBinding::new("d", DropRow, context),
        KeyBinding::new("escape", CloseRebase, context),
    ]);
}

pub enum RebaseEvent {
    Close,
    /// Show the commit's details in the right panel.
    SelectCommit(ObjectId),
    /// Run the edited plan.
    Start(RebasePlan),
}

impl EventEmitter<RebaseEvent> for RebaseView {}

impl Focusable for RebaseView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// Drag payload of a row: its index.
struct RowDrag(usize);

/// What follows the pointer while a row is dragged.
struct RowDragLabel(String);

impl Render for RowDragLabel {
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
            .text_size(m.text.xs)
            .child(self.0.clone())
    }
}

struct Row {
    item: TodoItem,
    /// The message box, made when the row first needs one: a reword, or a picked commit
    /// that squashes are folded into.
    editor: Option<Entity<TextareaState>>,
    /// What the box was last filled with (the commit's message and its squashes'): while
    /// its text is still that, it follows the squashes as they change.
    filled: Option<String>,
    /// A picked commit that squashes are folded into (#37): its box holds the message it
    /// ends up with, which it gets as a reword does.
    combined: bool,
}

pub struct RebaseView {
    /// The branch being rebased (or "detached at …").
    branch: String,
    /// `None`: from the first commit on.
    base: Option<ObjectId>,
    /// The branch's lane colour in the graph, for the preview of the result.
    lane: usize,
    original: Vec<ObjectId>,
    rows: Vec<Row>,
    selected: Option<usize>,
    /// The rows as drawn: only those on screen are built (#23). A reword's row grows with
    /// its message box, so they are measured rather than all one height.
    list: ListState,
    /// What `check` and `autosquash` make of the plan, worked out once per change (#23:
    /// each frame used to copy every item two or three times).
    status: RefCell<Option<PlanStatus>>,
    focus: FocusHandle,
    focus_on_render: bool,
    /// Redraws when a message box changes, so problems and Start follow the text.
    _editors: Vec<Subscription>,
}

impl RebaseView {
    pub fn new(branch: String, plan: RebasePlan, lane: usize, cx: &mut Context<Self>) -> Self {
        let rows: Vec<Row> = plan
            .items
            .into_iter()
            .map(|item| Row {
                item,
                editor: None,
                filled: None,
                combined: false,
            })
            .collect();
        let list = ListState::new(rows.len(), ListAlignment::Top, px(200.)) // unscaled: overdraw
            .with_uniform_item_height(cx.metrics().row);
        Self {
            branch,
            base: plan.base,
            lane,
            original: plan.original,
            rows,
            selected: None,
            list,
            status: RefCell::new(None),
            focus: cx.focus_handle(),
            focus_on_render: true,
            _editors: Vec::new(),
        }
    }

    /// The items as edited, each reword with the message as typed. A picked commit that
    /// squashes are folded into is a reword of its box's message: git's own combination is
    /// what the box starts with.
    /// Takes the keys again once a view shown on top of it closes.
    pub fn refocus(&mut self, cx: &mut Context<Self>) {
        self.focus_on_render = true;
        cx.notify();
    }

    fn items(&self, cx: &App) -> Vec<TodoItem> {
        self.rows
            .iter()
            .map(|row| {
                let mut item = row.item.clone();
                // An edit pauses at its commit already: a pause after it would stop again
                // at once.
                if item.action == TodoAction::Edit {
                    item.stop_after = false;
                }
                if (item.action == TodoAction::Reword || row.combined)
                    && let Some(editor) = &row.editor
                {
                    item.action = TodoAction::Reword;
                    item.message = editor.read(cx).value().trim().to_owned();
                }
                item
            })
            .collect()
    }

    /// The items as listed, each with its commit's own message: what autosquash matches
    /// and squash messages are made of.
    fn listed(&self) -> Vec<TodoItem> {
        self.rows.iter().map(|row| row.item.clone()).collect()
    }

    /// Gives each reword, and each picked commit that squashes are folded into, its message
    /// box, filled with the message it would end up with (its own, then its squashes'). A
    /// box whose text is still what was filled in is filled again as the squashes change;
    /// once edited, it keeps what was typed.
    fn sync_messages(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let listed = self.listed();
        for index in 0..self.rows.len() {
            let action = self.rows[index].item.action;
            let kept = matches!(action, TodoAction::Pick | TodoAction::Reword);
            let squashes = kept && !squashes_into(&listed, index).is_empty();
            self.rows[index].combined = action == TodoAction::Pick && squashes;
            if action != TodoAction::Reword && !squashes {
                continue;
            }
            let fill = combined_message(&listed, index, &listed[index].message);
            let row = &self.rows[index];
            let refill = match &row.editor {
                None => true,
                Some(editor) => row.filled.as_deref().is_some_and(|filled| {
                    filled != fill && editor.read(cx).value().trim() == filled.trim()
                }),
            };
            if !refill {
                continue;
            }
            match row.editor.clone() {
                Some(editor) => {
                    let text = fill.clone();
                    editor.update(cx, |editor, cx| editor.set_value(text, window, cx));
                }
                None => {
                    let editor = self.message_box(fill.clone(), window, cx);
                    self.rows[index].editor = Some(editor);
                }
            }
            self.rows[index].filled = Some(fill);
        }
        // Boxes came or went, in rows other than the one changed too.
        self.list.remeasure();
    }

    /// A message box holding `text`; its row grows with it.
    fn message_box(
        &mut self,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TextareaState> {
        let editor = cx.new(|cx| {
            let mut editor = TextareaState::new(window, cx).auto_grow(2, 12);
            editor.set_value(text, window, cx);
            editor
        });
        self._editors.push(cx.observe(&editor, |this, editor, cx| {
            // The box grows with its text: its row is measured again.
            let at = this
                .rows
                .iter()
                .position(|row| row.editor.as_ref() == Some(&editor));
            if let Some(at) = at {
                this.list.remeasure_items(at..at + 1);
            }
            this.changed(cx);
        }));
        editor
    }

    /// Whether row `index` shows its message box.
    fn boxed(&self, index: usize) -> bool {
        self.rows.get(index).is_some_and(|row| {
            row.editor.is_some() && (row.item.action == TodoAction::Reword || row.combined)
        })
    }

    /// Still the branch as it is: every commit picked, in the original order, no pauses.
    fn unchanged(&self) -> bool {
        self.rows.iter().zip(&self.original).all(|(row, id)| {
            row.item.commit == *id && row.item.action == TodoAction::Pick && !row.item.stop_after
        })
    }

    /// Whether the plan can start and what Autosquash would do, as of the last change.
    fn status(&self, cx: &App) -> PlanStatus {
        if let Some(status) = self.status.borrow().clone() {
            return status;
        }
        let items = self.items(cx);
        let listed = self.listed();
        let status = PlanStatus {
            problem: check(&items).err(),
            can_autosquash: autosquash(&listed) != listed,
            unchanged: self.unchanged(),
            outcome: Rc::new(plan_outcome(&items)),
        };
        *self.status.borrow_mut() = Some(status.clone());
        status
    }

    /// The plan changed: its status is worked out again, and the view drawn again.
    fn changed(&mut self, cx: &mut Context<Self>) {
        self.status.get_mut().take();
        cx.notify();
    }

    fn toggle_pause(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get_mut(index) {
            row.item.stop_after = !row.item.stop_after;
            self.selected = Some(index);
            self.changed(cx);
        }
    }

    /// A row dragged onto another takes its place; the rows between shift by one.
    fn move_to(&mut self, from: usize, to: usize, window: &mut Window, cx: &mut Context<Self>) {
        if from == to || from >= self.rows.len() || to >= self.rows.len() {
            return;
        }
        let row = self.rows.remove(from);
        self.rows.insert(to, row);
        self.selected = Some(to);
        // Squashes may have joined or left a commit: its box follows (and rows are measured
        // again, a reword's being taller).
        self.sync_messages(window, cx);
        self.changed(cx);
    }

    /// Git's autosquash on the list: `fixup!`, `squash!` and `amend!` commits move under
    /// the commit they name, as fixups or squashes. Rows keep their message boxes.
    fn apply_autosquash(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let items = self.listed();
        let squashed = autosquash(&items);
        if squashed == items {
            return;
        }
        let mut rows: Vec<Option<Row>> = std::mem::take(&mut self.rows)
            .into_iter()
            .map(Some)
            .collect();
        self.rows = squashed
            .into_iter()
            .filter_map(|item| {
                let slot = rows.iter_mut().find(|row| {
                    row.as_ref()
                        .is_some_and(|row| row.item.commit == item.commit)
                })?;
                let mut row = slot.take()?;
                row.item.action = item.action;
                Some(row)
            })
            .collect();
        self.selected = None;
        self.sync_messages(window, cx);
        self.changed(cx);
    }

    fn set_action(
        &mut self,
        index: usize,
        action: TodoAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Whether this row's message box has the keys: they come back to the rows when the
        // box goes (a focused element that disappears leaves nothing focused, #61).
        let box_focused = self
            .rows
            .get(index)
            .and_then(|row| row.editor.as_ref())
            .is_some_and(|editor| editor.focus_handle(cx).contains_focused(window, cx));
        let Some(row) = self.rows.get_mut(index) else {
            return;
        };
        row.item.action = action;
        // A reword's row grows with its message box, and shrinks back after; a squash gives
        // the commit it goes into one.
        self.sync_messages(window, cx);
        self.selected = Some(index);
        if box_focused && !self.boxed(index) {
            self.focus.focus(window, cx);
        }
        self.changed(cx);
    }

    fn move_row(&mut self, index: usize, up: bool, window: &mut Window, cx: &mut Context<Self>) {
        let target = if up {
            index.checked_sub(1)
        } else {
            (index + 1 < self.rows.len()).then_some(index + 1)
        };
        let Some(target) = target else {
            return;
        };
        self.rows.swap(index, target);
        self.selected = Some(target);
        self.sync_messages(window, cx);
        // Alt+↑/↓ keep the moved row in view.
        self.list.scroll_to_reveal_item(target);
        self.changed(cx);
    }

    fn move_up(&mut self, _: &MoveUp, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.selected {
            self.move_row(index, true, window, cx);
        }
    }

    fn move_down(&mut self, _: &MoveDown, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.selected {
            self.move_row(index, false, window, cx);
        }
    }

    /// The arrows, Home and End move the selected row, shown in the details as a click
    /// does, and keep it in view (#29).
    fn move_cursor(&mut self, to: Step, cx: &mut Context<Self>) {
        let Some(ix) = list_keys::step(self.rows.len(), self.selected, to) else {
            return;
        };
        self.selected = Some(ix);
        self.list.scroll_to_reveal_item(ix);
        cx.emit(RebaseEvent::SelectCommit(self.rows[ix].item.commit));
        cx.notify();
    }

    /// Enter: the selected row's commit in the details.
    fn show_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(row) = self.selected.and_then(|ix| self.rows.get(ix)) {
            cx.emit(RebaseEvent::SelectCommit(row.item.commit));
        }
    }

    /// P/R/E/S/F/D: the selected row's action; a reword's message box takes the keys, to
    /// type the message.
    fn set_selected_action(
        &mut self,
        action: TodoAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A message box has the keys: its letters are text, not actions (GPUI runs bindings
        // before text input for these keys; passing the key on lets the box take it).
        if !self.focus.is_focused(window) {
            cx.propagate();
            return;
        }
        let Some(index) = self.selected else {
            return;
        };
        self.set_action(index, action, window, cx);
        if action == TodoAction::Reword
            && let Some(editor) = self.rows.get(index).and_then(|row| row.editor.clone())
        {
            editor.update(cx, |editor, cx| editor.focus(window, cx));
        }
    }

    /// Escape: out of a message box first (back to the rows); then closes, asking first
    /// when the plan was changed (Enter answers Cancel, as on every warning, #13).
    fn escape(&mut self, _: &CloseRebase, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus.is_focused(window) {
            self.focus.focus(window, cx);
            cx.notify();
            return;
        }
        if self.status(cx).unchanged {
            cx.emit(RebaseEvent::Close);
            return;
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            &t!("rebase.close_ask"),
            Some(&t!("rebase.close_ask_detail")),
            &[
                PromptButton::ok(t!("rebase.close_discard").into_owned()),
                PromptButton::cancel(t!("staging.cancel").into_owned()),
            ],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await == Ok(0) {
                this.update(cx, |_, cx| cx.emit(RebaseEvent::Close)).ok();
            }
        })
        .detach();
    }

    /// Shift+F10: the selected row's commands in the palette.
    fn row_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.selected else {
            return;
        };
        let group: SharedString = t!("palette.group.rebase").into_owned().into();
        let this = cx.entity().downgrade();
        let mut commands = self.move_commands(&this, &group, index);
        commands.extend(self.row_commands(&this, &group, index));
        crate::palette::open(commands, window, cx);
    }

    /// A row as the palette's detail: its commit and summary.
    fn row_detail(&self, index: usize) -> String {
        let row = &self.rows[index];
        format!(
            "{} {}",
            row.item.commit.to_hex_with_len(8),
            row.item.summary()
        )
    }

    /// Moving row `index` up and down, where it can go.
    fn move_commands(
        &self,
        this: &WeakEntity<Self>,
        group: &SharedString,
        index: usize,
    ) -> Vec<PaletteCommand> {
        let mut commands = Vec::new();
        if index > 0 {
            let label = t!("palette.rebase_move_up");
            let command = view_command(this, group, label, move |this, window, cx| {
                this.move_row(index, true, window, cx)
            });
            commands.push(
                command
                    .detail(self.row_detail(index))
                    .key(MoveUp, Some(CONTEXT)),
            );
        }
        if index + 1 < self.rows.len() {
            let label = t!("palette.rebase_move_down");
            let command = view_command(this, group, label, move |this, window, cx| {
                this.move_row(index, false, window, cx)
            });
            commands.push(
                command
                    .detail(self.row_detail(index))
                    .key(MoveDown, Some(CONTEXT)),
            );
        }
        commands
    }

    /// Row `index`'s other actions and its pause, with the commit as the detail.
    fn row_commands(
        &self,
        this: &WeakEntity<Self>,
        group: &SharedString,
        index: usize,
    ) -> Vec<PaletteCommand> {
        let row = &self.rows[index];
        let shown = self.row_detail(index);
        let mut commands = Vec::new();
        for action in ACTIONS.into_iter().filter(|a| *a != row.item.action) {
            let command = view_command(
                this,
                group,
                action_label(action),
                move |this, window, cx| this.set_action(index, action, window, cx),
            );
            commands.push(command.detail(shown.clone()));
        }
        // An edit pauses at its commit already.
        if row.item.action != TodoAction::Edit {
            let label = if row.item.stop_after {
                t!("palette.rebase_no_pause")
            } else {
                t!("palette.rebase_pause")
            };
            let command = view_command(this, group, label, move |this, _, cx| {
                this.toggle_pause(index, cx)
            });
            commands.push(command.detail(shown));
        }
        commands
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        let items = self.items(cx);
        if check(&items).is_err() || self.unchanged() {
            return;
        }
        cx.emit(RebaseEvent::Start(RebasePlan {
            base: self.base,
            original: self.original.clone(),
            items,
        }));
    }

    /// The editor's buttons as palette commands: Start and Autosquash when they'd do
    /// something, moving the selected row, then each row's other actions and its pause
    /// (the commit as the detail), and Close.
    pub(crate) fn palette_commands(
        &self,
        this: &WeakEntity<Self>,
        cx: &App,
    ) -> Vec<PaletteCommand> {
        let group: SharedString = t!("palette.group.rebase").into_owned().into();
        let mut commands = Vec::new();
        let status = self.status(cx);
        if status.problem.is_none() && !status.unchanged {
            let label = t!("rebase.start");
            commands.push(view_command(this, &group, label, |this, _, cx| {
                this.start(cx)
            }));
        }
        if status.can_autosquash {
            let label = t!("rebase.autosquash");
            commands.push(view_command(this, &group, label, |this, window, cx| {
                this.apply_autosquash(window, cx)
            }));
        }
        if let Some(index) = self.selected.filter(|&ix| ix < self.rows.len()) {
            commands.extend(self.move_commands(this, &group, index));
        }
        for index in 0..self.rows.len() {
            commands.extend(self.row_commands(this, &group, index));
        }
        let label = t!("palette.rebase_close");
        commands.push(view_command(this, &group, label, |_, _, cx| {
            cx.emit(RebaseEvent::Close)
        }));
        commands
    }

    fn render_row(
        &self,
        index: usize,
        p: &Palette,
        m: &Metrics,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let z = |value: f32| px(value * m.zoom);
        let row = &self.rows[index];
        let action = row.item.action;
        let id = row.item.commit;
        let view = cx.entity().downgrade();
        let last = self.rows.len() - 1;
        // A reword shows its message box, and the row grows with it; so does a picked commit
        // that squashes go into.
        let combined = row.combined;
        let editor = row
            .editor
            .as_ref()
            .filter(|_| action == TodoAction::Reword || combined);
        let open = editor.is_some();
        // The fixed cells are as high as the kit's xsmall buttons and the box's lines
        // (1.25 rem). In a row that grows they sit on the box's first line, below its
        // border and the kit's top padding for the box's (default) size, which doesn't zoom.
        let cell = m.rem * 1.25;
        let top = if open {
            gpui_kit::component::Size::default().input_py() + z(1.)
        } else {
            Pixels::ZERO
        };

        let arrow = |name: &'static str, icon: IconName, up: bool, disabled: bool| {
            let tooltip = if up {
                t!("rebase.move_up")
            } else {
                t!("rebase.move_down")
            };
            Button::new((name, index))
                .ghost()
                .xsmall()
                .icon(Icon::from(icon))
                .tooltip(tooltip.into_owned())
                .disabled(disabled)
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.move_row(index, up, window, cx);
                }))
        };
        let picker = Button::new(("rebase-action", index))
            .outline()
            .xsmall()
            .w(z(84.))
            .mt(top)
            .label(action_label(action))
            .dropdown_menu(move |menu, _, _| {
                ACTIONS.iter().fold(menu, |menu, &choice| {
                    let view = view.clone();
                    menu.item(
                        PopupMenuItem::new(action_label(choice))
                            .checked(choice == action)
                            .on_click(move |_, window, cx| {
                                view.update(cx, |this, cx| {
                                    this.set_action(index, choice, window, cx)
                                })
                                .ok();
                            }),
                    )
                })
            });

        let paused = row.item.stop_after && action != TodoAction::Edit;
        let pause = Button::new(("rebase-pause", index))
            .xsmall()
            .mt(top)
            .flex_shrink_0()
            .label(t!("rebase.pause").into_owned())
            .tooltip(t!("rebase.pause_tip").into_owned())
            .disabled(action == TodoAction::Edit)
            .when(paused, |button| button.primary())
            .when(!paused, |button| button.ghost())
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.toggle_pause(index, cx);
            }));
        let drop_bg = p.acc.opacity(0.25);
        let summary = row.item.summary().to_owned();

        let text = match editor {
            Some(editor) => v_flex()
                .flex_1()
                .min_w_0()
                .gap(z(2.))
                .child(Textarea::new(editor).bg(p.bg))
                .when(combined, |this| {
                    this.child(
                        div()
                            .text_size(m.text.xs)
                            .text_color(p.fg3)
                            .child(t!("rebase.combined_note").into_owned()),
                    )
                })
                .into_any_element(),
            None => {
                let dropped = action == TodoAction::Drop;
                truncated("subject", row.item.summary().to_owned())
                    .flex_1()
                    .text_color(if dropped { p.fg3 } else { p.fg })
                    .when(dropped, |this| this.line_through())
                    .into_any_element()
            }
        };

        list_row(("rebase-row", index), self.selected == Some(index), p, m)
            // How view tests find the row (a no-op outside them).
            .debug_selector(|| format!("rebase-row-{index}"))
            .when(open, |this| {
                this.h_auto().min_h(m.row).py(z(4.)).items_start()
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.selected = Some(index);
                cx.emit(RebaseEvent::SelectCommit(id));
                cx.notify();
            }))
            // Dragged onto another row, it takes that row's place.
            .on_drag(RowDrag(index), move |_, _, _, cx| {
                let label = summary.clone();
                cx.new(|_| RowDragLabel(label))
            })
            .drag_over::<RowDrag>(move |style, _, _, _| style.bg(drop_bg))
            .on_drop(cx.listener(move |this, drag: &RowDrag, window, cx| {
                this.move_to(drag.0, index, window, cx)
            }))
            // The two arrows read as one control.
            .child(
                h_flex()
                    .flex_shrink_0()
                    .gap(z(4.))
                    .mt(top)
                    .child(arrow("rebase-up", IconName::ArrowUp, true, index == 0))
                    .child(arrow(
                        "rebase-down",
                        IconName::ArrowDown,
                        false,
                        index == last,
                    )),
            )
            .child(picker)
            // Folded commits are marked as going into the one above.
            .child(
                div()
                    .w(z(14.))
                    .flex_shrink_0()
                    .mt(top)
                    .line_height(cell)
                    .text_color(p.fg3)
                    .child(if action.folds() { "↳" } else { "" }),
            )
            .child(
                div()
                    .w(z(72.))
                    .flex_shrink_0()
                    .mt(top)
                    .line_height(cell)
                    .font_family(mono_font())
                    .text_size(m.text.mono)
                    .text_color(p.fg3)
                    .child(id.to_hex_with_len(8).to_string()),
            )
            .child(text)
            .child(pause)
            .into_any_element()
    }
}

fn action_label(action: TodoAction) -> String {
    match action {
        TodoAction::Pick => t!("rebase.pick"),
        TodoAction::Reword => t!("rebase.reword"),
        TodoAction::Edit => t!("rebase.edit"),
        TodoAction::Squash => t!("rebase.squash"),
        TodoAction::Fixup => t!("rebase.fixup"),
        TodoAction::Drop => t!("rebase.drop"),
    }
    .into_owned()
}

fn problem_text(problem: TodoProblem) -> String {
    match problem {
        TodoProblem::NothingKept => t!("rebase.problem_nothing_kept"),
        TodoProblem::NothingToFoldInto(index) => t!("rebase.problem_fold", n = index + 1),
        TodoProblem::EmptyMessage(index) => t!("rebase.problem_empty", n = index + 1),
        TodoProblem::OtherCommits => t!("rebase.problem_other_commits"),
        TodoProblem::PathNotUtf8 => t!("rebase.problem_path"),
    }
    .into_owned()
}

impl Render for RebaseView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.focus_on_render) {
            self.focus.focus(window, cx);
        }
        let look = cx.look().clone();
        let (p, m) = (look.palette, look.metrics);
        let z = |value: f32| px(value * m.zoom);
        let PlanStatus {
            problem,
            can_autosquash,
            unchanged,
            outcome,
        } = self.status(cx);
        let count = self.rows.len();
        let title = match self.base {
            Some(base) => t!(
                "rebase.title",
                branch = self.branch.clone(),
                sha = base.to_hex_with_len(8)
            ),
            None => t!("rebase.title_root", branch = self.branch.clone()),
        }
        .into_owned();
        // One commit has no order to explain: its own text, not a plural form.
        let subtitle = if count == 1 {
            t!("rebase.subtitle_single")
        } else {
            tn!("rebase.subtitle_multiple", count = count)
        };

        let header = centre_header(title, subtitle.into_owned(), &p, &m)
            .child(
                Button::new("rebase-autosquash")
                    .small()
                    .ghost()
                    .label(t!("rebase.autosquash").into_owned())
                    .tooltip(t!("rebase.autosquash_tip").into_owned())
                    .disabled(!can_autosquash)
                    .on_click(cx.listener(|this, _, window, cx| this.apply_autosquash(window, cx))),
            )
            .child(
                Button::new("rebase-start")
                    .small()
                    .primary()
                    .label(t!("rebase.start").into_owned())
                    .disabled(problem.is_some() || unchanged)
                    .on_click(cx.listener(|this, _, _, cx| this.start(cx))),
            )
            .child(
                Button::new("rebase-close")
                    .small()
                    .ghost()
                    .label(t!("diff.close").into_owned())
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(RebaseEvent::Close))),
            );

        // Always one line, so the rows never shift under the pointer as it changes.
        let (note, note_color) = match problem {
            Some(problem) => (problem_text(problem), p.del),
            None if unchanged => (t!("rebase.unchanged").into_owned(), p.fg2),
            None => (t!("rebase.ready").into_owned(), p.fg2),
        };
        let note = h_flex()
            .h(m.header)
            .flex_shrink_0()
            .px(z(12.))
            .bg(p.panel2)
            .border_b_1()
            .border_color(p.line)
            .text_size(m.text.small)
            .text_color(note_color)
            .child(truncated("note", note).flex_1());
        let rows = cx.processor(|this, ix, _, cx| {
            let look = cx.look().clone();
            this.render_row(ix, &look.palette, &look.metrics, cx)
        });

        v_flex()
            .size_full()
            .text_size(m.text.ui)
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::move_up))
            .on_action(cx.listener(Self::move_down))
            .on_action(cx.listener(|this, _: &NextRow, _, cx| this.move_cursor(Step::Next, cx)))
            .on_action(
                cx.listener(|this, _: &PreviousRow, _, cx| this.move_cursor(Step::Previous, cx)),
            )
            .on_action(cx.listener(|this, _: &FirstRow, _, cx| this.move_cursor(Step::First, cx)))
            .on_action(cx.listener(|this, _: &LastRow, _, cx| this.move_cursor(Step::Last, cx)))
            .on_action(cx.listener(|this, _: &OpenRow, _, cx| this.show_selected(cx)))
            .on_action(cx.listener(|this, _: &RowMenu, window, cx| this.row_menu(window, cx)))
            .on_action(cx.listener(|this, _: &PickRow, window, cx| {
                this.set_selected_action(TodoAction::Pick, window, cx)
            }))
            .on_action(cx.listener(|this, _: &RewordRow, window, cx| {
                this.set_selected_action(TodoAction::Reword, window, cx)
            }))
            .on_action(cx.listener(|this, _: &EditRow, window, cx| {
                this.set_selected_action(TodoAction::Edit, window, cx)
            }))
            .on_action(cx.listener(|this, _: &SquashRow, window, cx| {
                this.set_selected_action(TodoAction::Squash, window, cx)
            }))
            .on_action(cx.listener(|this, _: &FixupRow, window, cx| {
                this.set_selected_action(TodoAction::Fixup, window, cx)
            }))
            .on_action(cx.listener(|this, _: &DropRow, window, cx| {
                this.set_selected_action(TodoAction::Drop, window, cx)
            }))
            .on_action(cx.listener(Self::escape))
            .child(header)
            .child(note)
            // Only the rows on screen are built (#23).
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(list(self.list.clone(), rows).size_full()),
            )
            .child(preview(
                &outcome,
                &self.branch,
                self.base,
                p.lanes[self.lane % p.lanes.len()],
                &p,
                &m,
            ))
    }
}

/// What the plan as edited comes to: what stops it from starting, whether Autosquash would
/// move anything, whether it's still the branch as it is, and what the branch becomes.
#[derive(Clone)]
struct PlanStatus {
    problem: Option<TodoProblem>,
    can_autosquash: bool,
    unchanged: bool,
    /// Oldest first; shared, as it's drawn every frame.
    outcome: Rc<Vec<Outcome>>,
}

/// The most commits the preview lists (the newest); the rest are counted.
const PREVIEW_ROWS: usize = 50;

/// The branch as the plan leaves it (#37), newest first as the graph shows it: one row per
/// commit kept, on the branch's lane, with how many are folded into it and where the rebase
/// stops, down to the base.
fn preview(
    outcome: &[Outcome],
    branch: &str,
    base: Option<ObjectId>,
    lane: Hsla,
    p: &Palette,
    m: &Metrics,
) -> impl IntoElement {
    let z = |value: f32| px(value * m.zoom);
    let half = m.row * 0.5;
    // A row: the lane (a line through it, a dot on it), its text, then its marks.
    let row = |dot_color: Option<Hsla>,
               up: bool,
               down: bool,
               text: String,
               text_color: Hsla,
               marks: Vec<String>| {
        let segment = |top: Pixels| {
            div()
                .absolute()
                .left(z(6.))
                .top(top)
                .w(z(2.))
                .h(half)
                .bg(lane)
        };
        h_flex()
            .h(m.row)
            .flex_shrink_0()
            .px(z(12.))
            .gap(z(8.))
            .child(
                div()
                    .relative()
                    .w(z(14.))
                    .h_full()
                    .flex_shrink_0()
                    .when(up, |this| this.child(segment(Pixels::ZERO)))
                    .when(down, |this| this.child(segment(half)))
                    .children(dot_color.map(|color| {
                        div()
                            .absolute()
                            .left(z(2.))
                            .top(half - z(5.))
                            .child(dot(color, z(10.)))
                    })),
            )
            .child(truncated("text", text).flex_1().text_color(text_color))
            .children(marks.into_iter().map(|mark| {
                div()
                    .flex_shrink_0()
                    .text_size(m.text.xs)
                    .text_color(p.fg3)
                    .child(mark)
            }))
    };
    let shown = outcome.len().min(PREVIEW_ROWS);
    let mut rows: Vec<AnyElement> = Vec::new();
    for (index, commit) in outcome.iter().rev().take(shown).enumerate() {
        let mut marks = Vec::new();
        if commit.folded > 0 {
            marks.push(tn!("rebase.preview_folded", count = commit.folded).into_owned());
        }
        if commit.stops {
            marks.push(t!("rebase.preview_stops").into_owned());
        }
        let text = commit.summary.clone();
        rows.push(row(Some(lane), index > 0, true, text, p.fg, marks).into_any_element());
    }
    if outcome.len() > shown {
        let more = tn!("rebase.preview_more", count = outcome.len() - shown).into_owned();
        rows.push(row(None, true, true, more, p.fg3, Vec::new()).into_any_element());
    }
    let (base_dot, base_text) = match base {
        Some(base) => (
            Some(p.fg3),
            t!("rebase.preview_base", sha = base.to_hex_with_len(8)),
        ),
        None => (None, t!("rebase.preview_root")),
    };
    let up = !outcome.is_empty();
    let base_text = base_text.into_owned();
    rows.push(row(base_dot, up, false, base_text, p.fg3, Vec::new()).into_any_element());
    let title = tn!(
        "rebase.preview_title",
        count = outcome.len(),
        branch = branch.to_owned()
    )
    .into_owned();
    v_flex()
        .flex_shrink_0()
        .max_h(m.row * 7.)
        .border_t_1()
        .border_color(p.line)
        .bg(p.panel2)
        .child(
            h_flex()
                .h(m.header)
                .flex_shrink_0()
                .px(z(12.))
                .text_size(m.text.small)
                .text_color(p.fg2)
                .child(title),
        )
        .child(
            div()
                .id("rebase-preview")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(v_flex().children(rows)),
        )
}

/// The editor in a GPUI test window (#51), driven by keystrokes through the app's own key
/// bindings, and judged by the events it emits.
#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::Path;

    use gitweft_ui::SettingsStore;
    // Rust's own `#[test]`: with `test-support` on, `gpui_kit::*` (through `super::*`)
    // brings GPUI's `test` attribute macro, which would take its place.
    use std::prelude::v1::test;

    use super::*;

    /// What the editor emitted.
    #[derive(Debug, PartialEq)]
    enum Seen {
        Close,
        Select(ObjectId),
        Start(RebasePlan),
    }

    fn id(byte: u8) -> ObjectId {
        ObjectId::from_bytes_or_panic(&[byte; 20])
    }

    /// The editor on commits c1 to c4 in a test window, with the app's key bindings; returns
    /// it, its window's context, and what it emits.
    fn editor<'a>(
        cx: &'a mut TestAppContext,
        dir: &Path,
    ) -> (
        Entity<RebaseView>,
        &'a mut VisualTestContext,
        Rc<RefCell<Vec<Seen>>>,
    ) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            SettingsStore::init_with_file(cx, &dir.join("settings.json"));
            crate::list_keys::bind_keys(cx);
            bind_keys(cx);
        });
        let items: Vec<TodoItem> = (1..=4)
            .map(|byte| TodoItem {
                commit: id(byte),
                message: format!("c{byte}\n\nbody"),
                action: TodoAction::Pick,
                stop_after: false,
            })
            .collect();
        let plan = RebasePlan {
            base: Some(id(9)),
            original: items.iter().map(|item| item.commit).collect(),
            items,
        };
        let seen = Rc::new(RefCell::new(Vec::new()));
        let (view, cx) = cx.add_window_view(|_, cx| RebaseView::new("main".into(), plan, 0, cx));
        let sink = seen.clone();
        cx.update(|_, cx| {
            cx.subscribe(&view, move |_, event: &RebaseEvent, _| {
                sink.borrow_mut().push(match event {
                    RebaseEvent::Close => Seen::Close,
                    RebaseEvent::SelectCommit(id) => Seen::Select(*id),
                    RebaseEvent::Start(plan) => Seen::Start(plan.clone()),
                })
            })
            .detach()
        });
        (view, cx, seen)
    }

    #[test]
    fn keys_move_the_cursor_set_actions_and_move_rows() {
        use TodoAction::*;
        let dir = tempfile::tempdir().unwrap();
        let mut cx = TestAppContext::single();
        let (view, cx, seen) = editor(&mut cx, dir.path());

        // c1, then c2 squashed into it; the last row, c4, dropped and moved above c3.
        cx.simulate_keystrokes("down");
        assert_eq!(seen.borrow().last(), Some(&Seen::Select(id(1))));
        cx.simulate_keystrokes("down s end d alt-up");
        cx.update(|_, cx| view.update(cx, |view, cx| view.start(cx)));

        let seen = seen.borrow();
        let Some(Seen::Start(plan)) = seen.last() else {
            panic!("no plan started: {seen:?}");
        };
        let shape: Vec<(u8, TodoAction)> = plan
            .items
            .iter()
            .map(|item| (item.commit.as_bytes()[0], item.action))
            .collect();
        // c1 takes c2's message as well: a reword of the combined message (the box's).
        assert_eq!(shape, [(1, Reword), (2, Squash), (4, Drop), (3, Pick)]);
        assert_eq!(plan.items[0].message, "c1\n\nbody\n\nc2\n\nbody");
    }

    #[test]
    fn escape_closes_a_plan_that_changed_nothing_without_asking() {
        let dir = tempfile::tempdir().unwrap();
        let mut cx = TestAppContext::single();
        let (_view, cx, seen) = editor(&mut cx, dir.path());
        cx.simulate_keystrokes("escape");
        assert_eq!(seen.borrow().last(), Some(&Seen::Close));
    }

    /// Drags row `from` onto row `to` as a hand would: pressed, moved past GPUI's drag
    /// threshold, over the target, released there. Draws a frame first, for the rows'
    /// places.
    fn drag(cx: &mut VisualTestContext, from: usize, to: usize) {
        const ROWS: [&str; 4] = [
            "rebase-row-0",
            "rebase-row-1",
            "rebase-row-2",
            "rebase-row-3",
        ];
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let mut middle = |index: usize| {
            cx.debug_bounds(ROWS[index])
                .expect("the row is drawn")
                .center()
        };
        let (start, end) = (middle(from), middle(to));
        let none = Modifiers::none();
        cx.simulate_mouse_down(start, MouseButton::Left, none);
        cx.simulate_mouse_move(start + point(px(0.), px(5.)), MouseButton::Left, none);
        cx.simulate_mouse_move(end, MouseButton::Left, none);
        cx.simulate_mouse_up(end, MouseButton::Left, none);
    }

    #[test]
    fn a_row_dragged_onto_another_takes_its_place() {
        let dir = tempfile::tempdir().unwrap();
        let mut cx = TestAppContext::single();
        let (view, cx, seen) = editor(&mut cx, dir.path());

        drag(cx, 3, 0); // c4 onto c1: first, the others one down
        drag(cx, 1, 2); // c1 onto c2: they swap
        cx.update(|_, cx| view.update(cx, |view, cx| view.start(cx)));

        let seen = seen.borrow();
        let Some(Seen::Start(plan)) = seen.last() else {
            panic!("no plan started: {seen:?}");
        };
        let order: Vec<u8> = plan
            .items
            .iter()
            .map(|item| item.commit.as_bytes()[0])
            .collect();
        assert_eq!(order, [4, 2, 1, 3]);
    }
}
