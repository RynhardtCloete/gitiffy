//! The Home / landing page: the active workspace's repository library.
//!
//! Left: a searchable list of the library's repos with an add menu. Right: a
//! rendered-markdown README preview for the selected repo, or (with nothing
//! selected) an introduction with quick actions and the recently-opened list.
//! Single-click previews; double-click (or the Open button) opens the repo as
//! a tab. All mutations are returned as [`HomeAction`]s for the app to apply.

use std::path::{Path, PathBuf};

use eframe::egui::{self, Color32};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};

use crate::workspace::WsNode;

/// Actions the landing page asks the app to perform.
pub enum HomeAction {
    /// Open this repo as a tab in the active workspace.
    Open(PathBuf),
    /// Remove this repo from the active workspace's library.
    Remove(PathBuf),
    /// Launch the add-existing folder picker.
    AddExisting,
    /// Open the clone dialog.
    Clone,
    /// Open the new-repository dialog.
    Init,
    /// Launch the scan-folder picker.
    Scan,
}

/// Session-scoped UI state of the landing page.
#[derive(Default)]
pub struct HomeState {
    search: String,
    /// The library entry currently previewed.
    pub selected: Option<PathBuf>,
    /// README cache for the selected repo (`None` payload = no README found).
    readme: Option<(PathBuf, Option<String>)>,
    md_cache: CommonMarkCache,
}

/// Row height of one library list entry (two lines: name + parent path).
const LIB_ROW_H: f32 = 44.0;

impl HomeState {
    /// Render the landing page for the active workspace.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        ws: Option<&WsNode>,
        recent: &[PathBuf],
        actions: &mut Vec<HomeAction>,
    ) {
        let library: Vec<PathBuf> = ws.map(|w| w.library.clone()).unwrap_or_default();
        // Drop a stale selection (e.g. after Remove).
        if let Some(sel) = &self.selected {
            if !library.iter().any(|p| p == sel) {
                self.selected = None;
            }
        }

        egui::SidePanel::left("home-library")
            .resizable(true)
            .default_width(290.0)
            .width_range(220.0..=460.0)
            .show_inside(ui, |ui| {
                self.library_panel(ui, ws, &library, actions);
            });

        match self.selected.clone() {
            Some(repo) => self.readme_panel(ui, &repo, actions),
            None => self.intro_panel(ui, recent, actions),
        }
    }

    /// Left panel: search, add menu, and the library list.
    fn library_panel(
        &mut self,
        ui: &mut egui::Ui,
        ws: Option<&WsNode>,
        library: &[PathBuf],
        actions: &mut Vec<HomeAction>,
    ) {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.add_space(2.0);
            let hint = ws
                .map(|w| format!("Search {}…", w.name))
                .unwrap_or_else(|| "Search…".to_string());
            ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text(hint)
                    .desired_width(ui.available_width() - 34.0),
            );
            ui.menu_button(icon_add(), |ui| {
                ui.set_min_width(230.0);
                add_menu_items(ui, actions);
            })
            .response
            .on_hover_text("Add repositories to this workspace");
        });
        ui.add_space(6.0);
        ui.separator();

        let needle = self.search.to_lowercase();
        let shown: Vec<&PathBuf> = library
            .iter()
            .filter(|p| {
                needle.is_empty() || p.display().to_string().to_lowercase().contains(&needle)
            })
            .collect();

        if library.is_empty() {
            ui.add_space(12.0);
            ui.vertical_centered(|ui| {
                ui.weak("No repositories yet.");
                ui.weak("Add or scan a folder to get started.");
            });
            return;
        }
        if shown.is_empty() {
            ui.add_space(12.0);
            ui.vertical_centered(|ui| ui.weak("No matches."));
            return;
        }

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                for path in shown {
                    self.library_row(ui, path, actions);
                }
                ui.add_space(6.0);
            });
    }

    /// One two-line library row: repo name over its dimmed parent path.
    fn library_row(&mut self, ui: &mut egui::Ui, path: &PathBuf, actions: &mut Vec<HomeAction>) {
        let (rect, resp) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), LIB_ROW_H),
            egui::Sense::click(),
        );
        let selected = self.selected.as_ref() == Some(path);
        let missing = !path.exists();
        let bg = if selected {
            ui.visuals().selection.bg_fill
        } else if resp.hovered() {
            Color32::from_white_alpha(12)
        } else {
            Color32::from_white_alpha(4)
        };
        ui.painter().rect_filled(
            rect.shrink2(egui::vec2(0.0, 1.0)),
            egui::CornerRadius::same(4),
            bg,
        );

        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let parent = path
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let text_color = if missing {
            Color32::from_gray(120)
        } else {
            ui.visuals().text_color()
        };
        let painter = ui.painter_at(rect);
        painter.text(
            egui::pos2(rect.left() + 10.0, rect.top() + 13.0),
            egui::Align2::LEFT_CENTER,
            if missing {
                format!("{name}  (missing)")
            } else {
                name
            },
            egui::FontId::proportional(13.0),
            text_color,
        );
        painter.text(
            egui::pos2(rect.left() + 10.0, rect.bottom() - 12.0),
            egui::Align2::LEFT_CENTER,
            parent,
            egui::FontId::proportional(11.0),
            Color32::from_gray(130),
        );

        if resp.double_clicked() && !missing {
            actions.push(HomeAction::Open(path.clone()));
        } else if resp.clicked() {
            self.selected = Some(path.clone());
        }
        resp.context_menu(|ui| {
            if !missing && ui.button("Open").clicked() {
                actions.push(HomeAction::Open(path.clone()));
                ui.close();
            }
            if ui.button("Remove from this workspace").clicked() {
                actions.push(HomeAction::Remove(path.clone()));
                ui.close();
            }
            ui.separator();
            if ui.button("Copy path").clicked() {
                ui.ctx().copy_text(path.display().to_string());
                ui.close();
            }
        });
    }

    /// Right panel with a repo selected: header + rendered README.
    fn readme_panel(&mut self, ui: &mut egui::Ui, repo: &Path, actions: &mut Vec<HomeAction>) {
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.add_space(12.0);
            let name = repo
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| repo.display().to_string());
            ui.label(egui::RichText::new(name).size(18.0).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(12.0);
                if ui.button("Open repository").clicked() {
                    actions.push(HomeAction::Open(repo.to_path_buf()));
                }
            });
        });
        ui.horizontal(|ui| {
            ui.add_space(12.0);
            ui.label(
                egui::RichText::new(repo.display().to_string())
                    .size(11.0)
                    .color(Color32::from_gray(140)),
            );
        });
        ui.add_space(4.0);
        ui.separator();

        if self.readme.as_ref().map(|(p, _)| p.as_path()) != Some(repo) {
            self.readme = Some((repo.to_path_buf(), load_readme(repo)));
        }
        let text = self.readme.as_ref().and_then(|(_, t)| t.clone());
        match text {
            Some(text) => {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.add_space(12.0);
                            ui.vertical(|ui| {
                                CommonMarkViewer::new().show(ui, &mut self.md_cache, &text);
                                ui.add_space(12.0);
                            });
                        });
                    });
            }
            None => {
                ui.add_space(16.0);
                ui.vertical_centered(|ui| ui.weak("No README found in this repository."));
            }
        }
    }

    /// Right panel with nothing selected: intro, quick actions, and recents.
    fn intro_panel(
        &mut self,
        ui: &mut egui::Ui,
        recent: &[PathBuf],
        actions: &mut Vec<HomeAction>,
    ) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(36.0);
                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new("gittify").size(26.0).strong());
                    ui.label(
                        egui::RichText::new("Select a repository on the left to preview it,")
                            .color(Color32::from_gray(150)),
                    );
                    ui.label(
                        egui::RichText::new("double-click to open it, or start with:")
                            .color(Color32::from_gray(150)),
                    );
                    ui.add_space(16.0);
                    let quick = |ui: &mut egui::Ui, label: String| -> bool {
                        ui.add(egui::Button::new(label).min_size(egui::vec2(280.0, 30.0)))
                            .clicked()
                    };
                    if quick(ui, format!("{}  Add existing repository…", icon_add())) {
                        actions.push(HomeAction::AddExisting);
                    }
                    ui.add_space(4.0);
                    if quick(ui, "⬇  Clone repository…".to_string()) {
                        actions.push(HomeAction::Clone);
                    }
                    ui.add_space(4.0);
                    if quick(ui, "📁  New repository…".to_string()) {
                        actions.push(HomeAction::Init);
                    }
                    ui.add_space(4.0);
                    if quick(ui, "⟳  Scan a folder for repositories…".to_string()) {
                        actions.push(HomeAction::Scan);
                    }
                });

                let recent: Vec<&PathBuf> = recent.iter().filter(|p| p.exists()).collect();
                if !recent.is_empty() {
                    ui.add_space(28.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            egui::RichText::new("Recently opened")
                                .size(12.0)
                                .strong()
                                .color(Color32::from_gray(170)),
                        );
                        ui.add_space(4.0);
                        for path in recent {
                            let name = path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| path.display().to_string());
                            if ui
                                .link(egui::RichText::new(name).size(13.0))
                                .on_hover_text(path.display().to_string())
                                .clicked()
                            {
                                actions.push(HomeAction::Open(path.clone()));
                            }
                        }
                    });
                }
                ui.add_space(24.0);
            });
    }
}

