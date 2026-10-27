//! Keys for the lists that aren't the graph: the sidebar, the staging lists, the commit
//! details' files, the centre's file history, commit tree, reflog, trash, dangling commits
//! and comparison, and the code views' line cursor. One set of
//! actions, bound in each list's key context; the list's container (not its whole view: a
//! text field in it would lose Space) has the context. The staging lists also select files
//! by keyboard (Shift+arrows, Ctrl+A).
//!
//! A row's menu is a list of labels and the events they emit ([`event_menu`]), so the
//! right-click menu and Shift+F10's palette ([`event_commands`]) offer the same items.

use std::cell::Cell;
use std::rc::Rc;

use gitweft_ui::Metrics;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::*;

use crate::palette::PaletteCommand;
use crate::{
    blame_view, compare_view, conflict_view, dangling_view, diff_view, file_view, graph_view,
    history_view, rebase_view, reflog_view, trash_view, tree_view,
};

pub(crate) const SIDEBAR: &str = "Sidebar";
pub(crate) const STAGING: &str = "StagingList";
pub(crate) const DETAILS: &str = "DetailsFiles";

actions!(
    list,
    [
        NextRow,
        PreviousRow,
        FirstRow,
        LastRow,
        OpenRow,
        ToggleRow,
        DeleteRow,
        RowMenu,
        ExtendToNextRow,
        ExtendToPreviousRow,
        ExtendToFirstRow,
        ExtendToLastRow,
        SelectAllRows
    ]
);

/// The lists whose rows the arrows move through.
pub(crate) const LISTS: [&str; 14] = [
    SIDEBAR,
    STAGING,
    DETAILS,
    history_view::CONTEXT,
    tree_view::CONTEXT,
    reflog_view::CONTEXT,
    trash_view::CONTEXT,
    dangling_view::CONTEXT,
    compare_view::CONTEXT,
    diff_view::CONTEXT,
    file_view::CONTEXT,
    blame_view::CONTEXT,
    conflict_view::PANES,
    rebase_view::CONTEXT,
];
/// Where Shift with the arrows, Home and End selects rows: the staging lists' files, the
/// diff's lines.
pub(crate) const EXTEND_LISTS: [&str; 2] = [STAGING, diff_view::CONTEXT];
/// Where Space acts on the row: stages or unstages it, ticks a compared file viewed, or
/// picks a conflict's line.
pub(crate) const TOGGLE_LISTS: [&str; 3] = [STAGING, compare_view::CONTEXT, conflict_view::PANES];
/// Where Delete acts on the row: discards a change, or deletes a trash entry (both ask).
pub(crate) const DELETE_LISTS: [&str; 2] = [STAGING, trash_view::CONTEXT];
/// Where Shift+F10 opens the row's actions: the lists and the graph.
pub(crate) const MENU_LISTS: [&str; 12] = [
    SIDEBAR,
    STAGING,
    DETAILS,
    history_view::CONTEXT,
    tree_view::CONTEXT,
    reflog_view::CONTEXT,
    trash_view::CONTEXT,
    dangling_view::CONTEXT,
    compare_view::CONTEXT,
    blame_view::CONTEXT,
    rebase_view::CONTEXT,
    graph_view::CONTEXT,
];

pub(crate) fn bind_keys(cx: &mut App) {
    for context in LISTS {
        let context = Some(context);
        cx.bind_keys([
            KeyBinding::new("down", NextRow, context),
            KeyBinding::new("up", PreviousRow, context),
            KeyBinding::new("home", FirstRow, context),
            KeyBinding::new("end", LastRow, context),
            KeyBinding::new("enter", OpenRow, context),
        ]);
    }
    for context in MENU_LISTS {
        cx.bind_keys([
            KeyBinding::new("menu", RowMenu, Some(context)),
            KeyBinding::new("shift-f10", RowMenu, Some(context)),
        ]);
    }
    for context in TOGGLE_LISTS {
        cx.bind_keys([KeyBinding::new("space", ToggleRow, Some(context))]);
    }
    for context in DELETE_LISTS {
        cx.bind_keys([KeyBinding::new("delete", DeleteRow, Some(context))]);
    }
    for context in EXTEND_LISTS {
        let context = Some(context);
        cx.bind_keys([
            KeyBinding::new("shift-down", ExtendToNextRow, context),
            KeyBinding::new("shift-up", ExtendToPreviousRow, context),
            KeyBinding::new("shift-home", ExtendToFirstRow, context),
            KeyBinding::new("shift-end", ExtendToLastRow, context),
        ]);
    }
    cx.bind_keys([KeyBinding::new("ctrl-a", SelectAllRows, Some(STAGING))]);
}

/// Where a key moves a list's cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    Next,
    Previous,
    First,
    Last,
}

/// The row the cursor moves to in a list of `len` rows (`None`: the list is empty). With no
/// cursor yet, Next and First start at the top, Previous and Last at the bottom; a cursor
/// left past the end (the list shrank) comes back to the last row.
pub(crate) fn step(len: usize, current: Option<usize>, to: Step) -> Option<usize> {
    let last = len.checked_sub(1)?;
    Some(match (to, current) {
        (Step::First, _) | (Step::Next, None) => 0,
        (Step::Last, _) | (Step::Previous, None) => last,
        (Step::Next, Some(at)) => (at + 1).min(last),
        (Step::Previous, Some(at)) => at.saturating_sub(1).min(last),
    })
}

