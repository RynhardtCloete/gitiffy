//! The egui `GraphCanvas` bridge: the only backend-specific drawing code. The
//! shared `gg_ui_traits::draw_row` emits primitives; this maps them onto an
//! `egui::Painter`.

use eframe::egui;
use gg_ui_traits::{Color, GraphCanvas, Point, TextStyle};

/// Wraps an `egui::Painter` so `draw_row` can render the commit graph onto it.
pub struct EguiCanvas<'a> {
    painter: &'a egui::Painter,
}

impl<'a> EguiCanvas<'a> {
    pub fn new(painter: &'a egui::Painter) -> Self {
        Self { painter }
    }
}

fn pos(p: Point) -> egui::Pos2 {
    egui::pos2(p.x, p.y)
}

fn col(c: Color) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(c.r, c.g, c.b, c.a)
}

impl GraphCanvas for EguiCanvas<'_> {
    fn line(&mut self, from: Point, to: Point, color: Color, width: f32) {
        self.painter
            .line_segment([pos(from), pos(to)], egui::Stroke::new(width, col(color)));
    }

    fn circle(&mut self, center: Point, radius: f32, fill: Color) {
        self.painter.circle_filled(pos(center), radius, col(fill));
    }

    fn text(&mut self, at: Point, s: &str, style: TextStyle) {
        self.painter.text(
            pos(at),
            egui::Align2::LEFT_TOP,
            s,
            egui::FontId::proportional(style.size),
            col(style.color),
        );
    }
}
