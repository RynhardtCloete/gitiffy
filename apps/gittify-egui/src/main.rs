//! gittify — egui desktop front-end.
//!
//! A Fork-style repository workspace: organize repository folders into nestable
//! workspaces shown as tabs (persisted across launches) and browse each one's
//! commit graph, rendered through the same renderer-independent layout engine
//! the rest of gittify uses.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod canvas;
mod config;
mod workspace;

use app::GittifyApp;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([720.0, 420.0])
            .with_title("gittify"),
        ..Default::default()
    };

    eframe::run_native(
        "gittify",
        options,
        Box::new(|_cc| Ok(Box::new(GittifyApp::new()))),
    )
}
