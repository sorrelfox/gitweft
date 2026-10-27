//! Commit graph lane layout.
//!
//! Feed commits in topological order (every child before its parents) to a
//! [`LayoutBuilder`]; it assigns each commit a lane and a color and produces the lines
//! connecting them as a [`Layout`] a renderer can query row by row.
//!
//! Rules:
//! * A commit's first parent continues in the commit's own lane and color, so mainlines
//!   stay straight.
//! * Lanes never shift sideways; a freed lane is reused by the next branch tip.
//! * Merge parents branch out to the right (joining an existing line if one already
//!   leads to that parent).
//! * Lines of sibling branches converge at their common parent, not earlier.
//!
//! Geometry uses *half-rows*: row `r` spans half-rows `2r..2r+2`, its node sits at the
//! center `2r+1`. A [`Span`] is a vertical line segment on one lane between two half-row
//! positions; a [`Connector`] is a curve inside one half of a row.

mod ascii;
mod layout;

pub use ascii::render_ascii;
pub use layout::{
    ColorId, Connector, ConnectorKind, LaneId, Layout, LayoutBuilder, Node, PALETTE_SIZE, Span,
};
