//! `gg-ui-traits` — the renderer abstraction that lets the commit-graph layout
//! engine drive any toolkit. `gg-graph`/`gg-app` never name a GPUI or egui
//! type; a backend implements [`GraphCanvas`] (a primitive-drawing surface) and
//! reuses the shared [`draw_row`] routine, so the only per-backend code is the
//! handful of lines that translate primitives into real draw calls.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use gg_core::graph::{GraphRow, SegmentKind};

/// A 2D point in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    /// Horizontal coordinate.
    pub x: f32,
    /// Vertical coordinate.
    pub y: f32,
}

impl Point {
    /// Construct a point.
    pub fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// An RGBA color, 8 bits per channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    /// Red.
    pub r: u8,
    /// Green.
    pub g: u8,
    /// Blue.
    pub b: u8,
    /// Alpha.
    pub a: u8,
}

impl Color {
    /// Opaque color from RGB.
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }
}

/// The default 8-color lane palette (matches [`gg_graph::LANE_COLORS`]).
pub const LANE_PALETTE: [Color; 8] = [
    Color::rgb(0x42, 0x85, 0xf4), // blue
    Color::rgb(0xea, 0x43, 0x35), // red
    Color::rgb(0x34, 0xa8, 0x53), // green
    Color::rgb(0xfb, 0xbc, 0x05), // amber
    Color::rgb(0xa1, 0x42, 0xf4), // purple
    Color::rgb(0x00, 0xac, 0xc1), // cyan
    Color::rgb(0xff, 0x70, 0x43), // orange
    Color::rgb(0xec, 0x40, 0x7a), // pink
];

/// Map a lane color index to a palette color.
pub fn lane_color(index: u32) -> Color {
    LANE_PALETTE[(index as usize) % LANE_PALETTE.len()]
}

/// Text styling for canvas text draws.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextStyle {
    /// Font size in logical pixels.
    pub size: f32,
    /// Text color.
    pub color: Color,
    /// Bold weight.
    pub bold: bool,
}

/// The visible window the renderer should draw, expressed in rows. This is what
/// drives virtualization: only `first_row..first_row + visible_rows` is ever
/// laid out and drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Viewport {
    /// Index of the first visible row.
    pub first_row: usize,
    /// Number of rows visible (plus any overscan the caller adds).
    pub visible_rows: usize,
}

/// Pixel metrics for translating lane/row indices into coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GraphMetrics {
    /// Height of one commit row.
    pub row_height: f32,
    /// Horizontal spacing between lanes.
    pub lane_width: f32,
    /// Radius of a commit node dot.
    pub node_radius: f32,
    /// Stroke width for edges.
    pub edge_width: f32,
    /// Left padding before lane 0.
    pub x_offset: f32,
    /// Top padding before the first visible row.
    pub y_offset: f32,
}

impl Default for GraphMetrics {
    fn default() -> Self {
        Self {
            row_height: 22.0,
            lane_width: 16.0,
            node_radius: 4.0,
            edge_width: 1.5,
            x_offset: 12.0,
            y_offset: 0.0,
        }
    }
}

impl GraphMetrics {
    /// X coordinate of a lane's center.
    pub fn lane_x(&self, lane: usize) -> f32 {
        self.x_offset + lane as f32 * self.lane_width
    }

    /// Y coordinate of a row's vertical center, relative to the first visible row.
    pub fn row_center_y(&self, row: usize, first_visible: usize) -> f32 {
        let rel = row as isize - first_visible as isize;
        self.y_offset + rel as f32 * self.row_height + self.row_height / 2.0
    }
}

/// A primitive-drawing surface a UI backend implements. The graph engine emits
/// only these calls, so a backend needs nothing toolkit-specific beyond mapping
/// them to its painter.
pub trait GraphCanvas {
    /// Draw a straight line between two points.
    fn line(&mut self, from: Point, to: Point, color: Color, width: f32);
    /// Draw a filled circle.
    fn circle(&mut self, center: Point, radius: f32, fill: Color);
    /// Draw text anchored at its left-baseline-ish top-left.
    fn text(&mut self, at: Point, s: &str, style: TextStyle);
}

