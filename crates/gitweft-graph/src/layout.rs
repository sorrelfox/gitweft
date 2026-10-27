use std::collections::{BTreeSet, HashMap};
use std::hash::Hash;

pub type LaneId = u16;
/// Index into the renderer's lane palette, `0..PALETTE_SIZE`.
pub type ColorId = u16;

/// Number of distinct lane colors the layout assigns.
pub const PALETTE_SIZE: ColorId = 8;

/// Rows per bucket of the span index.
const CHUNK_ROWS: u32 = 256;

/// A commit's position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Node {
    pub lane: LaneId,
    pub color: ColorId,
}

/// A vertical line on `lane` from half-row `from` to half-row `to` (`from < to`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub lane: LaneId,
    pub color: ColorId,
    pub from: u32,
    pub to: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectorKind {
    /// Upper half of the row: a line arrives from the top of `lane` into the node.
    MergeIn,
    /// Lower half of the row: a line leaves the node towards the bottom of `lane`.
    BranchOut,
}

/// A curve between the row's node lane and `lane`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Connector {
    pub kind: ConnectorKind,
    pub lane: LaneId,
    pub color: ColorId,
}

/// The finished layout.
#[derive(Debug, Clone, Default)]
pub struct Layout {
    nodes: Vec<Node>,
    spans: Vec<Span>,
    connectors: Vec<Connector>,
    /// `connectors[conn_offsets[r]..conn_offsets[r + 1]]` belong to row `r`.
    conn_offsets: Vec<u32>,
    /// Span ids touching bucket `k`: `chunk_spans[chunk_offsets[k]..chunk_offsets[k + 1]]`.
    chunk_offsets: Vec<u32>,
    chunk_spans: Vec<u32>,
    lane_count: LaneId,
}

impl Layout {
    pub fn rows(&self) -> usize {
        self.nodes.len()
    }

    /// Number of lanes used (the graph column must be this many lanes wide).
    pub fn lane_count(&self) -> LaneId {
        self.lane_count
    }

    pub fn node(&self, row: usize) -> Node {
        self.nodes[row]
    }

    pub fn connectors(&self, row: usize) -> &[Connector] {
        let start = self.conn_offsets[row] as usize;
        let end = self.conn_offsets[row + 1] as usize;
        &self.connectors[start..end]
    }

    /// Spans that intersect row `row` (clip them to `2*row..2*row+2` to draw).
    pub fn spans(&self, row: usize) -> impl Iterator<Item = &Span> + '_ {
        let row = row as u32;
        let chunk = (row / CHUNK_ROWS) as usize;
        let ids: &[u32] = match (
            self.chunk_offsets.get(chunk),
            self.chunk_offsets.get(chunk + 1),
        ) {
            (Some(&start), Some(&end)) => &self.chunk_spans[start as usize..end as usize],
            _ => &[],
        };
        let (top, bottom) = (2 * row, 2 * row + 2);
        ids.iter()
            .map(|&id| &self.spans[id as usize])
            .filter(move |s| s.from < bottom && s.to > top)
    }

    /// All spans, sorted by start position.
    pub fn all_spans(&self) -> &[Span] {
        &self.spans
    }
}

/// An active line leading down to a commit not yet placed (tracked in `waiting`).
struct Slot {
    color: ColorId,
    /// Half-row where the current straight segment began.
    from: u32,
    /// The line is a first-parent edge (preferred to keep its lane).
    first_parent: bool,
    /// The color came from a branch's hint, not from [`LayoutBuilder::fresh_color`].
    hinted: bool,
}

/// Streams commits (children before parents) into a [`Layout`].
pub struct LayoutBuilder<K> {
    slots: Vec<Option<Slot>>,
    /// Lanes whose slot leads to a given commit.
    waiting: HashMap<K, Vec<LaneId>>,
    free: BTreeSet<LaneId>,
    next_color: ColorId,
    layout: Layout,
}

impl<K: Copy + Eq + Hash> Default for LayoutBuilder<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Copy + Eq + Hash> LayoutBuilder<K> {
    pub fn new() -> Self {
        Self::with_capacity(0)
    }

    pub fn with_capacity(rows: usize) -> Self {
        let mut layout = Layout::default();
        layout.nodes.reserve(rows);
        layout.conn_offsets.reserve(rows + 1);
        layout.conn_offsets.push(0);
        Self {
            slots: Vec::new(),
            waiting: HashMap::with_capacity(64),
            free: BTreeSet::new(),
            next_color: 0,
            layout,
        }
    }

