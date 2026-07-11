//! Native macOS menu bar (app / File / Edit / View / Go / Window / Help),
//! built with `muda` and attached to NSApp once at startup.
//!
//! Menu picks arrive on muda's event handler (an arbitrary main-thread
//! callback), so they are forwarded into a channel and the egui context is
//! woken; the app drains them as [`MenuAction`]s each frame. System-standard
//! items (Edit's clipboard verbs, minimize/zoom, and View's Enter Full Screen
//! with its native Ctrl+Cmd+F key equivalent) are `PredefinedMenuItem`s, so
//! AppKit handles them without any app plumbing.

use eframe::egui;
use muda::accelerator::{Accelerator, Code, Modifiers};
use muda::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};

/// An app-level command chosen from the native menu bar.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    AddRepository,
    CloneRepository,
    NewRepository,
    CloseRepository,
    EditUndo,
    EditRedo,
    EditCut,
    EditCopy,
    EditPaste,
    EditSelectAll,
    Refresh,
    ToggleSidebar,
    ShowLocalChanges,
    ShowAllCommits,
    PreviousTab,
    NextTab,
    OpenInTerminal,
    OpenInFileManager,
    OpenInEditor,
    Help,
}

pub struct MenuBar {
    /// Keeps the NSApp menu alive; dropping it would tear the menu down.
    _menu: Menu,
    rx: std::sync::mpsc::Receiver<MenuEvent>,
    actions: Vec<(MenuId, MenuAction)>,
}