/// The cursor's mark on a row: an accent bar at its left edge (the row must be `relative`).
pub(crate) fn cursor_bar(color: Hsla, m: &Metrics) -> Div {
    div()
        .absolute()
        .left_0()
        .top(m.dp(6.))
        .bottom(m.dp(6.))
        .w(m.dp(2.))
        .rounded_full()
        .bg(color)
}

/// [`cursor_bar`] for a list whose rows aren't its scrolling container's direct children
/// (the sidebar's sections): while `reveal` is set, the frame that draws the cursor's row
/// scrolls `scroll` just enough to show the row, then clears it.
pub(crate) fn revealing_cursor_bar(
    color: Hsla,
    m: &Metrics,
    scroll: &ScrollHandle,
    reveal: &Rc<Cell<bool>>,
) -> Div {
    let (scroll, reveal) = (scroll.clone(), reveal.clone());
    div()
        .absolute()
        .inset_0()
        .child(cursor_bar(color, m))
        .child(
            canvas(
                move |row, window, _| {
                    if reveal.take()
                        && let Some(offset) = reveal_offset(scroll.bounds(), row, scroll.offset())
                    {
                        scroll.set_offset(offset);
                        // The layout is done for this frame: the next one shows the new offset.
                        window.request_animation_frame();
                    }
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
}

/// The scroll offset that brings `row` into `view` (both in window coordinates, `offset`
/// as a `ScrollHandle` has it: zero at the top, negative further down), moving as little as
/// possible; `None` when it's already there.
pub(crate) fn reveal_offset(
    view: Bounds<Pixels>,
    row: Bounds<Pixels>,
    offset: Point<Pixels>,
) -> Option<Point<Pixels>> {
    let shift = if row.top() < view.top() {
        view.top() - row.top()
    } else if row.bottom() > view.bottom() {
        view.bottom() - row.bottom()
    } else {
        return None;
    };
    Some(point(offset.x, (offset.y + shift).min(px(0.))))
}

/// A row's menu from its items (each label and the event it emits from `view`).
pub(crate) fn event_menu<V, E>(
    menu: PopupMenu,
    view: &WeakEntity<V>,
    items: &[(String, E)],
) -> PopupMenu
where
    V: EventEmitter<E> + 'static,
    E: Clone + 'static,
{
    items.iter().fold(menu, |menu, (label, event)| {
        let (view, event) = (view.clone(), event.clone());
        menu.item(PopupMenuItem::new(label.clone()).on_click(move |_, _, cx| {
            view.update(cx, |_, cx| cx.emit(event.clone())).ok();
        }))
    })
}

/// The same items as palette commands (Shift+F10), under `group`, with what they act on
/// as the detail.
pub(crate) fn event_commands<V, E>(
    view: &WeakEntity<V>,
    group: &str,
    detail: &str,
    items: Vec<(String, E)>,
) -> Vec<PaletteCommand>
where
    V: EventEmitter<E> + 'static,
    E: Clone + 'static,
{
    let group = SharedString::from(group.to_owned());
    items
        .into_iter()
        .map(|(label, event)| {
            let view = view.clone();
            PaletteCommand::new(group.clone(), label, move |_, cx| {
                view.update(cx, |_, cx| cx.emit(event.clone())).ok();
            })
            .detail(detail)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Step, reveal_offset, step};
    use gpui_kit::{Bounds, point, px, size};

    #[test]
    fn the_cursor_stays_in_the_list() {
        assert_eq!(step(0, None, Step::Next), None);
        assert_eq!(step(3, None, Step::Next), Some(0));
        assert_eq!(step(3, None, Step::Previous), Some(2));
        assert_eq!(step(3, Some(2), Step::Next), Some(2));
        assert_eq!(step(3, Some(0), Step::Previous), Some(0));
        assert_eq!(step(3, Some(1), Step::Last), Some(2));
        assert_eq!(step(3, Some(1), Step::First), Some(0));
        // The list shrank under the cursor.
        assert_eq!(step(2, Some(5), Step::Previous), Some(1));
        assert_eq!(step(2, Some(5), Step::Next), Some(1));
    }

    #[test]
    fn a_row_out_of_view_is_scrolled_in_as_little_as_possible() {
        // A 200 px view at y 100, scrolled 50 px down.
        let view = Bounds::new(point(px(0.), px(100.)), size(px(100.), px(200.)));
        let row = |top: f32| Bounds::new(point(px(0.), px(top)), size(px(100.), px(30.)));
        let offset = point(px(0.), px(-50.));
        // Inside: nothing to do.
        assert_eq!(reveal_offset(view, row(100.), offset), None);
        assert_eq!(reveal_offset(view, row(270.), offset), None);
        // Below: scrolled down until its bottom is the view's.
        assert_eq!(
            reveal_offset(view, row(290.), offset),
            Some(point(px(0.), px(-70.)))
        );
        // Above: scrolled up until its top is the view's, never past the start.
        assert_eq!(
            reveal_offset(view, row(80.), offset),
            Some(point(px(0.), px(-30.)))
        );
        assert_eq!(
            reveal_offset(view, row(0.), offset),
            Some(point(px(0.), px(0.)))
        );
    }
}