    /// Adds the next commit. `parents` in git order (first parent first); parents that will
    /// never be pushed (outside the walk, shallow boundary) should be omitted, otherwise
    /// their lines run off the bottom. `color_hint` pins the color of a branch tip (e.g. a
    /// hash of the branch name) so a branch keeps its color across refreshes.
    ///
    /// A hinted commit reached by a line from a newer commit takes its hint's color (and
    /// the lane continues below it in that color) only if that line's color did not come
    /// from a branch itself; otherwise it keeps the line's color, so a branch pointing at
    /// an older commit of another branch doesn't repaint that branch's lane. See
    /// [`Self::push_claiming`] for commits whose color wins regardless.
    pub fn push(&mut self, id: K, parents: &[K], color_hint: Option<ColorId>) -> Node {
        self.push_with(id, parents, color_hint, false)
    }

    /// Like [`Self::push`] with a hint, but `color` always wins: the node and the lane below
    /// it take it even when a line from another branch arrives (the line keeps its own
    /// color down to the node). For the checked-out branch and mainlines, so their color
    /// doesn't depend on which branches sit on top of them.
    pub fn push_claiming(&mut self, id: K, parents: &[K], color: ColorId) -> Node {
        self.push_with(id, parents, Some(color), true)
    }

    fn push_with(
        &mut self,
        id: K,
        parents: &[K],
        color_hint: Option<ColorId>,
        claims: bool,
    ) -> Node {
        let row = self.layout.nodes.len() as u32;
        let (top, center, bottom) = (2 * row, 2 * row + 1, 2 * row + 2);
        let hint = color_hint.map(|c| c % PALETTE_SIZE);

        // 1. Lines arriving at this commit; pick the lane the node sits in.
        let arriving = self.waiting.remove(&id).unwrap_or_default();
        let preferred = arriving
            .iter()
            .copied()
            .filter(|&lane| self.slot(lane).first_parent)
            .min()
            .or_else(|| arriving.iter().copied().min());
        // Whether the node's color came from a branch (passed on to its first-parent line).
        let (node, hinted) = match preferred {
            Some(lane) => {
                let slot = self.slot(lane);
                match hint {
                    Some(color) if claims || !slot.hinted => (Node { lane, color }, true),
                    _ => (
                        Node {
                            lane,
                            color: slot.color,
                        },
                        slot.hinted,
                    ),
                }
            }
            None => {
                let lane = self.take_leftmost_free();
                match hint {
                    Some(color) => (Node { lane, color }, true),
                    None => (
                        Node {
                            lane,
                            color: self.fresh_color(lane),
                        },
                        false,
                    ),
                }
            }
        };

        for lane in arriving {
            let slot = self.slots[lane as usize]
                .take()
                .expect("waiting lane has a slot");
            if lane == node.lane {
                self.add_span(lane, slot.color, slot.from, center);
            } else {
                self.add_span(lane, slot.color, slot.from, top);
                self.layout.connectors.push(Connector {
                    kind: ConnectorKind::MergeIn,
                    lane,
                    color: slot.color,
                });
                self.free.insert(lane);
            }
        }

        // 2. Lines leaving towards the parents.
        match parents.first() {
            Some(&first) => self.occupy(node.lane, first, node.color, center, true, hinted),
            None => {
                self.free.insert(node.lane);
            }
        }
        for (index, &parent) in parents.iter().enumerate().skip(1) {
            if parents[..index].contains(&parent) {
                continue; // duplicate parent
            }
            let existing = self.waiting.get(&parent).and_then(|lanes| {
                lanes
                    .iter()
                    .copied()
                    .filter(|&lane| lane != node.lane)
                    .min_by_key(|&lane| (i32::from(lane) - i32::from(node.lane)).abs())
            });
            let (lane, color) = match existing {
                Some(lane) => (lane, self.slot(lane).color),
                None => {
                    let lane = self.take_free_right_of(node.lane);
                    let color = self.fresh_color(lane);
                    self.occupy(lane, parent, color, bottom, false, false);
                    (lane, color)
                }
            };
            self.layout.connectors.push(Connector {
                kind: ConnectorKind::BranchOut,
                lane,
                color,
            });
        }

        self.layout.nodes.push(node);
        self.layout
            .conn_offsets
            .push(self.layout.connectors.len() as u32);
        node
    }