/// The add menu shared by the panel's ➕ button (labels mirror the ribbon's
/// Repository menu).
fn add_menu_items(ui: &mut egui::Ui, actions: &mut Vec<HomeAction>) {
    if ui
        .button(format!("{}  Add existing repository…", icon_add()))
        .clicked()
    {
        actions.push(HomeAction::AddExisting);
        ui.close();
    }
    if ui.button("⬇  Clone repository…").clicked() {
        actions.push(HomeAction::Clone);
        ui.close();
    }
    if ui.button("📁  New repository…").clicked() {
        actions.push(HomeAction::Init);
        ui.close();
    }
    ui.separator();
    if ui.button("⟳  Scan a folder for repositories…").clicked() {
        actions.push(HomeAction::Scan);
        ui.close();
    }
}

fn icon_add() -> &'static str {
    "➕"
}

/// Read the repo's README (first common filename that exists), truncated so a
/// pathological file can't stall the UI.
fn load_readme(repo: &Path) -> Option<String> {
    const CANDIDATES: [&str; 7] = [
        "README.md",
        "Readme.md",
        "readme.md",
        "README.MD",
        "README",
        "README.txt",
        "readme.txt",
    ];
    const MAX_LEN: usize = 256 * 1024;
    for name in CANDIDATES {
        if let Ok(mut text) = std::fs::read_to_string(repo.join(name)) {
            if text.len() > MAX_LEN {
                let mut cut = MAX_LEN;
                while !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                text.truncate(cut);
                text.push_str("\n\n*…truncated*");
            }
            return Some(text);
        }
    }
    None
}