/// Draw a single laid-out graph row onto a canvas. This is the shared,
/// backend-independent rendering core: lane indices and edge segments in,
/// primitive draw calls out.
pub fn draw_row<C: GraphCanvas>(
    canvas: &mut C,
    row: &GraphRow,
    viewport: Viewport,
    metrics: &GraphMetrics,
) {
    let first = viewport.first_row;
    let center_y = metrics.row_center_y(row.row, first);
    let top_y = center_y - metrics.row_height / 2.0;
    let bottom_y = center_y + metrics.row_height / 2.0;
    let w = metrics.edge_width;

    for seg in &row.segments {
        let color = lane_color(seg.color);
        match seg.kind {
            SegmentKind::Passthrough => {
                let x = metrics.lane_x(seg.from_lane);
                canvas.line(Point::new(x, top_y), Point::new(x, bottom_y), color, w);
            }
            SegmentKind::MergeIn => {
                let from = Point::new(metrics.lane_x(seg.from_lane), top_y);
                let to = Point::new(metrics.lane_x(seg.to_lane), center_y);
                canvas.line(from, to, color, w);
            }
            SegmentKind::BranchOut => {
                let from = Point::new(metrics.lane_x(seg.from_lane), center_y);
                let to = Point::new(metrics.lane_x(seg.to_lane), bottom_y);
                canvas.line(from, to, color, w);
            }
        }
    }

    // The commit node sits on top of the edges.
    let node = Point::new(metrics.lane_x(row.node_lane), center_y);
    canvas.circle(node, metrics.node_radius, lane_color(row.node_color));
}

#[cfg(test)]
mod tests {
    use super::*;
    use gg_core::Oid;
    use gg_graph::{CommitInput, GraphLayout};

    /// A canvas that records primitives, for asserting on draw output.
    #[derive(Default)]
    struct Recorder {
        lines: usize,
        circles: usize,
        last_circle: Option<Point>,
    }
    impl GraphCanvas for Recorder {
        fn line(&mut self, _: Point, _: Point, _: Color, _: f32) {
            self.lines += 1;
        }
        fn circle(&mut self, center: Point, _: f32, _: Color) {
            self.circles += 1;
            self.last_circle = Some(center);
        }
        fn text(&mut self, _: Point, _: &str, _: TextStyle) {}
    }

    fn oid(n: u32) -> Oid {
        let mut b = [0u8; 20];
        b[0..4].copy_from_slice(&n.to_be_bytes());
        Oid::from_bytes(&b).unwrap()
    }

    #[test]
    fn draws_a_node_per_row_and_some_edges() {
        let commits = [
            CommitInput::new(oid(1), vec![oid(2), oid(3)]),
            CommitInput::new(oid(2), vec![oid(4)]),
            CommitInput::new(oid(3), vec![oid(4)]),
            CommitInput::new(oid(4), vec![]),
        ];
        let layout = GraphLayout::from_commits(&commits);
        let metrics = GraphMetrics::default();
        let viewport = Viewport {
            first_row: 0,
            visible_rows: layout.len(),
        };

        let mut rec = Recorder::default();
        for row in layout.rows() {
            draw_row(&mut rec, row, viewport, &metrics);
        }
        // One node circle per commit.
        assert_eq!(rec.circles, layout.len());
        // The merge row alone produces at least two branch-out edges.
        assert!(rec.lines >= 2);
    }

    #[test]
    fn node_x_follows_lane() {
        let m = GraphMetrics::default();
        assert_eq!(m.lane_x(0), m.x_offset);
        assert_eq!(m.lane_x(2), m.x_offset + 2.0 * m.lane_width);
    }
}
