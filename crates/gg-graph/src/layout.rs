//! The incremental, virtualization-friendly lane-assignment engine.
//!
//! The algorithm is a single forward scan over commits in display order (each
//! commit appears before all of its parents — exactly what a topological
//! revwalk yields). It maintains a vector of *active lanes*, where each slot
//! holds the oid of the commit we next expect to place in that lane. This is
//! the classic active-lanes approach (pvigier / git-graph / Sapling), with two
//! properties the spec calls out:
//!
//! * **Slot reuse** — freed lanes are refilled before new ones are pushed, so
//!   many short-lived branches do not make the graph grow without bound.
//! * **Convergence reuse** — when a parent is already awaited by an existing
//!   lane, the commit's edge routes into that lane instead of spawning a
//!   duplicate, keeping shared-ancestor fan-in compact.
//!
//! Octopus merges (>2 parents) are handled directly: every extra parent simply
//! claims another lane.
//!
//! Layout is incremental: [`GraphLayout::extend`] continues the scan and caches
//! each produced [`GraphRow`], so history can be paged in and only the rows up
//! to the furthest-scrolled point are ever computed.

use gg_core::graph::{GraphRow, Segment, SegmentKind};
use gg_core::Oid;

/// The minimal per-commit input the layout engine consumes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitInput {
    /// The commit's id.
    pub oid: Oid,
    /// Parent ids in order (first parent first).
    pub parents: Vec<Oid>,
}

impl CommitInput {
    /// Build a commit input from an oid and its parents.
    pub fn new(oid: Oid, parents: Vec<Oid>) -> Self {
        Self { oid, parents }
    }
}

/// Number of distinct lane colors the engine cycles through. Renderers map this
/// onto their palette.
pub const LANE_COLORS: u32 = 8;

/// Incremental commit-graph layout state plus the rows produced so far.
#[derive(Default, Debug)]
pub struct GraphLayout {
    rows: Vec<GraphRow>,
    /// Per-lane: the oid this lane is next waiting to place (`None` == free).
    active: Vec<Option<Oid>>,
    /// Per-lane stable color index (kept across frees, reset on reuse).
    lane_color: Vec<u32>,
    /// Rolling color counter.
    next_color: u32,
}

impl GraphLayout {
    /// A fresh, empty layout.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a complete layout from commits already in display order.
    pub fn from_commits(commits: &[CommitInput]) -> Self {
        let mut layout = Self::new();
        layout.extend(commits);
        layout
    }

    /// The rows laid out so far.
    pub fn rows(&self) -> &[GraphRow] {
        &self.rows
    }

    /// Number of rows laid out so far.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// True when no rows have been laid out.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The current maximum lane width across all laid-out rows (canvas sizing).
    pub fn max_width(&self) -> usize {
        self.rows.iter().map(|r| r.lanes).max().unwrap_or(0)
    }

    /// Continue the layout scan with the next batch of commits (in display
    /// order, continuing from the last row). Newly produced rows are appended.
    pub fn extend(&mut self, commits: &[CommitInput]) {
        self.rows.reserve(commits.len());
        for c in commits {
            self.push_commit(c);
        }
    }

    fn take_color(&mut self) -> u32 {
        let c = self.next_color % LANE_COLORS;
        self.next_color = self.next_color.wrapping_add(1);
        c
    }

    /// Reserve a lane, reusing the leftmost free slot before growing.
    fn alloc_lane(&mut self) -> usize {
        let color = self.take_color();
        if let Some(i) = self.active.iter().position(Option::is_none) {
            self.lane_color[i] = color;
            i
        } else {
            self.active.push(None);
            self.lane_color.push(color);
            self.active.len() - 1
        }
    }

    fn push_commit(&mut self, c: &CommitInput) {
        let row = self.rows.len();
        let incoming = self.active.clone();

        // Dedupe parents while preserving order (defensive against malformed
        // input that lists a parent twice).
        let mut parents: Vec<Oid> = Vec::with_capacity(c.parents.len());
        for p in &c.parents {
            if !parents.contains(p) {
                parents.push(*p);
            }
        }

        // Lanes that were waiting for this exact commit.
        let expecting: Vec<usize> = incoming
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| (*slot == Some(c.oid)).then_some(i))
            .collect();

        // The node sits in the leftmost expecting lane, or a fresh lane if this
        // commit is a tip nothing was waiting on.
        let node_lane = match expecting.first() {
            Some(&first) => first,
            None => self.alloc_lane(),
        };

        // Release every expecting lane; continuations are decided below.
        for &l in &expecting {
            self.active[l] = None;
        }

        let mut segments: Vec<Segment> = Vec::new();

        // Lanes unrelated to this commit continue straight down.
        for (l, slot) in incoming.iter().enumerate() {
            if slot.is_some() && !expecting.contains(&l) {
                segments.push(Segment {
                    kind: SegmentKind::Passthrough,
                    from_lane: l,
                    to_lane: l,
                    color: self.lane_color[l],
                });
            }
        }

        // Child lanes feeding into this commit's node.
        for &l in &expecting {
            segments.push(Segment {
                kind: SegmentKind::MergeIn,
                from_lane: l,
                to_lane: node_lane,
                color: self.lane_color[l],
            });
        }

        // Route an outgoing edge to each parent, reusing an existing lane that
        // already awaits the parent, then the node lane for the first parent,
        // then a freshly allocated lane.
        for (pi, p) in parents.iter().enumerate() {
            let parent_lane = if let Some(l) = self.active.iter().position(|s| *s == Some(*p)) {
                l
            } else if pi == 0 {
                self.active[node_lane] = Some(*p);
                node_lane
            } else {
                let l = self.alloc_lane();
                self.active[l] = Some(*p);
                l
            };
            segments.push(Segment {
                kind: SegmentKind::BranchOut,
                from_lane: node_lane,
                to_lane: parent_lane,
                color: self.lane_color[parent_lane],
            });
        }

        // Required width: the highest lane index touched by this cell, +1.
        let mut max_lane = node_lane;
        for (i, slot) in incoming.iter().enumerate() {
            if slot.is_some() {
                max_lane = max_lane.max(i);
            }
        }
        for (i, slot) in self.active.iter().enumerate() {
            if slot.is_some() {
                max_lane = max_lane.max(i);
            }
        }
        for s in &segments {
            max_lane = max_lane.max(s.from_lane).max(s.to_lane);
        }

        let node_color = self.lane_color[node_lane];
        self.rows.push(GraphRow {
            row,
            commit: c.oid,
            node_lane,
            node_color,
            segments,
            lanes: max_lane + 1,
        });
    }
}
