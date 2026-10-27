//! Keys for the code views (#28): the diff, the file and the blame, and the conflict tool's
//! Alt+Down/Up. The line cursor's keys are the lists' ([`crate::list_keys`]): the arrows,
//! Home and End (and in the diff, Shift with them to select lines). Here: a page up or down
//! and the text scrolled sideways in every code view, and the next or previous hunk (in a
//! blame: the next or previous commit's lines; in the conflict tool: the next or previous
//! conflict). None of these is a plain letter or Space: a review's comment box in the diff
//! would lose them.

use gpui_kit::*;

use crate::{blame_view, conflict_view, diff_view, file_view};

actions!(
    code,
    [
        NextPage,
        PreviousPage,
        ScrollLeft,
        ScrollRight,
        NextHunk,
        PreviousHunk
    ]
);

/// Where Page Up/Down and Left/Right work.
pub(crate) const CODE_VIEWS: [&str; 3] =
    [diff_view::CONTEXT, file_view::CONTEXT, blame_view::CONTEXT];
/// Where Alt+Down/Up go from hunk to hunk (in a blame: from one commit's lines to the next;
/// in the conflict tool: from conflict to conflict).
pub(crate) const HUNK_VIEWS: [&str; 3] = [
    diff_view::CONTEXT,
    blame_view::CONTEXT,
    conflict_view::CONTEXT,
];

pub(crate) fn bind_keys(cx: &mut App) {
    for context in CODE_VIEWS {
        let context = Some(context);
        cx.bind_keys([
            KeyBinding::new("pagedown", NextPage, context),
            KeyBinding::new("pageup", PreviousPage, context),
            KeyBinding::new("left", ScrollLeft, context),
            KeyBinding::new("right", ScrollRight, context),
        ]);
    }
    for context in HUNK_VIEWS {
        let context = Some(context);
        cx.bind_keys([
            KeyBinding::new("alt-down", NextHunk, context),
            KeyBinding::new("alt-up", PreviousHunk, context),
        ]);
    }
}

/// How many rows a page key moves: the rows that fit in `height`, less one so a row of the
/// last page stays in view; at least one.
pub(crate) fn page_rows(height: Pixels, row_height: Pixels) -> usize {
    if row_height <= Pixels::ZERO {
        return 1;
    }
    ((height / row_height).floor() as usize)
        .saturating_sub(1)
        .max(1)
}

/// Where a page key takes the cursor in `len` rows: `rows` further (`down`) or back, within
/// the rows; with no cursor yet, a page down from the top or the top itself. `None` without
/// rows.
pub(crate) fn page_target(
    len: usize,
    cursor: Option<usize>,
    down: bool,
    rows: usize,
) -> Option<usize> {
    let last = len.checked_sub(1)?;
    Some(match (cursor, down) {
        (None, true) => rows.min(last),
        (None, false) => 0,
        (Some(at), true) => (at + rows).min(last),
        (Some(at), false) => at.saturating_sub(rows),
    })
}

/// The first row of the next (`forward`) or previous block from `from`, a block being rows
/// that `same` says belong together (a blame's lines of one commit). Going back from inside a
/// block lands on its own first row. `None` past the ends.
pub(crate) fn block_row(
    len: usize,
    from: Option<usize>,
    forward: bool,
    same: impl Fn(usize, usize) -> bool,
) -> Option<usize> {
    let start_of = |ix: usize| {
        let mut first = ix;
        while first > 0 && same(first - 1, ix) {
            first -= 1;
        }
        first
    };
    match (from, forward) {
        _ if len == 0 => None,
        (None, true) => Some(0),
        (None, false) => Some(start_of(len - 1)),
        (Some(at), true) => (at + 1..len).find(|&ix| !same(ix, at)),
        (Some(at), false) => {
            let at = at.min(len - 1);
            let first = start_of(at);
            if first < at {
                Some(first)
            } else {
                first.checked_sub(1).map(start_of)
            }
        }
    }
}

/// The height of a code view's rows, as drawn last: the wrapped `list` or the plain one.
pub(crate) fn view_height(
    wrap: bool,
    uniform: &UniformListScrollHandle,
    list: &ListState,
) -> Pixels {
    if wrap {
        list.viewport_bounds().size.height
    } else {
        uniform.0.borrow().base_handle.bounds().size.height
    }
}

/// Scrolls whichever list is drawn by the least that shows row `ix`.
pub(crate) fn reveal(wrap: bool, uniform: &UniformListScrollHandle, list: &ListState, ix: usize) {
    if wrap {
        list.scroll_to_reveal_item(ix);
    } else {
        uniform.scroll_to_item(ix, ScrollStrategy::Nearest);
    }
}

/// How far Left/Right scroll the text: four characters of the code font.
pub(crate) fn sideways_step(window: &Window, cx: &App) -> Pixels {
    use gitweft_ui::LookExt as _;
    crate::code_scroll::char_width(window, cx.metrics().text.mono) * 4.
}

/// The cursor's outline over its row (drawn while the view has the focus).
pub(crate) fn cursor_ring(row: AnyElement, cx: &App) -> AnyElement {
    use gitweft_ui::LookExt as _;
    let ring = cx.palette().acc.opacity(0.7);
    div()
        .relative()
        .w_full()
        .child(row)
        .child(div().absolute().inset_0().border_1().border_color(ring))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{block_row, page_rows, page_target};
    use gpui_kit::px;

    #[test]
    fn a_page_keeps_one_row_of_the_last() {
        assert_eq!(page_rows(px(400.), px(20.)), 19);
        assert_eq!(page_rows(px(410.), px(20.)), 19);
        // Too small a view (or none drawn yet) still moves.
        assert_eq!(page_rows(px(10.), px(20.)), 1);
        assert_eq!(page_rows(px(0.), px(20.)), 1);
        assert_eq!(page_rows(px(400.), px(0.)), 1);
    }

    #[test]
    fn a_page_stays_in_the_rows() {
        assert_eq!(page_target(0, None, true, 10), None);
        assert_eq!(page_target(50, None, true, 10), Some(10));
        assert_eq!(page_target(50, None, false, 10), Some(0));
        assert_eq!(page_target(50, Some(45), true, 10), Some(49));
        assert_eq!(page_target(50, Some(5), false, 10), Some(0));
        assert_eq!(page_target(50, Some(30), false, 10), Some(20));
    }

    #[test]
    fn blocks_are_runs_of_one_owner() {
        // A blame's owners: commit 0 on lines 0-1, 1 on 2-4, 0 again on 5.
        let owners = [0, 0, 1, 1, 1, 0];
        let same = |a: usize, b: usize| owners[a] == owners[b];
        assert_eq!(block_row(6, None, true, same), Some(0));
        assert_eq!(block_row(6, None, false, same), Some(5));
        assert_eq!(block_row(6, Some(0), true, same), Some(2));
        assert_eq!(block_row(6, Some(3), true, same), Some(5));
        assert_eq!(block_row(6, Some(5), true, same), None);
        // Back from inside a block: its first line; from its first line: the block before.
        assert_eq!(block_row(6, Some(4), false, same), Some(2));
        assert_eq!(block_row(6, Some(2), false, same), Some(0));
        assert_eq!(block_row(6, Some(0), false, same), None);
        assert_eq!(block_row(0, None, true, same), None);
    }
}
