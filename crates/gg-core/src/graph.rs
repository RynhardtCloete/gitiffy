//! Renderer-independent commit-graph layout primitives.
//!
//! `gg-graph` produces a [`GraphRow`] per commit; a UI backend walks the row's
//! [`Segment`]s and the node position and emits draw calls. No pixel geometry
//! lives here, only lane indices and connection topology, so the same layout
//! drives GPUI, egui, or an ASCII renderer.
//!
//! ## Coordinate model
//!
//! Each row occupies one cell, one row tall. Lanes are columns indexed from 0.
//! A cell has three vertical anchor points per lane: the top boundary, the
//! vertical center (where this row's commit dot sits, in [`GraphRow::node_lane`]),
//! and the bottom boundary. Segments connect these anchors:
//!
//! * [`SegmentKind::Passthrough`] — a lane that is unrelated to this commit and
//!   simply continues straight down: `(lane, top) -> (lane, bottom)`.
//! * [`SegmentKind::MergeIn`] — an incoming edge from a child lane into this
//!   commit's node: `(from_lane, top) -> (node_lane, center)`.
//! * [`SegmentKind::BranchOut`] — an outgoing edge from this commit's node to a
//!   parent lane: `(node_lane, center) -> (to_lane, bottom)`.

use crate::oid::Oid;

/// The kind of connection a [`Segment`] represents within a row cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegmentKind {
    /// A lane passing straight through this row, unrelated to its commit.
    Passthrough,
    /// An edge from a (child) lane at the top boundary into this row's node.
    MergeIn,
    /// An edge from this row's node out to a parent lane at the bottom boundary.
    BranchOut,
}

/// One drawable connection within a row cell. Interpretation of `from_lane` /
/// `to_lane` depends on [`Segment::kind`] (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    /// What this segment connects.
    pub kind: SegmentKind,
    /// Source lane (top anchor for `Passthrough`/`MergeIn`; the node lane for `BranchOut`).
    pub from_lane: usize,
    /// Destination lane (the node lane for `MergeIn`; bottom anchor otherwise).
    pub to_lane: usize,
    /// Stable lane color index, cycled through the renderer's palette.
    pub color: u32,
}

/// The laid-out graph for a single commit / history row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphRow {
    /// Zero-based row index (matches the history list row).
    pub row: usize,
    /// The commit drawn on this row.
    pub commit: Oid,
    /// The lane (column) the commit's node dot occupies.
    pub node_lane: usize,
    /// Palette color index for the node, matching its continuing lane.
    pub node_color: u32,
    /// All connections to draw in this row's cell.
    pub segments: Vec<Segment>,
    /// Number of lanes occupied around this row (drives required canvas width).
    pub lanes: usize,
}

impl GraphRow {
    /// Convenience: iterate only the segments of a given kind.
    pub fn segments_of(&self, kind: SegmentKind) -> impl Iterator<Item = &Segment> {
        self.segments.iter().filter(move |s| s.kind == kind)
    }
}
