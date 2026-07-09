//! egui implementation of the gittify rendering abstraction — the validated
//! fallback backend.
//!
//! The only backend-specific code is the [`GraphCanvas`] bridge below: it maps
//! the engine's primitive draw calls onto an `egui::Painter`. Everything else
//! (lane assignment, edge routing, virtualization, row geometry) is shared via
//! `gg-graph` and `gg_ui_traits::draw_row`. The history list itself uses egui's
//! `ScrollArea::show_rows`, which virtualizes to the visible window exactly like
//! the graph engine does.

#![forbid(unsafe_code)]

use gg_ui_traits::{Color, GraphCanvas, Point, TextStyle};

/// Wraps an `egui::Painter` so the shared `draw_row` routine can render onto it.
pub struct EguiCanvas<'a> {
    painter: &'a egui::Painter,
}

impl<'a> EguiCanvas<'a> {
    /// Build a canvas around a painter (already offset to the graph gutter).
    pub fn new(painter: &'a egui::Painter) -> Self {
        Self { painter }
    }
}

fn to_pos(p: Point) -> egui::Pos2 {
    egui::pos2(p.x, p.y)
}

fn to_color(c: Color) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(c.r, c.g, c.b, c.a)
}

impl GraphCanvas for EguiCanvas<'_> {
    fn line(&mut self, from: Point, to: Point, color: Color, width: f32) {
        self.painter.line_segment(
            [to_pos(from), to_pos(to)],
            egui::Stroke::new(width, to_color(color)),
        );
    }

    fn circle(&mut self, center: Point, radius: f32, fill: Color) {
        self.painter
            .circle_filled(to_pos(center), radius, to_color(fill));
    }

    fn text(&mut self, at: Point, s: &str, style: TextStyle) {
        self.painter.text(
            to_pos(at),
            egui::Align2::LEFT_TOP,
            s,
            egui::FontId::proportional(style.size),
            to_color(style.color),
        );
    }
}

// A full eframe `App` (history list + graph gutter + diff pane) wiring
// `gg_app::AppHandle` to egui widgets is the next increment for this backend;
// the canvas bridge above is the piece the layout engine actually depends on.
