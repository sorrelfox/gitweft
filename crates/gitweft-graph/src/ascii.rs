//! Text rendering of a layout, for tests and debugging.
//!
//! One line per row: one character per lane (`*` node, `|` a line passing the row's
//! center, space otherwise), followed by the row's connectors: `<N` for a line merging in
//! from lane N, `>N` for a line branching out to lane N.

use crate::layout::{ConnectorKind, Layout};

pub fn render_ascii(layout: &Layout) -> Vec<String> {
    let lanes = layout.lane_count() as usize;
    (0..layout.rows())
        .map(|row| {
            let center = 2 * row as u32 + 1;
            let node = layout.node(row);
            let mut cells = vec![' '; lanes];
            for span in layout.spans(row) {
                if span.from < center && span.to > center {
                    cells[span.lane as usize] = '|';
                }
            }
            cells[node.lane as usize] = '*';
            let mut line: String = cells.into_iter().collect::<String>().trim_end().to_owned();
            for connector in layout.connectors(row) {
                let arrow = match connector.kind {
                    ConnectorKind::MergeIn => '<',
                    ConnectorKind::BranchOut => '>',
                };
                line.push(' ');
                line.push(arrow);
                line.push_str(&connector.lane.to_string());
            }
            line
        })
        .collect()
}