    pub fn finish(mut self) -> Layout {
        // Lines to commits that never came run off the bottom edge.
        let end = 2 * self.layout.nodes.len() as u32;
        for lane in 0..self.slots.len() {
            if let Some(slot) = self.slots[lane].take() {
                self.add_span(lane as LaneId, slot.color, slot.from, end);
            }
        }
        self.layout.lane_count = self.slots.len() as LaneId;
        self.layout.spans.sort_by_key(|s| (s.from, s.lane));
        // Grown by doubling while laid out: up to twice what they hold, for as long as the
        // graph is shown (#64).
        self.layout.spans.shrink_to_fit();
        self.layout.connectors.shrink_to_fit();
        self.layout.conn_offsets.shrink_to_fit();
        index_spans(&mut self.layout);
        self.layout
    }

    fn slot(&self, lane: LaneId) -> &Slot {
        self.slots[lane as usize]
            .as_ref()
            .expect("lane has an active slot")
    }

    fn occupy(
        &mut self,
        lane: LaneId,
        expect: K,
        color: ColorId,
        from: u32,
        first_parent: bool,
        hinted: bool,
    ) {
        self.slots[lane as usize] = Some(Slot {
            color,
            from,
            first_parent,
            hinted,
        });
        self.waiting.entry(expect).or_default().push(lane);
    }

    fn add_span(&mut self, lane: LaneId, color: ColorId, from: u32, to: u32) {
        if from < to {
            self.layout.spans.push(Span {
                lane,
                color,
                from,
                to,
            });
        }
    }

    fn take_leftmost_free(&mut self) -> LaneId {
        match self.free.pop_first() {
            Some(lane) => lane,
            None => self.new_lane(),
        }
    }

    fn take_free_right_of(&mut self, lane: LaneId) -> LaneId {
        match self.free.range(lane + 1..).next().copied() {
            Some(free) => {
                self.free.remove(&free);
                free
            }
            None => self.new_lane(),
        }
    }

    /// A new lane on the right. Past [`LaneId::MAX`] lanes at once (theoretical: that
    /// many branches side by side), the last one is shared rather than panicking (#25):
    /// the drawing is off there, the history isn't.
    fn new_lane(&mut self) -> LaneId {
        match LaneId::try_from(self.slots.len()) {
            Ok(lane) => {
                self.slots.push(None);
                lane
            }
            Err(_) => LaneId::MAX,
        }
    }

    /// Next palette color, avoiding the colors of the neighbouring lanes.
    fn fresh_color(&mut self, lane: LaneId) -> ColorId {
        let neighbour = |offset: isize| -> Option<ColorId> {
            let index = lane as isize + offset;
            (index >= 0)
                .then(|| self.slots.get(index as usize)?.as_ref().map(|s| s.color))
                .flatten()
        };
        let (left, right) = (neighbour(-1), neighbour(1));
        let mut color = self.next_color;
        for _ in 0..PALETTE_SIZE {
            if Some(color) != left && Some(color) != right {
                break;
            }
            color = (color + 1) % PALETTE_SIZE;
        }
        self.next_color = (color + 1) % PALETTE_SIZE;
        color
    }
}

/// Buckets spans by the row chunks they touch, for fast per-row queries.
fn index_spans(layout: &mut Layout) {
    let rows = layout.nodes.len() as u32;
    let chunks = rows.div_ceil(CHUNK_ROWS) as usize;
    let chunk_range = |s: &Span| {
        let first_row = s.from / 2;
        let last_row = (s.to - 1) / 2;
        (first_row / CHUNK_ROWS) as usize..=((last_row / CHUNK_ROWS) as usize).min(chunks - 1)
    };

    let mut counts = vec![0u32; chunks + 1];
    for span in &layout.spans {
        for chunk in chunk_range(span) {
            counts[chunk + 1] += 1;
        }
    }
    for i in 1..counts.len() {
        counts[i] += counts[i - 1];
    }
    let mut fill = counts.clone();
    let mut chunk_spans = vec![0u32; counts[chunks] as usize];
    for (id, span) in layout.spans.iter().enumerate() {
        for chunk in chunk_range(span) {
            chunk_spans[fill[chunk] as usize] = id as u32;
            fill[chunk] += 1;
        }
    }
    layout.chunk_offsets = counts;
    layout.chunk_spans = chunk_spans;
}

#[cfg(test)]
mod tests {
    use super::{LaneId, LayoutBuilder};

    #[test]
    fn past_the_last_lane_it_is_shared_rather_than_panicking() {
        let mut builder: LayoutBuilder<u32> = LayoutBuilder::new();
        builder.slots = (0..=LaneId::MAX).map(|_| None).collect();
        assert_eq!(builder.new_lane(), LaneId::MAX);
        assert_eq!(builder.new_lane(), LaneId::MAX);
        assert_eq!(builder.slots.len(), usize::from(LaneId::MAX) + 1);
    }
}