impl MenuBar {
    /// Build and attach the menu bar. Returns `None` (leaving the app fully
    /// usable through the in-window UI) if menu construction fails.
    pub fn install(ctx: egui::Context) -> Option<Self> {
        let (tx, rx) = std::sync::mpsc::channel();
        // Forward events into our channel and wake egui, so a menu pick is
        // handled on the very next frame instead of the next natural repaint.
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let _ = tx.send(event);
            ctx.request_repaint();
        }));

        let mut actions = Vec::new();
        let mut item = |text: &str, accel: Option<Accelerator>, action: MenuAction| {
            let it = MenuItem::new(text, true, accel);
            actions.push((it.id().clone(), action));
            it
        };
        let cmd = Modifiers::META;
        let cmd_shift = Modifiers::META | Modifiers::SHIFT;

        let app_menu = Submenu::with_items(
            "gittify",
            true,
            &[
                &PredefinedMenuItem::about(None, None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::hide(None),
                &PredefinedMenuItem::hide_others(None),
                &PredefinedMenuItem::show_all(None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::quit(None),
            ],
        )
        .ok()?;

        let file = Submenu::with_items(
            "File",
            true,
            &[
                &item(
                    "Add Existing Repository…",
                    Some(Accelerator::new(Some(cmd), Code::KeyO)),
                    MenuAction::AddRepository,
                ),
                &item(
                    "Clone Repository…",
                    Some(Accelerator::new(Some(cmd_shift), Code::KeyO)),
                    MenuAction::CloneRepository,
                ),
                &item(
                    "New Repository…",
                    Some(Accelerator::new(Some(cmd_shift), Code::KeyN)),
                    MenuAction::NewRepository,
                ),
                &PredefinedMenuItem::separator(),
                &item(
                    "Close Repository",
                    Some(Accelerator::new(Some(cmd), Code::KeyW)),
                    MenuAction::CloseRepository,
                ),
            ],
        )
        .ok()?;

        // Deliberately NOT PredefinedMenuItems: those call the standard AppKit
        // selectors (copy:, selectAll:, …), which winit's view does not
        // implement, so they would swallow the key equivalents (⌘A, ⌘C, …)
        // while doing nothing. Custom items round-trip through the app, which
        // re-injects the matching egui event, so both the menu items and the
        // shortcuts work inside egui text fields.
        let edit = Submenu::with_items(
            "Edit",
            true,
            &[
                &item(
                    "Undo",
                    Some(Accelerator::new(Some(cmd), Code::KeyZ)),
                    MenuAction::EditUndo,
                ),
                &item(
                    "Redo",
                    Some(Accelerator::new(Some(cmd_shift), Code::KeyZ)),
                    MenuAction::EditRedo,
                ),
                &PredefinedMenuItem::separator(),
                &item(
                    "Cut",
                    Some(Accelerator::new(Some(cmd), Code::KeyX)),
                    MenuAction::EditCut,
                ),
                &item(
                    "Copy",
                    Some(Accelerator::new(Some(cmd), Code::KeyC)),
                    MenuAction::EditCopy,
                ),
                &item(
                    "Paste",
                    Some(Accelerator::new(Some(cmd), Code::KeyV)),
                    MenuAction::EditPaste,
                ),
                &item(
                    "Select All",
                    Some(Accelerator::new(Some(cmd), Code::KeyA)),
                    MenuAction::EditSelectAll,
                ),
            ],
        )
        .ok()?;

        let view = Submenu::with_items(
            "View",
            true,
            &[
                &item(
                    "Local Changes",
                    Some(Accelerator::new(Some(cmd), Code::Digit1)),
                    MenuAction::ShowLocalChanges,
                ),
                &item(
                    "All Commits",
                    Some(Accelerator::new(Some(cmd), Code::Digit2)),
                    MenuAction::ShowAllCommits,
                ),
                &PredefinedMenuItem::separator(),
                &item(
                    "Toggle Sidebar",
                    Some(Accelerator::new(Some(cmd), Code::KeyB)),
                    MenuAction::ToggleSidebar,
                ),
                &item(
                    "Refresh",
                    Some(Accelerator::new(Some(cmd), Code::KeyR)),
                    MenuAction::Refresh,
                ),
                &PredefinedMenuItem::separator(),
                // Standard AppKit fullscreen item: gives the app the system
                // Ctrl+Cmd+F toggle.
                &PredefinedMenuItem::fullscreen(None),
            ],
        )
        .ok()?;

        let go = Submenu::with_items(
            "Go",
            true,
            &[
                &item(
                    "Previous Repository",
                    Some(Accelerator::new(Some(cmd_shift), Code::BracketLeft)),
                    MenuAction::PreviousTab,
                ),
                &item(
                    "Next Repository",
                    Some(Accelerator::new(Some(cmd_shift), Code::BracketRight)),
                    MenuAction::NextTab,
                ),
                &PredefinedMenuItem::separator(),
                &item("Open in Terminal", None, MenuAction::OpenInTerminal),
                &item("Open in Finder", None, MenuAction::OpenInFileManager),
                &item("Open in Editor", None, MenuAction::OpenInEditor),
            ],
        )
        .ok()?;

        let window = Submenu::with_items(
            "Window",
            true,
            &[
                &PredefinedMenuItem::minimize(None),
                &PredefinedMenuItem::maximize(None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::bring_all_to_front(None),
            ],
        )
        .ok()?;

        let help = Submenu::with_items(
            "Help",
            true,
            &[&item("gittify on GitHub", None, MenuAction::Help)],
        )
        .ok()?;

        let menu = Menu::with_items(&[&app_menu, &file, &edit, &view, &go, &window, &help]).ok()?;
        menu.init_for_nsapp();
        // Let AppKit manage the window list / help search field.
        window.set_as_windows_menu_for_nsapp();
        help.set_as_help_menu_for_nsapp();

        Some(Self {
            _menu: menu,
            rx,
            actions,
        })
    }

    /// Next pending menu action, if any (drain by calling until `None`).
    pub fn poll(&self) -> Option<MenuAction> {
        while let Ok(event) = self.rx.try_recv() {
            if let Some((_, action)) = self.actions.iter().find(|(id, _)| id == event.id()) {
                return Some(*action);
            }
        }
        None
    }
}
