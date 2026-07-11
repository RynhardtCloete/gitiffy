//! The eframe application: a Fork-style repository workspace.
//!
//! Fixed-size repository tabs run along the very top (persisted per
//! workspace) with the workspace selector pinned top-right, a captioned
//! ribbon of tool groups (Repository / Branch / Sync / Stash, plus an
//! "Open in" menu) beneath them, and a collapsible sidebar on the left
//! holding the view switch (Local Changes / All Commits) and the filterable
//! branches / remotes / tags / stashes tree. The two views:
//!
//! * **History** — the commit graph + log as aligned columns (graph, subject
//!   with ref pills, author, date, short SHA), rendered through the shared
//!   layout engine and `draw_row`. Selecting a commit minifies the table into
//!   a left-hand strip and gives the rest of the window to the commit's
//!   files (a tab strip) over a full-size diff; clicking the commit again
//!   restores the full table.
//! * **Changes** — the working tree: staged / unstaged / untracked files with
//!   stage, unstage, and discard actions; a live diff preview; and a commit box
//!   (title + description) with Commit and Commit & Push.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{self, Color32};
use gg_app::{AppHandle, AppState, Command, DiffView, Event, EventWaker};
use gg_core::{
    ChangeKind, CommitMeta, FileChange, FileDiff, LineKind, Oid, RefKind, RefRecord, StatusEntry,
    Time,
};
use gg_git::{CommitOpts, Credentials, ResetMode, WalkOpts};
use gg_ui_traits::{draw_row, GraphMetrics, Viewport};

use crate::canvas::EguiCanvas;
use crate::config;
use crate::workspace::{WorkspaceStore, WsNode};

const HISTORY_LIMIT: usize = 2000;
/// How many more commits each history page pulls in when the user scrolls to
/// the bottom of a truncated graph.
const HISTORY_PAGE: usize = 2000;
const ROW_HEIGHT: f32 = 28.0;
/// Fixed height of one virtualized file-list row (changes / commit files).
const FILE_ROW_HEIGHT: f32 = 24.0;
/// Fixed height of one virtualized diff row (hunk header or code line).
const DIFF_ROW_HEIGHT: f32 = 18.0;
/// Fixed size of one repository tab in the top tab strip. A constant footprint
/// keeps neighbors from shifting when tabs open/close or names differ.
const TAB_WIDTH: f32 = 168.0;
const TAB_HEIGHT: f32 = 28.0;
const LANE_WIDTH: f32 = 16.0;
const MAX_GUTTER: f32 = 320.0;

const COL_AUTHOR_W: f32 = 180.0;
const COL_DATE_W: f32 = 150.0;
const COL_SHA_W: f32 = 76.0;
const COL_GAP: f32 = 16.0;
const COL_PAD: f32 = 12.0;

/// UI glyphs, restricted to codepoints egui's bundled fonts (NotoEmoji /
/// emoji-icon-font / Ubuntu) actually render. Obscure symbol codepoints (e.g.
/// `⎇`, `●`, `✓`, fullwidth `＋`) show as tofu boxes, so the whole app draws its
/// icons from this verified set.
mod icon {
    pub const ADD: &str = "➕";
    pub const REFRESH: &str = "⟳";
    pub const COMMIT: &str = "✔";
    pub const PUSH: &str = "⤴";
    pub const PULL: &str = "⬇";
    pub const RENAME: &str = "✏";
    pub const DELETE: &str = "🗑";
    pub const REMOTE: &str = "⬇";
    pub const REMOVE: &str = "🗙";
    pub const CARET_DOWN: &str = "⏷";
    pub const CARET_LEFT: &str = "⏴";
    pub const CARET_RIGHT: &str = "⏵";
    pub const DOT: &str = "•";
    pub const ARROW: &str = "»";
    pub const TAG: &str = "🏷";
    pub const FOLDER: &str = "📁";
    pub const SETTINGS: &str = "⚙";
}

/// Which pane of a repository is showing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    History,
    Changes,
}

/// Identifies a selected changed file (path + which side).
#[derive(Clone, PartialEq, Eq)]
struct ChangeSel {
    path: PathBuf,
    staged: bool,
}

/// A branch operation chosen from the toolbar menu, applied after the panel
/// closure ends (so it doesn't fight the borrow of `self`).
enum BranchCmd {
    /// Check out an existing local branch.
    Checkout(String),
    /// Create a local tracking branch from a remote branch and check it out.
    CheckoutTracking {
        /// Local branch name to create.
        local: String,
        /// Remote start point (e.g. `origin/feature`).
        start: String,
    },
    /// Delete a tag.
    DeleteTag(String),
    /// Open the remotes-management dialog.
    ManageRemotes,
}

/// A network operation chosen from the ribbon.
#[derive(Clone)]
enum NetCmd {
    /// Fetch from the derived (upstream/origin/first) remote.
    Fetch,
    /// Fetch from a specific remote (picked from the fetch dropdown).
    FetchRemote(String),
    Pull,
    Push,
    /// Push the current branch with `--force-with-lease`.
    PushForce,
}

/// Actions collected while rendering the repository sidebar, applied by
/// `update` once the panel closure returns.
#[derive(Default)]
struct SidebarOut {
    branch_cmd: Option<(usize, BranchCmd)>,
    open_dialog: Option<BranchDialog>,
    stash_cmd: Option<StashCmd>,
    /// Open the create-tag dialog: `Some(target)` where the inner `None`
    /// targets HEAD.
    tag_dialog_at: Option<Option<String>>,
    /// Collapse the sidebar to its thin rail (the embedded ⏴ button).
    collapse: bool,
    /// Switch the repo view (the Local Changes / All Commits section).
    set_view: Option<View>,
}

/// A stash operation chosen from the toolbar stash menu.
enum StashCmd {
    /// Create a stash (optionally including untracked files).
    Push(bool),
    /// Apply a stash without dropping it.
    Apply(usize),
    /// Apply a stash and drop it.
    Pop(usize),
    /// Drop a stash.
    Drop(usize),
}

/// What a pending folder-picker result is for.
#[derive(Clone, Copy)]
enum PickFor {
    /// Add an existing repository as a tab.
    AddExisting,
    /// Destination parent folder for a clone.
    CloneDest,
    /// Parent folder for a new (`git init`) repository.
    InitParent,
    /// Root folder to scan for repositories (bulk add).
    ScanRoot,
}

/// Modal for cloning a remote repository: URL + destination folder. The
/// clone itself runs on a background thread.
struct CloneDialog {
    url: String,
    dest: Option<PathBuf>,
    /// In-flight clone; the cloned path (or git's stderr) arrives here.
    rx: Option<std::sync::mpsc::Receiver<Result<PathBuf, String>>>,
    error: Option<String>,
    /// A previous attempt failed for lack of credentials: show the
    /// username/secret fields and retry with them.
    need_auth: bool,
    username: String,
    password: String,
}

/// Credential prompt shown when a repository's network operation fails
/// because git found no credentials (no helper configured, nothing cached).
/// Submitting routes the credentials through the `gg-askpass` helper and
/// retries the failed operation.
struct AuthDialog {
    /// Pool index of the repo whose operation failed.
    repo: usize,
    /// The failed operation, retried on submit.
    retry: NetCmd,
    /// Operation label ("Pull", "Push", …) for the dialog text.
    label: String,
    username: String,
    password: String,
}

/// Modal for initializing a brand-new repository: parent folder + name.
struct InitDialog {
    parent: Option<PathBuf>,
    name: String,
    error: Option<String>,
}

/// One repository found by a folder scan, with its bulk-add checkbox state.
struct ScanEntry {
    path: PathBuf,
    checked: bool,
    /// Already in the active workspace's library (shown, but not re-added).
    already: bool,
}

/// Modal for bulk-adding repositories discovered under a scanned folder. The
/// filesystem walk runs on a background thread.
struct ScanDialog {
    root: PathBuf,
    /// In-flight scan; the discovered repo paths arrive here.
    rx: Option<std::sync::mpsc::Receiver<Vec<PathBuf>>>,
    found: Vec<ScanEntry>,
}

/// A modal for creating a tag at a commit (or HEAD).
struct TagDialog {
    repo: usize,
    /// Target commit hex, or `None` for HEAD.
    target: Option<String>,
    name: String,
    message: String,
}

/// A modal for managing remotes. Holds the add-form fields; the existing
/// remotes come from [`AppState::remotes`].
struct RemotesDialog {
    repo: usize,
    name: String,
    url: String,
}

/// A modal branch dialog awaiting text input / confirmation.
enum BranchDialog {
    /// Create a new branch (from a start commit/ref, or HEAD), optionally
    /// checking it out.
    New {
        repo: usize,
        name: String,
        /// Start point (a commit hex when launched from a commit's menu), or
        /// `None` for HEAD.
        start: Option<String>,
        checkout: bool,
    },
    /// Rename an existing branch.
    Rename {
        repo: usize,
        old: String,
        name: String,
    },
    /// Confirm deleting a branch.
    Delete {
        repo: usize,
        name: String,
        force: bool,
    },
}

/// An action chosen from a history commit's right-click context menu, applied
/// after the panel's borrow of `self` ends.
enum CommitMenuAction {
    /// Check out the commit (detached HEAD).
    Checkout(String),
    /// Open the new-branch dialog with this commit as the start point.
    NewBranchHere(String),
    /// Cherry-pick the commit onto the current branch.
    CherryPick(Oid),
    /// Revert the commit.
    Revert(Oid),
    /// Reset the current branch to the commit (soft/mixed).
    Reset(String, ResetMode),
    /// Reset `--hard` to the commit, pending confirmation.
    ResetHardConfirm(String),
    /// Open the create-tag dialog targeting this commit.
    CreateTagHere(String),
    /// Merge this commit into the current branch.
    Merge(String),
    /// Rebase the current branch onto this commit.
    Rebase(String),
}

/// One open repository: its worker, state, view, and per-repo UI buffers.
struct RepoTab {
    path: PathBuf,
    name: String,
    handle: AppHandle,
    state: AppState,
    loading: bool,
    view: View,
    commit_title: String,
    commit_body: String,
    /// Replace the previous commit (`--amend`) instead of adding a new one.
    amend: bool,
    /// Sign the commit (`-S`).
    sign: bool,
    selected_change: Option<ChangeSel>,
    /// Paths checked for multi-file actions (stage/unstage/discard in bulk).
    multi: HashSet<PathBuf>,
    /// Label of a long-running op in flight (pull, merge, …), shown as a loading
    /// indicator until its Completed/Failed event arrives. `None` when idle.
    busy: Option<String>,
    /// Raw git output lines for the current op, shown in the details window.
    op_log: Vec<String>,
    /// Whether the commit-detail panel is expanded to show the full message.
    detail_expanded: bool,
    /// Selected commit (History view), remembered per tab.
    selected_commit: Option<usize>,
    /// Selected file within the selected commit's changed-files list.
    selected_commit_file: Option<usize>,
    /// Ref pills keyed by target commit, rebuilt when the refs refresh (instead
    /// of every frame).
    labels: HashMap<Oid, Vec<RefChip>>,
    /// Bumped on every `Event::Diff`, keying [`RepoTab::diff_doc`].
    diff_gen: u64,
    /// Bumped on every `Event::CommitDiff`, keying [`RepoTab::commit_doc`].
    commit_diff_gen: u64,
    /// Cached flat row model of the changes-view diff, keyed by `diff_gen`.
    diff_doc: Option<(u64, DiffDoc)>,
    /// Cached flat row model of the selected commit-file diff, keyed by
    /// `(commit_diff_gen, file index)`.
    commit_doc: Option<((u64, usize), DiffDoc)>,
    /// The history limit most recently requested from the worker.
    requested_limit: usize,
    /// The last loaded history reached the root (nothing more to page in).
    history_complete: bool,
    /// A grow-the-history request is in flight.
    loading_more: bool,
    /// Sidebar filter text (matches branches, tags, remotes, stashes).
    sidebar_filter: String,
    /// The most recent network op, kept for a credential-prompt retry until
    /// it completes.
    pending_net: Option<NetCmd>,
}

impl RepoTab {
    fn open(path: PathBuf, waker: Option<EventWaker>) -> Result<Self, String> {
        let handle = match waker {
            Some(w) => AppHandle::open_with_waker(path.clone(), w),
            None => AppHandle::open(path.clone()),
        }
        .map_err(|e| e.to_string())?;
        handle.send(Command::LoadRefs);
        handle.send(Command::LoadStatus);
        handle.send(Command::LoadStashes);
        handle.send(Command::LoadRemotes);
        handle.send(Command::LoadHistory(WalkOpts {
            tips: Vec::new(),
            limit: Some(HISTORY_LIMIT),
            first_parent: false,
        }));
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        Ok(Self {
            path,
            name,
            handle,
            state: AppState::default(),
            loading: true,
            view: View::History,
            commit_title: String::new(),
            commit_body: String::new(),
            amend: false,
            sign: false,
            selected_change: None,
            multi: HashSet::new(),
            busy: None,
            op_log: Vec::new(),
            detail_expanded: false,
            selected_commit: None,
            selected_commit_file: None,
            labels: HashMap::new(),
            diff_gen: 0,
            commit_diff_gen: 0,
            diff_doc: None,
            commit_doc: None,
            requested_limit: HISTORY_LIMIT,
            history_complete: false,
            loading_more: false,
            sidebar_filter: String::new(),
            pending_net: None,
        })
    }

    fn reload(&mut self) {
        self.handle.send(Command::LoadRefs);
        self.handle.send(Command::LoadStatus);
        self.handle.send(Command::LoadStashes);
        self.handle.send(Command::LoadRemotes);
        self.requested_limit = HISTORY_LIMIT;
        self.history_complete = false;
        self.loading_more = false;
        self.handle.send(Command::LoadHistory(WalkOpts {
            tips: Vec::new(),
            limit: Some(HISTORY_LIMIT),
            first_parent: false,
        }));
        self.loading = true;
    }

    /// Mark a long-running op as started: show its label as busy and reset the
    /// per-op progress log (so the loading indicator and details window track
    /// the new operation).
    fn start_op(&mut self, label: &str) {
        self.busy = Some(label.to_string());
        self.op_log.clear();
    }
}

/// The whole application.
pub struct GittifyApp {
    /// The persisted workspace tree + active-node pointer.
    workspaces: WorkspaceStore,
    /// Warm pool of open repository workers, one per unique path.
    repos: Vec<RepoTab>,
    add_error: Option<String>,
    /// Pending discard awaiting confirmation: (repo, tracked paths to restore,
    /// untracked paths to delete).
    confirm_discard: Option<(usize, Vec<PathBuf>, Vec<PathBuf>)>,
    /// Pending `reset --hard` awaiting confirmation: (repo, target commit hex).
    confirm_reset: Option<(usize, String)>,
    /// Open branch dialog (new / rename / delete), if any.
    branch_dialog: Option<BranchDialog>,
    /// Open create-tag dialog, if any.
    tag_dialog: Option<TagDialog>,
    /// Open remotes-management dialog, if any.
    remotes_dialog: Option<RemotesDialog>,
    /// Open clone-repository dialog, if any.
    clone_dialog: Option<CloneDialog>,
    /// Open new-repository (git init) dialog, if any.
    init_dialog: Option<InitDialog>,
    /// Open credential prompt for a failed network op, if any.
    auth_dialog: Option<AuthDialog>,
    /// Open scan-folder (bulk add) dialog, if any.
    scan_dialog: Option<ScanDialog>,
    /// Landing page state (library list, README preview).
    home: crate::home::HomeState,
    /// The Home tab is selected (also forced when no repo tabs are open).
    home_selected: bool,
    /// Whether the operation-details (progress log) window is open.
    show_op_details: bool,
    /// Whether the manage-workspaces settings modal is open.
    settings_open: bool,
    /// In-progress workspace rename: (node id, edit buffer).
    ws_rename: Option<(u64, String)>,
    styled: bool,
    /// Whether the left repository sidebar is shown.
    show_sidebar: bool,
    /// Pending async folder-picker result and what it is for (the dialog runs
    /// on its own thread so it never blocks the UI).
    picker_rx: Option<(PickFor, std::sync::mpsc::Receiver<Option<PathBuf>>)>,
    /// The egui context, captured on the first frame; used to build worker
    /// wakers and to repaint when the folder picker resolves.
    ui_ctx: Option<egui::Context>,
    /// The native macOS menu bar; `None` if it failed to build.
    #[cfg(target_os = "macos")]
    menubar: Option<crate::menubar::MenuBar>,
}

impl GittifyApp {
    /// Build the app, loading the persisted workspace tree.
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        #[cfg(not(target_os = "macos"))]
        let _ = cc;
        Self {
            workspaces: config::load_workspaces(),
            repos: Vec::new(),
            add_error: None,
            confirm_discard: None,
            confirm_reset: None,
            branch_dialog: None,
            tag_dialog: None,
            remotes_dialog: None,
            clone_dialog: None,
            init_dialog: None,
            auth_dialog: None,
            scan_dialog: None,
            home: crate::home::HomeState::default(),
            home_selected: false,
            show_op_details: false,
            settings_open: false,
            ws_rename: None,
            styled: false,
            show_sidebar: true,
            picker_rx: None,
            ui_ctx: None,
            #[cfg(target_os = "macos")]
            menubar: crate::menubar::MenuBar::install(cc.egui_ctx.clone()),
        }
    }

    /// Apply one command chosen from the native macOS menu bar.
    #[cfg(target_os = "macos")]
    fn apply_menu_action(&mut self, ctx: &egui::Context, action: crate::menubar::MenuAction) {
        use crate::menubar::MenuAction;
        match action {
            // Edit verbs translate back into the egui events their shortcuts
            // would have produced (the menu consumed the keystroke).
            MenuAction::EditUndo => inject_key(ctx, egui::Key::Z, egui::Modifiers::COMMAND),
            MenuAction::EditRedo => inject_key(
                ctx,
                egui::Key::Z,
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
            ),
            MenuAction::EditCut => ctx.input_mut(|i| i.events.push(egui::Event::Cut)),
            MenuAction::EditCopy => ctx.input_mut(|i| i.events.push(egui::Event::Copy)),
            MenuAction::EditPaste => {
                if let Ok(text) = arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
                    ctx.input_mut(|i| i.events.push(egui::Event::Paste(text)));
                }
            }
            MenuAction::EditSelectAll => inject_key(ctx, egui::Key::A, egui::Modifiers::COMMAND),
            MenuAction::AddRepository => self.pick_and_add(),
            MenuAction::CloneRepository => self.open_clone_dialog(),
            MenuAction::NewRepository => self.open_init_dialog(),
            MenuAction::CloseRepository => {
                if let Some(i) = self.workspaces.active_node().map(|w| w.active_tab) {
                    self.close_tab(i);
                }
            }
            MenuAction::Refresh => {
                if let Some(sel) = self.active_index() {
                    self.repos[sel].reload();
                }
            }
            MenuAction::ToggleSidebar => self.show_sidebar = !self.show_sidebar,
            MenuAction::ShowLocalChanges | MenuAction::ShowAllCommits => {
                if let Some(sel) = self.active_index() {
                    let tab = &mut self.repos[sel];
                    if matches!(action, MenuAction::ShowLocalChanges) {
                        tab.view = View::Changes;
                        tab.handle.send(Command::LoadStatus);
                    } else {
                        tab.view = View::History;
                    }
                }
            }
            MenuAction::PreviousTab | MenuAction::NextTab => {
                if let Some(ws) = self.workspaces.active_node_mut() {
                    let n = ws.repos.len();
                    if n > 0 {
                        let step = if matches!(action, MenuAction::NextTab) {
                            1
                        } else {
                            n - 1
                        };
                        ws.active_tab = (ws.active_tab + step) % n;
                    }
                }
                self.persist();
                self.sync_open_tabs();
            }
            MenuAction::OpenInTerminal => {
                if let Some(tab) = self.active_index().and_then(|s| self.repos.get(s)) {
                    open_in_terminal(&tab.path);
                }
            }
            MenuAction::OpenInFileManager => {
                if let Some(tab) = self.active_index().and_then(|s| self.repos.get(s)) {
                    open_in_file_manager(&tab.path);
                }
            }
            MenuAction::OpenInEditor => {
                if let Some(tab) = self.active_index().and_then(|s| self.repos.get(s)) {
                    open_in_editor(&tab.path);
                }
            }
            MenuAction::Help => {
                let _ = std::process::Command::new("open")
                    .arg("https://github.com/RynhardtCloete/gitiffy")
                    .spawn();
            }
        }
    }

    /// A waker that repaints the UI when the worker queues an event, so results
    /// appear immediately instead of on the next poll tick.
    fn waker(&self) -> Option<EventWaker> {
        self.ui_ctx.as_ref().map(|ctx| {
            let ctx = ctx.clone();
            Arc::new(move || ctx.request_repaint()) as EventWaker
        })
    }

    fn persist(&self) {
        config::save_workspaces(&self.workspaces);
    }

    /// Pool index of the active workspace's active tab, if open.
    fn active_index(&self) -> Option<usize> {
        let ws = self.workspaces.active_node()?;
        let path = ws.repos.get(ws.active_tab)?;
        self.repos.iter().position(|t| &t.path == path)
    }

    /// Ensure a warm `RepoTab` exists in the pool for `path` (spawning its
    /// worker lazily). Errors are surfaced via `add_error`.
    fn ensure_open(&mut self, path: &Path) {
        if self.repos.iter().any(|t| t.path == path) {
            return;
        }
        match RepoTab::open(path.to_path_buf(), self.waker()) {
            Ok(tab) => self.repos.push(tab),
            Err(e) => self.add_error = Some(format!("{}: {e}", path.display())),
        }
    }

    /// Open (warm) every tab of the active workspace and clamp its active index.
    fn sync_open_tabs(&mut self) {
        let paths = self
            .workspaces
            .active_node()
            .map(|w| w.repos.clone())
            .unwrap_or_default();
        for p in &paths {
            self.ensure_open(p);
        }
        if let Some(ws) = self.workspaces.active_node_mut() {
            if !ws.repos.is_empty() && ws.active_tab >= ws.repos.len() {
                ws.active_tab = ws.repos.len() - 1;
            }
        }
        // Drop pooled workers no workspace references anymore.
        let store = &self.workspaces;
        self.repos.retain(|t| store.references(&t.path));
    }

    /// Open the native folder picker on a background thread; the chosen folder
    /// is routed by `purpose` when the result arrives (polled in `update`).
    fn pick_folder(&mut self, purpose: PickFor) {
        if self.picker_rx.is_some() {
            return; // A picker is already open.
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = self.ui_ctx.clone();
        std::thread::spawn(move || {
            // The async dialog is safe off the main thread (on macOS it
            // dispatches to the main queue, which eframe's event loop pumps).
            let dir = pollster::block_on(rfd::AsyncFileDialog::new().pick_folder())
                .map(|h| h.path().to_path_buf());
            let _ = tx.send(dir);
            if let Some(ctx) = ctx {
                ctx.request_repaint();
            }
        });
        self.picker_rx = Some((purpose, rx));
    }

    /// Open the folder picker to add an existing repository.
    fn pick_and_add(&mut self) {
        self.pick_folder(PickFor::AddExisting);
    }

    /// Open the clone-repository dialog (no-op while a clone dialog exists,
    /// so an in-flight clone is never clobbered).
    fn open_clone_dialog(&mut self) {
        if self.clone_dialog.is_none() {
            self.clone_dialog = Some(CloneDialog {
                url: String::new(),
                dest: None,
                rx: None,
                error: None,
                need_auth: false,
                username: String::new(),
                password: String::new(),
            });
        }
    }

    /// Open the new-repository (`git init`) dialog.
    fn open_init_dialog(&mut self) {
        if self.init_dialog.is_none() {
            self.init_dialog = Some(InitDialog {
                parent: None,
                name: String::new(),
                error: None,
            });
        }
    }

    /// Poll the async folder picker, routing the chosen folder to whichever
    /// flow requested it.
    fn poll_picker(&mut self) {
        let Some((purpose, rx)) = &self.picker_rx else {
            return;
        };
        let purpose = *purpose;
        match rx.try_recv() {
            Ok(dir) => {
                self.picker_rx = None;
                if let Some(dir) = dir {
                    match purpose {
                        PickFor::AddExisting => self.add_repo(dir),
                        PickFor::CloneDest => {
                            if let Some(d) = &mut self.clone_dialog {
                                d.dest = Some(dir);
                            }
                        }
                        PickFor::InitParent => {
                            if let Some(d) = &mut self.init_dialog {
                                d.parent = Some(dir);
                            }
                        }
                        PickFor::ScanRoot => self.start_scan(dir),
                    }
                }
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => self.picker_rx = None,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }

    /// Kick off a background scan of `root` for repositories to bulk-add.
    fn start_scan(&mut self, root: PathBuf) {
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = self.ui_ctx.clone();
        let walk_root = root.clone();
        std::thread::spawn(move || {
            let mut found = Vec::new();
            scan_for_repos(&walk_root, SCAN_DEPTH, &mut found);
            let _ = tx.send(found);
            if let Some(ctx) = ctx {
                ctx.request_repaint();
            }
        });
        self.scan_dialog = Some(ScanDialog {
            root,
            rx: Some(rx),
            found: Vec::new(),
        });
    }

    /// Apply one action requested by the landing page.
    fn apply_home_action(&mut self, action: crate::home::HomeAction) {
        use crate::home::HomeAction;
        match action {
            HomeAction::Open(path) => self.add_repo(path),
            HomeAction::Remove(path) => {
                if let Some(ws) = self.workspaces.active_node_mut() {
                    ws.library.retain(|p| p != &path);
                }
                if self.home.selected.as_ref() == Some(&path) {
                    self.home.selected = None;
                }
                self.persist();
            }
            HomeAction::AddExisting => self.pick_and_add(),
            HomeAction::Clone => self.open_clone_dialog(),
            HomeAction::Init => self.open_init_dialog(),
            HomeAction::Scan => self.pick_folder(PickFor::ScanRoot),
        }
    }

    /// Add a repo as a tab in the active workspace (and make it active).
    fn add_repo(&mut self, path: PathBuf) {
        self.add_error = None;
        if let Some(ws) = self.workspaces.active_node_mut() {
            ws.add_to_library(&path);
            if let Some(i) = ws.repos.iter().position(|p| p == &path) {
                ws.active_tab = i;
            } else {
                ws.repos.push(path.clone());
                ws.active_tab = ws.repos.len() - 1;
            }
        }
        self.workspaces.touch_recent(&path);
        self.ensure_open(&path);
        self.persist();
        // Opening a repo always brings its tab to the front, over Home.
        self.home_selected = false;
    }

    /// Close the tab at `tab_index` in the active workspace.
    fn close_tab(&mut self, tab_index: usize) {
        if let Some(ws) = self.workspaces.active_node_mut() {
            if tab_index < ws.repos.len() {
                ws.repos.remove(tab_index);
                if ws.active_tab >= ws.repos.len() {
                    ws.active_tab = ws.repos.len().saturating_sub(1);
                }
            }
        }
        self.persist();
    }

    /// Start a network operation on the given repo, remembering it so a
    /// credential prompt can retry it.
    fn dispatch_net(&mut self, sel: usize, nc: NetCmd) {
        let tab = &mut self.repos[sel];
        tab.pending_net = Some(nc.clone());
        let remote = derive_remote(&tab.state).unwrap_or_else(|| "origin".to_string());
        let (cmd, label) = match nc {
            NetCmd::Fetch => (Command::Fetch(remote), "Fetch"),
            NetCmd::FetchRemote(r) => (Command::Fetch(r), "Fetch"),
            NetCmd::Pull => (Command::Pull(remote), "Pull"),
            NetCmd::Push => (Command::PushCurrent { force: false }, "Push"),
            NetCmd::PushForce => (Command::PushCurrent { force: true }, "Push (force)"),
        };
        tab.start_op(label);
        tab.handle.send(cmd);
    }

    /// Dispatch a branch/tag/remotes action collected from the toolbar menu or
    /// the sidebar.
    fn apply_branch_cmd(&mut self, cmd: Option<(usize, BranchCmd)>) {
        let Some((sel, cmd)) = cmd else {
            return;
        };
        match cmd {
            BranchCmd::ManageRemotes => {
                self.remotes_dialog = Some(RemotesDialog {
                    repo: sel,
                    name: String::new(),
                    url: String::new(),
                });
            }
            other => {
                if let Some(tab) = self.repos.get(sel) {
                    match other {
                        BranchCmd::Checkout(name) => tab.handle.send(Command::Checkout(name)),
                        BranchCmd::CheckoutTracking { local, start } => {
                            tab.handle.send(Command::CreateBranch {
                                name: local,
                                start: Some(start),
                                checkout: true,
                            });
                        }
                        BranchCmd::DeleteTag(name) => tab.handle.send(Command::DeleteTag(name)),
                        BranchCmd::ManageRemotes => {}
                    }
                }
            }
        }
    }

    /// Dispatch a stash action collected from the toolbar menu or the sidebar.
    fn apply_stash_cmd(&mut self, sc: Option<StashCmd>) {
        let Some(sc) = sc else {
            return;
        };
        if let Some(sel) = self.active_index() {
            let tab = &mut self.repos[sel];
            let cmd = match sc {
                StashCmd::Push(u) => Command::Stash {
                    include_untracked: u,
                },
                StashCmd::Apply(i) => Command::StashApply(i),
                StashCmd::Pop(i) => Command::StashPop(i),
                StashCmd::Drop(i) => Command::StashDrop(i),
            };
            tab.start_op("Stash");
            tab.handle.send(cmd);
        }
    }

    fn drain_events(&mut self) {
        // A credential prompt to open once the borrow of `repos` ends.
        let mut auth_req: Option<AuthDialog> = None;
        for (repo_idx, tab) in self.repos.iter_mut().enumerate() {
            let events = tab.handle.poll_events();
            for ev in events {
                match &ev {
                    Event::History(view) => {
                        tab.loading = false;
                        tab.loading_more = false;
                        // A short read means the walk hit the root: no more pages.
                        tab.history_complete = view.commits.len() < tab.requested_limit;
                        // The selection is a row index; a refresh can shift
                        // rows under it (a new commit inserts at the top).
                        // Re-anchor it to the same commit id, or clear it if
                        // that commit left the view (reset, amend).
                        if let Some(i) = tab.selected_commit {
                            let old_oid = tab
                                .state
                                .history
                                .as_ref()
                                .and_then(|v| v.commits.get(i))
                                .map(|c| c.oid);
                            tab.selected_commit = old_oid
                                .and_then(|oid| view.commits.iter().position(|c| c.oid == oid));
                            if tab.selected_commit.is_none() {
                                tab.selected_commit_file = None;
                            }
                        }
                    }
                    Event::Refs(refs) => tab.labels = build_label_map(refs),
                    Event::Status(status) => {
                        // The refreshed tree may no longer contain the
                        // previewed file on the previewed side (committed,
                        // discarded, fully staged): drop the stale selection
                        // and its diff so the pane doesn't show a ghost.
                        let stale = tab.selected_change.as_ref().is_some_and(|sel| {
                            !status.entries.iter().any(|e| {
                                e.path == sel.path
                                    && if sel.staged {
                                        e.is_staged()
                                    } else {
                                        e.has_unstaged()
                                    }
                            })
                        });
                        if stale {
                            tab.selected_change = None;
                            tab.state.diff = None;
                            tab.diff_doc = None;
                        }
                        // Bulk-selection checkboxes for vanished paths too.
                        tab.multi
                            .retain(|p| status.entries.iter().any(|e| &e.path == p));
                    }
                    Event::Diff { .. } => tab.diff_gen = tab.diff_gen.wrapping_add(1),
                    Event::CommitDiff { .. } => {
                        tab.commit_diff_gen = tab.commit_diff_gen.wrapping_add(1);
                    }
                    Event::Completed(label) => {
                        // Clear the commit box once a commit succeeds.
                        if label == "commit" {
                            tab.commit_title.clear();
                            tab.commit_body.clear();
                            tab.amend = false;
                        }
                        tab.busy = None;
                        tab.pending_net = None;
                    }
                    Event::Failed { label, error } => {
                        if label == "load-history" {
                            tab.loading = false;
                        }
                        tab.busy = None;
                        // A network op that died for lack of credentials gets
                        // the normal username/password prompt and a retry.
                        if let Some(nc) = tab.pending_net.take() {
                            if is_auth_error(error) && askpass_helper().is_some() {
                                auth_req = Some(AuthDialog {
                                    repo: repo_idx,
                                    retry: nc,
                                    label: label.clone(),
                                    username: String::new(),
                                    password: String::new(),
                                });
                            }
                        }
                    }
                    // Raw git output lines feed the operation-details transcript
                    // (progress meters drive the bar via Event::Progress instead).
                    Event::OpOutput { line, .. } => {
                        tab.op_log.push(line.clone());
                        if tab.op_log.len() > 1000 {
                            let excess = tab.op_log.len() - 1000;
                            tab.op_log.drain(0..excess);
                        }
                    }
                    _ => {}
                }
                tab.state.apply(ev);
            }
        }
        if self.auth_dialog.is_none() {
            self.auth_dialog = auth_req;
        }
    }
}

impl eframe::App for GittifyApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.styled {
            configure_style(ctx);
            self.styled = true;
        }
        if self.ui_ctx.is_none() {
            self.ui_ctx = Some(ctx.clone());
        }
        self.poll_picker();
        self.sync_open_tabs();
        self.drain_events();

        // Native menu-bar picks (macOS).
        #[cfg(target_os = "macos")]
        {
            let mut actions = Vec::new();
            if let Some(mb) = &self.menubar {
                while let Some(action) = mb.poll() {
                    actions.push(action);
                }
            }
            for action in actions {
                self.apply_menu_action(ctx, action);
            }
        }

        // Fullscreen toggle: macOS gets the system Ctrl+Cmd+F through the View
        // menu's native fullscreen item, so the in-app shortcut only backstops
        // a failed menu install; other platforms use the conventional F11.
        #[cfg(target_os = "macos")]
        let toggle_fs = self.menubar.is_none()
            && ctx
                .input(|i| i.modifiers.mac_cmd && i.modifiers.ctrl && i.key_pressed(egui::Key::F));
        #[cfg(not(target_os = "macos"))]
        let toggle_fs = ctx.input(|i| i.key_pressed(egui::Key::F11));
        if toggle_fs {
            let fs = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fs));
        }

        // --- tab bar: the workspace's repositories, topmost like a browser ---
        let mut tab_select: Option<usize> = None;
        let mut tab_close: Option<usize> = None;
        let mut want_add_tab = false;
        let mut want_home = false;
        let mut ws_select: Option<u64> = None;
        let mut want_settings = false;
        egui::TopBottomPanel::top("tabbar")
            .exact_height(TAB_HEIGHT + 12.0)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    ui.add_space(6.0);
                    let (repos, active_tab) = self
                        .workspaces
                        .active_node()
                        .map(|w| (w.repos.clone(), w.active_tab))
                        .unwrap_or_default();
                    // Pinned Home tab: the workspace's repository library.
                    let home_active = self.home_selected || repos.is_empty();
                    if ui
                        .selectable_label(home_active, "  Home  ")
                        .on_hover_text("This workspace's repository library")
                        .clicked()
                    {
                        want_home = true;
                    }
                    ui.add_space(2.0);
                    for (i, path) in repos.iter().enumerate() {
                        let name = path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| path.display().to_string());
                        let loading = self.repos.iter().any(|t| &t.path == path && t.loading);
                        let active = i == active_tab;
                        if draw_repo_tab(ui, &name, path, active, loading, &mut tab_close, i) {
                            tab_select = Some(i);
                        }
                    }
                    if ui
                        .small_button(icon::ADD)
                        .on_hover_text("Add a repository to this workspace")
                        .clicked()
                    {
                        want_add_tab = true;
                    }
                    // Workspace selector, pinned to the window's top-right
                    // corner (Fork keeps its account/workspace switch there).
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(6.0);
                        workspace_dropdown(
                            ui,
                            &self.workspaces,
                            &mut ws_select,
                            &mut want_settings,
                        );
                    });
                });
            });
        if want_home {
            self.home_selected = true;
        }
        if let Some(i) = tab_select {
            if let Some(ws) = self.workspaces.active_node_mut() {
                ws.active_tab = i;
            }
            self.home_selected = false;
            self.persist();
        }
        if let Some(i) = tab_close {
            self.close_tab(i);
        }
        if want_add_tab {
            self.pick_and_add();
        }

        // --- ribbon: captioned tool groups under the tabs ---
        let mut want_add = false;
        let mut want_clone = false;
        let mut want_init = false;
        let mut want_scan = false;
        let mut want_refresh = false;
        let mut branch_cmd: Option<(usize, BranchCmd)> = None;
        let mut open_dialog: Option<BranchDialog> = None;
        let mut net_cmd: Option<NetCmd> = None;
        let mut stash_cmd: Option<StashCmd> = None;
        let mut want_cancel = false;
        let mut want_terminal = false;
        let mut want_finder = false;
        let mut want_editor = false;
        let mut want_details = false;
        let sel_opt = self.active_index();
        egui::TopBottomPanel::top("toolbar")
            .exact_height(60.0)
            .show(ctx, |ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.add_space(4.0);
                    let has_repo = sel_opt.is_some();
                    ribbon_group(ui, "Repository", |ui| {
                        ui.menu_button(
                            format!("{}  Repository  {}", icon::FOLDER, icon::CARET_DOWN),
                            |ui| {
                                ui.set_min_width(220.0);
                                if ui
                                    .button(format!("{}  Add existing repository…", icon::ADD))
                                    .clicked()
                                {
                                    want_add = true;
                                    ui.close();
                                }
                                if ui
                                    .button(format!("{}  Clone repository…", icon::REMOTE))
                                    .clicked()
                                {
                                    want_clone = true;
                                    ui.close();
                                }
                                if ui
                                    .button(format!("{}  New repository…", icon::FOLDER))
                                    .clicked()
                                {
                                    want_init = true;
                                    ui.close();
                                }
                                if ui
                                    .button(format!(
                                        "{}  Scan a folder for repositories…",
                                        icon::REFRESH
                                    ))
                                    .clicked()
                                {
                                    want_scan = true;
                                    ui.close();
                                }
                                if ui
                                    .add_enabled(
                                        has_repo,
                                        egui::Button::new(format!("{}  Refresh", icon::REFRESH)),
                                    )
                                    .clicked()
                                {
                                    want_refresh = true;
                                    ui.close();
                                }
                                ui.separator();
                                if ui
                                    .add_enabled(has_repo, egui::Button::new("Manage remotes…"))
                                    .clicked()
                                {
                                    if let Some(sel) = sel_opt {
                                        branch_cmd = Some((sel, BranchCmd::ManageRemotes));
                                    }
                                    ui.close();
                                }
                                if ui
                                    .button(format!("{}  Manage workspaces…", icon::SETTINGS))
                                    .clicked()
                                {
                                    want_settings = true;
                                    ui.close();
                                }
                            },
                        );
                    });
                    ui.separator();
                    if let Some(sel) = sel_opt {
                        let tab = &mut self.repos[sel];
                        let (ahead, behind) = tab
                            .state
                            .status
                            .as_ref()
                            .filter(|s| s.upstream.is_some())
                            .map(|s| (s.ahead, s.behind))
                            .unwrap_or((0, 0));
                        ribbon_group(ui, "Branch", |ui| {
                            branch_menu(ui, sel, tab, &mut branch_cmd, &mut open_dialog);
                            if ahead > 0 || behind > 0 {
                                ui.label(
                                    egui::RichText::new(ahead_behind_label(ahead, behind))
                                        .color(Color32::from_gray(160)),
                                )
                                .on_hover_text("Commits ahead / behind the upstream branch");
                            }
                        });
                        ui.separator();
                        let has_remote = !tab.state.remotes.is_empty();
                        ribbon_group(ui, "Sync", |ui| {
                            ui.spacing_mut().item_spacing.x = 2.0;
                            if ui
                                .add_enabled(has_remote, egui::Button::new("Fetch"))
                                .on_hover_text("Fetch from the remote")
                                .clicked()
                            {
                                net_cmd = Some(NetCmd::Fetch);
                            }
                            // Per-remote fetch when there is more than one remote.
                            if tab.state.remotes.len() > 1 {
                                ui.menu_button(icon::CARET_DOWN, |ui| {
                                    for r in &tab.state.remotes {
                                        if ui.button(format!("Fetch {}", r.name)).clicked() {
                                            net_cmd = Some(NetCmd::FetchRemote(r.name.clone()));
                                            ui.close();
                                        }
                                    }
                                });
                            }
                            ui.add_space(4.0);
                            let pull_label = if behind > 0 {
                                format!("{} Pull ({behind})", icon::PULL)
                            } else {
                                format!("{} Pull", icon::PULL)
                            };
                            if ui
                                .add_enabled(has_remote, egui::Button::new(pull_label))
                                .on_hover_text("Pull (fetch + integrate) the current branch")
                                .clicked()
                            {
                                net_cmd = Some(NetCmd::Pull);
                            }
                            ui.add_space(4.0);
                            let push_label = if ahead > 0 {
                                format!("{} Push ({ahead})", icon::PUSH)
                            } else {
                                format!("{} Push", icon::PUSH)
                            };
                            if ui
                                .add_enabled(has_remote, egui::Button::new(push_label))
                                .on_hover_text("Push the current branch")
                                .clicked()
                            {
                                net_cmd = Some(NetCmd::Push);
                            }
                            ui.menu_button(icon::CARET_DOWN, |ui| {
                                if ui
                                    .add_enabled(
                                        has_remote,
                                        egui::Button::new("Push (force with lease)"),
                                    )
                                    .on_hover_text(
                                        "Force-push the current branch, refusing to overwrite \
                                         work you haven't seen (--force-with-lease)",
                                    )
                                    .clicked()
                                {
                                    net_cmd = Some(NetCmd::PushForce);
                                    ui.close();
                                }
                            });
                        });
                        ui.separator();
                        ribbon_group(ui, "Stash", |ui| {
                            stash_menu(ui, tab, &mut stash_cmd);
                        });
                        ui.separator();
                    }
                    // Right side: the "Open in" menu, then status (progress /
                    // changed count / errors) flowing leftward.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(4.0);
                        if has_repo {
                            ui.menu_button(
                                format!("{}  Open in  {}", icon::PUSH, icon::CARET_DOWN),
                                |ui| {
                                    ui.set_min_width(180.0);
                                    if ui.button(">_  Terminal").clicked() {
                                        want_terminal = true;
                                        ui.close();
                                    }
                                    if ui
                                        .button(format!("{}  {}", icon::FOLDER, FILE_MANAGER_NAME))
                                        .clicked()
                                    {
                                        want_finder = true;
                                        ui.close();
                                    }
                                    if ui.button(format!("{}  Editor", icon::RENAME)).clicked() {
                                        want_editor = true;
                                        ui.close();
                                    }
                                },
                            );
                            ui.separator();
                        }
                        if let Some(err) = &self.add_error {
                            ui.colored_label(Color32::from_rgb(0xff, 0x6b, 0x6b), err);
                        }
                        if let Some(tab) = sel_opt.and_then(|s| self.repos.get(s)) {
                            if let Some(busy) = &tab.busy {
                                if tab.state.progress.is_some()
                                    && ui.small_button("Cancel").clicked()
                                {
                                    want_cancel = true;
                                }
                                if ui.small_button("Details").clicked() {
                                    want_details = true;
                                }
                                match &tab.state.progress {
                                    // Network ops report a percentage: a real bar.
                                    Some((_, p)) if p.percent.is_some() => {
                                        let frac = p.percent.unwrap_or(0.0) / 100.0;
                                        let text = match (p.current, p.total) {
                                            (Some(c), Some(t)) => format!("{}: {c}/{t}", p.phase),
                                            _ => p.phase.clone(),
                                        };
                                        ui.add(
                                            egui::ProgressBar::new(frac)
                                                .desired_width(180.0)
                                                .text(text),
                                        );
                                    }
                                    // Merge-family ops don't stream: indeterminate.
                                    _ => {
                                        ui.label(
                                            egui::RichText::new(format!("{busy}…"))
                                                .color(Color32::from_gray(160)),
                                        );
                                        ui.spinner();
                                    }
                                }
                            } else if let Some(status) = &tab.state.status {
                                let n = status.entries.len();
                                if n > 0 {
                                    ui.label(
                                        egui::RichText::new(format!("{n} changed"))
                                            .color(Color32::from_gray(140)),
                                    );
                                }
                            }
                        }
                    });
                });
            });
        if want_add {
            self.pick_and_add();
        }
        if want_clone {
            self.open_clone_dialog();
        }
        if want_init {
            self.open_init_dialog();
        }
        if want_scan {
            self.pick_folder(PickFor::ScanRoot);
        }
        if want_refresh {
            if let Some(sel) = self.active_index() {
                self.repos[sel].reload();
            }
        }
        if let Some(id) = ws_select {
            self.workspaces.active = id;
            self.persist();
            self.sync_open_tabs();
        }
        if want_settings {
            self.settings_open = true;
        }
        if let Some(dialog) = open_dialog {
            self.branch_dialog = Some(dialog);
        }
        self.apply_branch_cmd(branch_cmd);
        if let Some(nc) = net_cmd {
            if let Some(sel) = self.active_index() {
                self.dispatch_net(sel, nc);
            }
        }
        self.apply_stash_cmd(stash_cmd);
        if want_cancel {
            if let Some(tab) = self.active_index().and_then(|s| self.repos.get(s)) {
                tab.handle.request_cancel();
            }
        }
        if want_terminal {
            if let Some(tab) = self.active_index().and_then(|s| self.repos.get(s)) {
                open_in_terminal(&tab.path);
            }
        }
        if want_finder {
            if let Some(tab) = self.active_index().and_then(|s| self.repos.get(s)) {
                open_in_file_manager(&tab.path);
            }
        }
        if want_editor {
            if let Some(tab) = self.active_index().and_then(|s| self.repos.get(s)) {
                open_in_editor(&tab.path);
            }
        }
        if want_details {
            self.show_op_details = !self.show_op_details;
        }

        // Home showing? (Selected explicitly, or forced when no tabs exist.)
        let show_home = self.home_selected || self.active_index().is_none();

        // --- repository sidebar (view switch / branches / remotes / tags /
        // stashes), collapsible to a thin rail via its own embedded button ---
        if let Some(sel) = self.active_index().filter(|_| !show_home) {
            if self.show_sidebar {
                let mut out = SidebarOut::default();
                egui::SidePanel::left("sidebar")
                    .resizable(true)
                    .default_width(230.0)
                    .width_range(170.0..=420.0)
                    .show(ctx, |ui| {
                        sidebar_ui(ui, sel, &mut self.repos[sel], &mut out);
                    });
                if out.collapse {
                    self.show_sidebar = false;
                }
                if let Some(view) = out.set_view {
                    let tab = &mut self.repos[sel];
                    tab.view = view;
                    if matches!(view, View::Changes) {
                        tab.handle.send(Command::LoadStatus);
                    }
                }
                if let Some(dialog) = out.open_dialog {
                    self.branch_dialog = Some(dialog);
                }
                if let Some(target) = out.tag_dialog_at {
                    self.tag_dialog = Some(TagDialog {
                        repo: sel,
                        target,
                        name: String::new(),
                        message: String::new(),
                    });
                }
                self.apply_branch_cmd(out.branch_cmd);
                self.apply_stash_cmd(out.stash_cmd);
            } else {
                egui::SidePanel::left("sidebar-rail")
                    .resizable(false)
                    .exact_width(30.0)
                    .show(ctx, |ui| {
                        ui.add_space(8.0);
                        ui.vertical_centered(|ui| {
                            if ui
                                .small_button(icon::CARET_RIGHT)
                                .on_hover_text("Expand the sidebar")
                                .clicked()
                            {
                                self.show_sidebar = true;
                            }
                        });
                    });
            }
        }

        // --- selected commit detail (History view, only while a commit is
        // selected; a second click on the commit hides it again) ---
        let active = self.active_index();
        let show_detail = !show_home
            && active
                .map(|s| {
                    matches!(self.repos[s].view, View::History)
                        && self.repos[s].selected_commit.is_some()
                })
                .unwrap_or(false);
        if show_detail {
            // Fixed height: compact by default, taller when the user expands the
            // full message via the panel's "Show more" caret (no manual drag).
            let expanded = active
                .map(|s| self.repos[s].detail_expanded)
                .unwrap_or(false);
            let height = if expanded { 320.0 } else { 96.0 };
            egui::TopBottomPanel::bottom("detail")
                .resizable(false)
                .exact_height(height)
                .show(ctx, |ui| self.detail_ui(ui));
        }

        // --- main content ---
        egui::CentralPanel::default().show(ctx, |ui| self.central_ui(ui));

        // --- modals ---
        self.confirm_discard_modal(ctx);
        self.confirm_reset_modal(ctx);
        self.clone_dialog_modal(ctx);
        self.init_dialog_modal(ctx);
        self.auth_dialog_modal(ctx);
        self.scan_dialog_modal(ctx);
        self.branch_dialog_modal(ctx);
        self.tag_dialog_modal(ctx);
        self.remotes_dialog_modal(ctx);
        self.op_details_window(ctx);
        self.settings_modal(ctx);

        // Workers wake the UI per event (see `EventWaker`), so no idle polling
        // is needed. Keep a slow safety tick only while an operation is in
        // flight, so progress bars and spinners never stall even if a waker is
        // missed.
        if self
            .repos
            .iter()
            .any(|t| t.busy.is_some() || t.loading || t.loading_more)
        {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}

impl GittifyApp {
    fn central_ui(&mut self, ui: &mut egui::Ui) {
        let sel = self.active_index();
        if self.home_selected || sel.is_none() {
            let mut actions = Vec::new();
            self.home.ui(
                ui,
                self.workspaces.active_node(),
                &self.workspaces.recent,
                &mut actions,
            );
            for action in actions {
                self.apply_home_action(action);
            }
            return;
        }
        let sel = sel.expect("checked above");
        match self.repos[sel].view {
            View::History => self.graph_view(ui, sel),
            View::Changes => {
                if let Some((tracked, untracked)) = changes_ui(&mut self.repos[sel], ui) {
                    self.confirm_discard = Some((sel, tracked, untracked));
                }
            }
        }
    }

    fn confirm_discard_modal(&mut self, ctx: &egui::Context) {
        let Some((idx, tracked, untracked)) = self.confirm_discard.clone() else {
            return;
        };
        let total = tracked.len() + untracked.len();
        let mut do_it = false;
        let mut cancel = false;
        egui::Window::new("Discard changes")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.add_space(4.0);
                ui.label(format!("Permanently discard changes to {total} file(s)?"));
                ui.colored_label(
                    Color32::from_rgb(0xff, 0x9b, 0x6b),
                    "This cannot be undone.",
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(egui::Button::new("Discard").fill(Color32::from_rgb(0x8a, 0x2c, 0x2c)))
                        .clicked()
                    {
                        do_it = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if do_it {
            if let Some(tab) = self.repos.get(idx) {
                // Tracked paths are restored; untracked paths are deleted.
                if !tracked.is_empty() {
                    tab.handle.send(Command::Discard {
                        paths: tracked,
                        untracked: false,
                    });
                }
                if !untracked.is_empty() {
                    tab.handle.send(Command::Discard {
                        paths: untracked,
                        untracked: true,
                    });
                }
            }
            self.confirm_discard = None;
        } else if cancel {
            self.confirm_discard = None;
        }
    }

    /// Confirm a destructive `reset --hard` to a commit.
    fn confirm_reset_modal(&mut self, ctx: &egui::Context) {
        let Some((idx, target)) = self.confirm_reset.clone() else {
            return;
        };
        let mut do_it = false;
        let mut cancel = false;
        egui::Window::new("Reset --hard")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.add_space(4.0);
                ui.label(format!(
                    "Reset the current branch to {} and discard all changes?",
                    short_hex(&target)
                ));
                ui.colored_label(
                    Color32::from_rgb(0xff, 0x9b, 0x6b),
                    "Uncommitted changes will be lost. This cannot be undone.",
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(
                            egui::Button::new("Reset --hard")
                                .fill(Color32::from_rgb(0x8a, 0x2c, 0x2c)),
                        )
                        .clicked()
                    {
                        do_it = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if do_it {
            if let Some(tab) = self.repos.get(idx) {
                tab.handle.send(Command::Reset {
                    target,
                    mode: ResetMode::Hard,
                });
            }
            self.confirm_reset = None;
        } else if cancel {
            self.confirm_reset = None;
        }
    }

    /// Render the clone-repository dialog: URL + destination folder, with the
    /// clone running on a background thread while the dialog shows progress.
    fn clone_dialog_modal(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.clone_dialog.take() else {
            return;
        };
        // Resolve an in-flight clone first.
        if let Some(rx) = &dialog.rx {
            match rx.try_recv() {
                Ok(Ok(path)) => {
                    // Dropping the dialog closes it; the clone becomes a tab.
                    self.add_repo(path);
                    return;
                }
                Ok(Err(err)) => {
                    // A credential failure turns the dialog into the normal
                    // auth prompt: username/password fields plus retry.
                    if is_auth_error(&err) && askpass_helper().is_some() {
                        dialog.need_auth = true;
                        dialog.error = Some(
                            "Authentication required. For HTTPS remotes, use a personal \
                             access token as the password."
                                .to_string(),
                        );
                    } else {
                        dialog.error = Some(err);
                    }
                    dialog.rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    dialog.error = Some("The clone was interrupted.".to_string());
                    dialog.rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        let busy = dialog.rx.is_some();
        let target = match (&dialog.dest, repo_name_from_url(&dialog.url)) {
            (Some(dest), Some(name)) => Some(dest.join(name)),
            _ => None,
        };
        let mut start = false;
        let mut close = false;
        let mut pick = false;
        egui::Window::new("Clone repository")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.add_space(4.0);
                egui::Grid::new("clone-form")
                    .num_columns(2)
                    .spacing([8.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("URL");
                        ui.add_enabled(
                            !busy,
                            egui::TextEdit::singleline(&mut dialog.url)
                                .hint_text("https://… or git@…")
                                .desired_width(340.0),
                        );
                        ui.end_row();
                        ui.label("Folder");
                        let shown = dialog
                            .dest
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "Choose…".to_string());
                        if ui.add_enabled(!busy, egui::Button::new(shown)).clicked() {
                            pick = true;
                        }
                        ui.end_row();
                        if dialog.need_auth {
                            ui.label("Username");
                            ui.add_enabled(
                                !busy,
                                egui::TextEdit::singleline(&mut dialog.username)
                                    .desired_width(340.0),
                            );
                            ui.end_row();
                            ui.label("Password");
                            ui.add_enabled(
                                !busy,
                                egui::TextEdit::singleline(&mut dialog.password)
                                    .password(true)
                                    .desired_width(340.0),
                            );
                            ui.end_row();
                        }
                    });
                if let Some(target) = &target {
                    ui.label(
                        egui::RichText::new(format!("Will clone into {}", target.display()))
                            .color(Color32::from_gray(140)),
                    );
                }
                if let Some(err) = &dialog.error {
                    ui.colored_label(Color32::from_rgb(0xff, 0x6b, 0x6b), err);
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if busy {
                        ui.spinner();
                        ui.label("Cloning…");
                    } else {
                        if ui
                            .add_enabled(target.is_some(), egui::Button::new("Clone"))
                            .clicked()
                        {
                            start = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    }
                });
            });
        if busy {
            // Keep the spinner moving and the result channel polled.
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if start {
            if let Some(target) = target {
                if target.exists() {
                    dialog.error = Some(format!("{} already exists.", target.display()));
                } else {
                    dialog.error = None;
                    let creds = dialog
                        .need_auth
                        .then(|| session_credentials(&dialog.username, &dialog.password));
                    dialog.rx = Some(spawn_clone(
                        dialog.url.trim().to_string(),
                        target,
                        creds,
                        self.ui_ctx.clone(),
                    ));
                }
            }
        }
        if pick {
            self.pick_folder(PickFor::CloneDest);
        }
        if !close {
            self.clone_dialog = Some(dialog);
        }
    }

    /// Render the scan-folder dialog: a background walk of the chosen root,
    /// then a checkbox list for bulk-adding the found repos to the active
    /// workspace's library.
    fn scan_dialog_modal(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.scan_dialog.take() else {
            return;
        };
        // Resolve the background walk.
        if let Some(rx) = &dialog.rx {
            match rx.try_recv() {
                Ok(paths) => {
                    let library = self
                        .workspaces
                        .active_node()
                        .map(|w| w.library.clone())
                        .unwrap_or_default();
                    dialog.found = paths
                        .into_iter()
                        .map(|path| {
                            let already = library.iter().any(|p| p == &path);
                            ScanEntry {
                                path,
                                checked: !already,
                                already,
                            }
                        })
                        .collect();
                    dialog.rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => dialog.rx = None,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        let scanning = dialog.rx.is_some();
        let mut add = false;
        let mut close = false;
        egui::Window::new("Scan folder for repositories")
            .collapsible(false)
            .resizable(true)
            .default_width(460.0)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(dialog.root.display().to_string())
                        .color(Color32::from_gray(140)),
                );
                ui.add_space(4.0);
                if scanning {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Scanning…");
                    });
                } else if dialog.found.is_empty() {
                    ui.weak("No repositories found under this folder.");
                } else {
                    ui.horizontal(|ui| {
                        if ui.small_button("Select all").clicked() {
                            for e in dialog.found.iter_mut().filter(|e| !e.already) {
                                e.checked = true;
                            }
                        }
                        if ui.small_button("Select none").clicked() {
                            for e in dialog.found.iter_mut() {
                                e.checked = false;
                            }
                        }
                        ui.label(
                            egui::RichText::new(format!("{} found", dialog.found.len()))
                                .size(11.0)
                                .color(Color32::from_gray(140)),
                        );
                    });
                    ui.add_space(2.0);
                    egui::ScrollArea::vertical()
                        .max_height(320.0)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for e in dialog.found.iter_mut() {
                                let rel = e
                                    .path
                                    .strip_prefix(&dialog.root)
                                    .unwrap_or(&e.path)
                                    .display()
                                    .to_string();
                                let label = if rel.is_empty() { ".".to_string() } else { rel };
                                if e.already {
                                    ui.add_enabled(
                                        false,
                                        egui::Checkbox::new(
                                            &mut e.checked,
                                            format!("{label}  · already added"),
                                        ),
                                    );
                                } else {
                                    ui.checkbox(&mut e.checked, label);
                                }
                            }
                        });
                }
                ui.add_space(8.0);
                let picked = dialog
                    .found
                    .iter()
                    .filter(|e| e.checked && !e.already)
                    .count();
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            picked > 0,
                            egui::Button::new(format!("Add {picked} repositories")),
                        )
                        .clicked()
                    {
                        add = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
        if scanning {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if add {
            if let Some(ws) = self.workspaces.active_node_mut() {
                for e in dialog.found.iter().filter(|e| e.checked && !e.already) {
                    ws.add_to_library(&e.path);
                }
            }
            self.persist();
            // Show the freshly stocked library.
            self.home_selected = true;
            return;
        }
        if !close {
            self.scan_dialog = Some(dialog);
        }
    }

    /// Render the credential prompt for a failed network op: username +
    /// password/token, routed through `gg-askpass` and retried on submit.
    fn auth_dialog_modal(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.auth_dialog.take() else {
            return;
        };
        let repo_name = self
            .repos
            .get(dialog.repo)
            .map(|t| t.name.clone())
            .unwrap_or_default();
        let mut submit = false;
        let mut close = false;
        egui::Window::new("Authentication required")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.add_space(4.0);
                ui.label(format!(
                    "{} for {repo_name} needs credentials.",
                    dialog.label
                ));
                ui.label(
                    egui::RichText::new(
                        "They are kept for this session only. For HTTPS remotes, use a \
                         personal access token as the password.",
                    )
                    .color(Color32::from_gray(140)),
                );
                ui.add_space(6.0);
                egui::Grid::new("auth-form")
                    .num_columns(2)
                    .spacing([8.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Username");
                        ui.add(
                            egui::TextEdit::singleline(&mut dialog.username).desired_width(240.0),
                        );
                        ui.end_row();
                        ui.label("Password");
                        ui.add(
                            egui::TextEdit::singleline(&mut dialog.password)
                                .password(true)
                                .desired_width(240.0),
                        );
                        ui.end_row();
                    });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let ready = !dialog.password.is_empty() || !dialog.username.is_empty();
                    if ui.add_enabled(ready, egui::Button::new("Retry")).clicked() {
                        submit = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
        if submit {
            if let (Some(askpass), Some(tab)) = (askpass_helper(), self.repos.get(dialog.repo)) {
                tab.handle.send(Command::SetCredentials {
                    askpass,
                    creds: session_credentials(&dialog.username, &dialog.password),
                });
                let repo = dialog.repo;
                let retry = dialog.retry.clone();
                // Dropping the dialog closes it; the retried op reports
                // through the usual busy indicator.
                self.dispatch_net(repo, retry);
                return;
            }
            close = true;
        }
        if !close {
            self.auth_dialog = Some(dialog);
        }
    }

    /// Render the new-repository dialog: parent folder + name, `git init`ed
    /// and opened as a tab on confirm.
    fn init_dialog_modal(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.init_dialog.take() else {
            return;
        };
        let name_ok = {
            let name = dialog.name.trim();
            !name.is_empty() && !name.contains(['/', '\\'])
        };
        let mut create = false;
        let mut close = false;
        let mut pick = false;
        egui::Window::new("New repository")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.add_space(4.0);
                egui::Grid::new("init-form")
                    .num_columns(2)
                    .spacing([8.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Name");
                        ui.add(
                            egui::TextEdit::singleline(&mut dialog.name)
                                .hint_text("my-project")
                                .desired_width(260.0),
                        );
                        ui.end_row();
                        ui.label("Folder");
                        let shown = dialog
                            .parent
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "Choose…".to_string());
                        if ui.button(shown).clicked() {
                            pick = true;
                        }
                        ui.end_row();
                    });
                if let (Some(parent), true) = (&dialog.parent, name_ok) {
                    ui.label(
                        egui::RichText::new(format!(
                            "Will create {}",
                            parent.join(dialog.name.trim()).display()
                        ))
                        .color(Color32::from_gray(140)),
                    );
                }
                if let Some(err) = &dialog.error {
                    ui.colored_label(Color32::from_rgb(0xff, 0x6b, 0x6b), err);
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            dialog.parent.is_some() && name_ok,
                            egui::Button::new("Create"),
                        )
                        .clicked()
                    {
                        create = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
        if create {
            if let Some(parent) = &dialog.parent {
                let target = parent.join(dialog.name.trim());
                match init_repo(&target) {
                    Ok(()) => {
                        // Dropping the dialog closes it; open the new repo.
                        self.add_repo(target);
                        return;
                    }
                    Err(err) => dialog.error = Some(err),
                }
            }
        }
        if pick {
            self.pick_folder(PickFor::InitParent);
        }
        if !close {
            self.init_dialog = Some(dialog);
        }
    }

    /// Render the active branch dialog (new / rename / delete) as a modal,
    /// dispatching the corresponding command when confirmed.
    fn branch_dialog_modal(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.branch_dialog.take() else {
            return;
        };
        // Re-stored unless the dialog was confirmed or cancelled this frame.
        let mut next: Option<BranchDialog> = None;
        let send = |repo: usize, cmd: Command, app: &Self| {
            if let Some(tab) = app.repos.get(repo) {
                tab.handle.send(cmd);
            }
        };
        match dialog {
            BranchDialog::New {
                repo,
                mut name,
                start,
                mut checkout,
            } => {
                let mut create = false;
                let mut cancel = false;
                egui::Window::new("New branch")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                    .show(ctx, |ui| {
                        ui.add_space(4.0);
                        if let Some(s) = &start {
                            ui.label(format!("Start point: {}", short_hex(s)));
                            ui.add_space(2.0);
                        }
                        ui.label("Branch name");
                        ui.add(
                            egui::TextEdit::singleline(&mut name)
                                .hint_text("feature/my-branch")
                                .desired_width(280.0),
                        );
                        ui.add_space(4.0);
                        ui.checkbox(&mut checkout, "Check out after creating");
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            let valid = !name.trim().is_empty();
                            if ui.add_enabled(valid, egui::Button::new("Create")).clicked() {
                                create = true;
                            }
                            if ui.button("Cancel").clicked() {
                                cancel = true;
                            }
                        });
                    });
                if create {
                    send(
                        repo,
                        Command::CreateBranch {
                            name: name.trim().to_string(),
                            start: start.clone(),
                            checkout,
                        },
                        self,
                    );
                } else if !cancel {
                    next = Some(BranchDialog::New {
                        repo,
                        name,
                        start,
                        checkout,
                    });
                }
            }
            BranchDialog::Rename {
                repo,
                old,
                mut name,
            } => {
                let mut rename = false;
                let mut cancel = false;
                egui::Window::new("Rename branch")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                    .show(ctx, |ui| {
                        ui.add_space(4.0);
                        ui.label(format!("Rename “{old}” to:"));
                        ui.add(egui::TextEdit::singleline(&mut name).desired_width(280.0));
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            let valid = !name.trim().is_empty() && name.trim() != old;
                            if ui.add_enabled(valid, egui::Button::new("Rename")).clicked() {
                                rename = true;
                            }
                            if ui.button("Cancel").clicked() {
                                cancel = true;
                            }
                        });
                    });
                if rename {
                    send(
                        repo,
                        Command::RenameBranch {
                            old: old.clone(),
                            new: name.trim().to_string(),
                        },
                        self,
                    );
                } else if !cancel {
                    next = Some(BranchDialog::Rename { repo, old, name });
                }
            }
            BranchDialog::Delete {
                repo,
                name,
                mut force,
            } => {
                let mut delete = false;
                let mut cancel = false;
                egui::Window::new("Delete branch")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                    .show(ctx, |ui| {
                        ui.add_space(4.0);
                        ui.label(format!("Delete branch “{name}”?"));
                        ui.checkbox(&mut force, "Force delete (drop unmerged commits)");
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui
                                .add(
                                    egui::Button::new("Delete")
                                        .fill(Color32::from_rgb(0x8a, 0x2c, 0x2c)),
                                )
                                .clicked()
                            {
                                delete = true;
                            }
                            if ui.button("Cancel").clicked() {
                                cancel = true;
                            }
                        });
                    });
                if delete {
                    send(
                        repo,
                        Command::DeleteBranch {
                            name: name.clone(),
                            force,
                        },
                        self,
                    );
                } else if !cancel {
                    next = Some(BranchDialog::Delete { repo, name, force });
                }
            }
        }
        self.branch_dialog = next;
    }

    /// Render the create-tag dialog and dispatch `CreateTag` when confirmed.
    fn tag_dialog_modal(&mut self, ctx: &egui::Context) {
        let Some(mut dlg) = self.tag_dialog.take() else {
            return;
        };
        let mut create = false;
        let mut cancel = false;
        egui::Window::new("New tag")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.add_space(4.0);
                if let Some(t) = &dlg.target {
                    ui.label(format!("Target: {}", short_hex(t)));
                    ui.add_space(2.0);
                }
                ui.label("Tag name");
                ui.add(
                    egui::TextEdit::singleline(&mut dlg.name)
                        .hint_text("v1.0.0")
                        .desired_width(280.0),
                );
                ui.add_space(4.0);
                ui.label("Message (optional, annotated tag)");
                ui.add(
                    egui::TextEdit::multiline(&mut dlg.message)
                        .hint_text("Release notes…")
                        .desired_rows(2)
                        .desired_width(280.0),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let valid = !dlg.name.trim().is_empty();
                    if ui.add_enabled(valid, egui::Button::new("Create")).clicked() {
                        create = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if create {
            if let Some(tab) = self.repos.get(dlg.repo) {
                let message =
                    (!dlg.message.trim().is_empty()).then(|| dlg.message.trim().to_string());
                tab.handle.send(Command::CreateTag {
                    name: dlg.name.trim().to_string(),
                    target: dlg.target.clone(),
                    message,
                });
            }
        } else if !cancel {
            self.tag_dialog = Some(dlg);
        }
    }

    /// Render the remotes-management dialog (list, remove, add).
    fn remotes_dialog_modal(&mut self, ctx: &egui::Context) {
        let Some(mut dlg) = self.remotes_dialog.take() else {
            return;
        };
        let remotes = self
            .repos
            .get(dlg.repo)
            .map(|t| t.state.remotes.clone())
            .unwrap_or_default();
        let mut close = false;
        let mut to_remove: Option<String> = None;
        let mut to_add = false;
        egui::Window::new("Remotes")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.add_space(4.0);
                if remotes.is_empty() {
                    ui.weak("No remotes configured.");
                } else {
                    egui::Grid::new("remotes-grid")
                        .num_columns(3)
                        .spacing([12.0, 4.0])
                        .show(ui, |ui| {
                            for r in &remotes {
                                ui.label(egui::RichText::new(&r.name).strong());
                                ui.label(
                                    egui::RichText::new(&r.url).color(Color32::from_gray(150)),
                                );
                                if ui
                                    .small_button(icon::DELETE)
                                    .on_hover_text("Remove")
                                    .clicked()
                                {
                                    to_remove = Some(r.name.clone());
                                }
                                ui.end_row();
                            }
                        });
                }
                ui.add_space(8.0);
                ui.separator();
                ui.label(
                    egui::RichText::new("ADD REMOTE")
                        .size(10.0)
                        .color(Color32::from_gray(130)),
                );
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut dlg.name)
                            .hint_text("name")
                            .desired_width(100.0),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut dlg.url)
                            .hint_text("https://… or git@…")
                            .desired_width(260.0),
                    );
                    let valid = !dlg.name.trim().is_empty() && !dlg.url.trim().is_empty();
                    if ui.add_enabled(valid, egui::Button::new("Add")).clicked() {
                        to_add = true;
                    }
                });
                ui.add_space(8.0);
                if ui.button("Close").clicked() {
                    close = true;
                }
            });
        if let Some(tab) = self.repos.get(dlg.repo) {
            if let Some(name) = to_remove {
                tab.handle.send(Command::RemoveRemote(name));
            }
            if to_add {
                tab.handle.send(Command::AddRemote {
                    name: dlg.name.trim().to_string(),
                    url: dlg.url.trim().to_string(),
                });
                dlg.name.clear();
                dlg.url.clear();
            }
        }
        if !close {
            self.remotes_dialog = Some(dlg);
        }
    }

    /// A floating window showing the streamed progress lines of the current (or
    /// most recent) operation, opened by the toolbar "Details" button.
    fn op_details_window(&mut self, ctx: &egui::Context) {
        if !self.show_op_details {
            return;
        }
        let mut open = true;
        egui::Window::new("Operation details")
            .open(&mut open)
            .default_width(480.0)
            .resizable(true)
            .show(ctx, |ui| {
                let Some(tab) = self.active_index().and_then(|s| self.repos.get(s)) else {
                    ui.weak("No repository selected.");
                    return;
                };
                if let Some(busy) = &tab.busy {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(format!("{busy}…"));
                    });
                    ui.separator();
                }
                if tab.op_log.is_empty() {
                    ui.weak("No detailed output.");
                } else {
                    egui::ScrollArea::vertical()
                        .max_height(320.0)
                        .auto_shrink([false, false])
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            for line in &tab.op_log {
                                ui.label(egui::RichText::new(line).monospace().size(12.0));
                            }
                        });
                }
            });
        if !open {
            self.show_op_details = false;
        }
    }

    /// The manage-workspaces modal: a tree with rename / delete / add and
    /// drag-and-drop reparenting. Rendered from a snapshot + local rename
    /// buffer; collected mutations are applied to the real tree afterward.
    fn settings_modal(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let roots = self.workspaces.roots.clone();
        let active = self.workspaces.active;
        let mut rename = self.ws_rename.clone();
        let mut act = WsActions::default();
        let mut open = true;
        egui::Window::new("Manage workspaces")
            .open(&mut open)
            .default_width(440.0)
            .resizable(true)
            .show(ctx, |ui| {
                // Swallow drags on the body so only the title bar moves the
                // window (this sits above the window's move-anywhere response
                // but below every widget added after it).
                ui.interact(
                    ui.max_rect(),
                    ui.id().with("ws-body-drag-catcher"),
                    egui::Sense::drag(),
                );
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    if ui.button(format!("{}  New workspace", icon::ADD)).clicked() {
                        act.add_root = true;
                    }
                    ui.label(
                        egui::RichText::new(
                            "Drag onto a workspace to nest it; drop between rows to reorder.",
                        )
                        .size(11.0)
                        .color(Color32::from_gray(140)),
                    );
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .max_height(380.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ws_tree_ui(ui, &roots, None, 0, active, false, &mut rename, &mut act);
                        // A full-width strip below the tree: dropping here
                        // moves a workspace to the top level (last position).
                        if egui::DragAndDrop::has_payload_of_type::<u64>(ui.ctx()) {
                            let (rect, strip) = ui.allocate_exact_size(
                                egui::vec2(ui.available_width(), 26.0),
                                egui::Sense::hover(),
                            );
                            let hovered = strip.dnd_hover_payload::<u64>().is_some();
                            let stroke_color = if hovered {
                                Color32::from_rgb(0x6f, 0xa8, 0xff)
                            } else {
                                Color32::from_gray(90)
                            };
                            ui.painter().rect_stroke(
                                rect.shrink(2.0),
                                egui::CornerRadius::same(4),
                                egui::Stroke::new(1.0_f32, stroke_color),
                                egui::StrokeKind::Inside,
                            );
                            ui.painter().text(
                                rect.center(),
                                egui::Align2::CENTER_CENTER,
                                "Drop here to make it top-level",
                                egui::FontId::proportional(11.0),
                                stroke_color,
                            );
                            if let Some(src) = strip.dnd_release_payload::<u64>() {
                                act.reparent = Some((*src, None, usize::MAX));
                            }
                        }
                    });
            });

        self.ws_rename = rename;
        if let Some(id) = act.toggle_expand {
            if let Some(n) = self.workspaces.find_mut(id) {
                n.expanded = !n.expanded;
            }
            self.persist();
        }
        if let Some(id) = act.select {
            self.workspaces.active = id;
            self.sync_open_tabs();
            self.persist();
        }
        if let Some(id) = act.start_rename {
            let name = self
                .workspaces
                .find(id)
                .map(|n| n.name.clone())
                .unwrap_or_default();
            self.ws_rename = Some((id, name));
        }
        if act.cancel_rename {
            self.ws_rename = None;
        }
        if act.commit_rename {
            if let Some((id, name)) = self.ws_rename.take() {
                let name = name.trim().to_string();
                if !name.is_empty() {
                    if let Some(n) = self.workspaces.find_mut(id) {
                        n.name = name;
                    }
                }
                self.persist();
            }
        }
        if act.add_root {
            let id = self.workspaces.next_id();
            self.workspaces
                .insert(None, usize::MAX, WsNode::new(id, "New workspace"));
            self.persist();
        }
        if let Some(parent) = act.add_child {
            let id = self.workspaces.next_id();
            self.workspaces
                .insert(Some(parent), usize::MAX, WsNode::new(id, "New workspace"));
            if let Some(p) = self.workspaces.find_mut(parent) {
                p.expanded = true;
            }
            self.persist();
        }
        if let Some(id) = act.delete {
            self.workspaces.remove(id);
            self.workspaces.normalize();
            self.persist();
        }
        if let Some((id, parent, index)) = act.reparent {
            self.workspaces.move_to(id, parent, index);
            self.persist();
        }
        if !open {
            self.settings_open = false;
            self.ws_rename = None;
        }
    }

    fn graph_view(&mut self, ui: &mut egui::Ui, sel: usize) {
        let mut clicked = None;
        let mut ctx_action: Option<CommitMenuAction> = None;
        let mut file_click: Option<usize> = None;
        {
            let RepoTab {
                name,
                state,
                loading,
                selected_commit,
                selected_commit_file,
                labels,
                commit_doc,
                commit_diff_gen,
                requested_limit,
                history_complete,
                loading_more,
                handle,
                ..
            } = &mut self.repos[sel];
            let sel_oid = selected_commit
                .and_then(|i| state.history.as_ref().and_then(|v| v.commits.get(i)))
                .map(|c| c.oid);
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                ui.label(egui::RichText::new(name.as_str()).size(15.0).strong());
                ui.add_space(8.0);
                ui.label(egui::RichText::new(&state.status_line).color(Color32::from_gray(140)));
                if *loading {
                    ui.spinner();
                }
            });
            if let Some(err) = &state.last_error {
                ui.colored_label(Color32::from_rgb(0xff, 0x6b, 0x6b), err);
            }
            ui.add_space(4.0);

            let mut want_more = false;
            if let Some(oid) = sel_oid {
                // Consolidated commit view: the history shrinks to a minified
                // strip on the left (scroll it horizontally for the subject
                // and author columns) and the changed files get the rest as a
                // tab bar over a full-width diff.
                let selected_file = *selected_commit_file;
                egui::SidePanel::left("history-mini")
                    .resizable(true)
                    .default_width(mini_content_width(MINI_DEFAULT_GUTTER))
                    .width_range(220.0..=620.0)
                    .show_inside(ui, |ui| {
                        if let Some(view) = &state.history {
                            let rows = view.layout.rows();
                            let width = view.layout.max_width().max(1);
                            let gutter = (8.0 + width as f32 * LANE_WIDTH + 8.0).min(MAX_GUTTER);
                            let selected = *selected_commit;
                            let content_w = mini_full_width(gutter);
                            egui::ScrollArea::horizontal()
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    ui.set_min_width(content_w);
                                    let row_w = content_w.max(ui.available_width());
                                    let (hrect, _) = ui.allocate_exact_size(
                                        egui::vec2(row_w, 22.0),
                                        egui::Sense::hover(),
                                    );
                                    draw_mini_header(ui, hrect, gutter);
                                    let total = rows.len() + usize::from(!*history_complete);
                                    egui::ScrollArea::vertical()
                                        .auto_shrink([false, false])
                                        .show_rows(ui, ROW_HEIGHT, total, |ui, range| {
                                            ui.spacing_mut().item_spacing.y = 0.0;
                                            for i in range {
                                                if i >= rows.len() {
                                                    want_more = true;
                                                    let (rect, _) = ui.allocate_exact_size(
                                                        egui::vec2(row_w, ROW_HEIGHT),
                                                        egui::Sense::hover(),
                                                    );
                                                    ui.painter_at(rect).text(
                                                        egui::pos2(
                                                            rect.left() + 12.0,
                                                            rect.center().y,
                                                        ),
                                                        egui::Align2::LEFT_CENTER,
                                                        "Loading older commits…",
                                                        egui::FontId::proportional(12.0),
                                                        Color32::from_gray(130),
                                                    );
                                                    continue;
                                                }
                                                let row = &rows[i];
                                                let commit = &view.commits[i];
                                                let (rect, resp) = ui.allocate_exact_size(
                                                    egui::vec2(row_w, ROW_HEIGHT),
                                                    egui::Sense::click(),
                                                );
                                                if resp.clicked() {
                                                    clicked = Some(i);
                                                }
                                                resp.context_menu(|ui| {
                                                    commit_context_menu(
                                                        ui,
                                                        commit,
                                                        &mut ctx_action,
                                                    );
                                                });
                                                draw_commit_row_mini(
                                                    ui,
                                                    rect,
                                                    row,
                                                    commit,
                                                    i,
                                                    gutter,
                                                    selected == Some(i),
                                                    resp.hovered(),
                                                    labels.get(&commit.oid),
                                                );
                                            }
                                        });
                                });
                        }
                    });
                commit_detail_pane(
                    ui,
                    state,
                    commit_doc,
                    *commit_diff_gen,
                    oid,
                    selected_file,
                    &mut file_click,
                );
            } else if let Some(view) = &state.history {
                let rows = view.layout.rows();
                let width = view.layout.max_width().max(1);
                let gutter = (8.0 + width as f32 * LANE_WIDTH + 8.0).min(MAX_GUTTER);
                let selected = *selected_commit;

                let (hrect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 22.0),
                    egui::Sense::hover(),
                );
                draw_header(ui, hrect, gutter);

                // One extra virtual row acts as the "load older commits" trigger
                // while the history may be truncated.
                let total = rows.len() + usize::from(!*history_complete);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show_rows(ui, ROW_HEIGHT, total, |ui, range| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for i in range {
                            if i >= rows.len() {
                                // Scrolled to the pager row: request the next page.
                                want_more = true;
                                let (rect, _) = ui.allocate_exact_size(
                                    egui::vec2(ui.available_width(), ROW_HEIGHT),
                                    egui::Sense::hover(),
                                );
                                ui.painter_at(rect).text(
                                    egui::pos2(rect.left() + 12.0, rect.center().y),
                                    egui::Align2::LEFT_CENTER,
                                    "Loading older commits…",
                                    egui::FontId::proportional(12.0),
                                    Color32::from_gray(130),
                                );
                                continue;
                            }
                            let row = &rows[i];
                            let commit = &view.commits[i];
                            let (rect, resp) = ui.allocate_exact_size(
                                egui::vec2(ui.available_width(), ROW_HEIGHT),
                                egui::Sense::click(),
                            );
                            if resp.clicked() {
                                clicked = Some(i);
                            }
                            resp.context_menu(|ui| {
                                commit_context_menu(ui, commit, &mut ctx_action);
                            });
                            draw_commit_row(
                                ui,
                                rect,
                                row,
                                commit,
                                i,
                                gutter,
                                selected == Some(i),
                                resp.hovered(),
                                labels.get(&commit.oid),
                            );
                        }
                    });
            } else if !*loading {
                ui.weak("No history.");
            }
            if want_more && !*loading_more && !*history_complete {
                *requested_limit += HISTORY_PAGE;
                *loading_more = true;
                handle.send(Command::LoadHistory(WalkOpts {
                    tips: Vec::new(),
                    limit: Some(*requested_limit),
                    first_parent: false,
                }));
            }
        }
        if let Some(i) = clicked {
            if self.repos[sel].selected_commit == Some(i) {
                // Clicking the selected commit again dismisses the detail
                // view; the history table gets the full window back.
                self.repos[sel].selected_commit = None;
                self.repos[sel].selected_commit_file = None;
            } else {
                self.repos[sel].selected_commit = Some(i);
                self.repos[sel].selected_commit_file = None;
                if let Some(tab) = self.repos.get(sel) {
                    if let Some(c) = tab.state.history.as_ref().and_then(|v| v.commits.get(i)) {
                        tab.handle.send(Command::LoadCommitDiff(c.oid));
                    }
                }
            }
        }
        if let Some(f) = file_click {
            self.repos[sel].selected_commit_file = Some(f);
        }
        if let Some(action) = ctx_action {
            match action {
                CommitMenuAction::NewBranchHere(start) => {
                    self.branch_dialog = Some(BranchDialog::New {
                        repo: sel,
                        name: String::new(),
                        start: Some(start),
                        checkout: true,
                    });
                }
                CommitMenuAction::ResetHardConfirm(target) => {
                    self.confirm_reset = Some((sel, target));
                }
                CommitMenuAction::CreateTagHere(target) => {
                    self.tag_dialog = Some(TagDialog {
                        repo: sel,
                        target: Some(target),
                        name: String::new(),
                        message: String::new(),
                    });
                }
                other => {
                    if let Some(tab) = self.repos.get_mut(sel) {
                        match other {
                            CommitMenuAction::Checkout(h) => tab.handle.send(Command::Checkout(h)),
                            CommitMenuAction::CherryPick(o) => {
                                tab.start_op("Cherry-pick");
                                tab.handle.send(Command::CherryPick(o));
                            }
                            CommitMenuAction::Revert(o) => {
                                tab.start_op("Revert");
                                tab.handle.send(Command::Revert(o));
                            }
                            CommitMenuAction::Reset(t, m) => {
                                tab.handle.send(Command::Reset { target: t, mode: m })
                            }
                            CommitMenuAction::Merge(t) => {
                                tab.start_op("Merge");
                                tab.handle.send(Command::Merge { target: t });
                            }
                            CommitMenuAction::Rebase(t) => {
                                tab.start_op("Rebase");
                                tab.handle.send(Command::Rebase { upstream: t });
                            }
                            // Handled above (they mutate `self`, not the tab).
                            CommitMenuAction::NewBranchHere(_)
                            | CommitMenuAction::ResetHardConfirm(_)
                            | CommitMenuAction::CreateTagHere(_) => {}
                        }
                    }
                }
            }
        }
    }

    fn detail_ui(&mut self, ui: &mut egui::Ui) {
        let Some(sel) = self.active_index() else {
            return;
        };
        let expanded = self.repos[sel].detail_expanded;
        let mut flip = false;
        {
            let tab = &self.repos[sel];
            let Some(view) = &tab.state.history else {
                return;
            };
            let Some(idx) = tab.selected_commit else {
                ui.add_space(8.0);
                ui.weak("  Select a commit to see details.");
                return;
            };
            let Some(commit) = view.commits.get(idx) else {
                return;
            };

            let summary = if commit.summary.is_empty() {
                commit.message.lines().next().unwrap_or("")
            } else {
                &commit.summary
            };

            // Subject on the left, a show more / less caret on the right.
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                ui.label(egui::RichText::new(summary).size(15.0).strong());
                let caret = if expanded {
                    "Show less  ⏷"
                } else {
                    "Show more  ⏵"
                };
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(8.0);
                    if ui.small_button(caret).clicked() {
                        flip = true;
                    }
                });
            });
            ui.add_space(2.0);
            // Compact one-line metadata.
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(format!(
                        "{}  ·  {}  ·  {}",
                        commit.oid.short(8),
                        commit.author.name,
                        fmt_time(commit.author.time)
                    ))
                    .color(Color32::from_gray(150)),
                );
            });

            if expanded {
                ui.add_space(6.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Grid::new("commit-meta")
                            .num_columns(2)
                            .spacing([12.0, 4.0])
                            .show(ui, |ui| {
                                meta_row(ui, "Commit", &commit.oid.to_hex());
                                meta_row(
                                    ui,
                                    "Committer",
                                    &format!(
                                        "{} <{}>  ·  {}",
                                        commit.committer.name,
                                        commit.committer.email,
                                        fmt_time(commit.committer.time)
                                    ),
                                );
                                if !commit.parents.is_empty() {
                                    let parents: Vec<String> =
                                        commit.parents.iter().map(|p| p.short(8)).collect();
                                    meta_row(ui, "Parents", &parents.join(", "));
                                }
                            });
                        ui.add_space(8.0);
                        ui.separator();
                        ui.add_space(4.0);
                        ui.label(egui::RichText::new(commit.message.trim_end()).monospace());
                        ui.add_space(8.0);
                    });
            }
        }
        if flip {
            self.repos[sel].detail_expanded = !expanded;
        }
    }
}

/// Render the Changes view for one repo. Returns a discard request as
/// `(tracked_to_restore, untracked_to_delete)` to confirm, if the user invoked
/// a discard action.
fn changes_ui(tab: &mut RepoTab, ui: &mut egui::Ui) -> Option<(Vec<PathBuf>, Vec<PathBuf>)> {
    // Diff preview on the right. Returns the index of a hunk whose stage /
    // unstage button was clicked, if any.
    let mut hunk_toggle: Option<usize> = None;
    {
        // Split borrows: the cached row model is rebuilt from the state's diff.
        let RepoTab {
            state,
            diff_doc,
            diff_gen,
            ..
        } = &mut *tab;
        egui::SidePanel::right("diffpane")
            .resizable(true)
            .default_width(560.0)
            .show_inside(ui, |ui| {
                hunk_toggle = draw_diff_preview(ui, state.diff.as_ref(), diff_doc, *diff_gen);
            });
    }

    // Commit box pinned to the bottom of the left area. The action row is
    // pinned to the very bottom (its own nested panel) so Commit / Commit &
    // Push are never clipped, regardless of window or panel height; the text
    // fields fill whatever space remains above.
    let mut commit_push: Option<bool> = None;
    egui::TopBottomPanel::bottom("commitbox")
        .resizable(true)
        .default_height(190.0)
        .min_height(150.0)
        .show_inside(ui, |ui| {
            let staged_count = tab
                .state
                .status
                .as_ref()
                .map(|s| s.entries.iter().filter(|e| e.is_staged()).count())
                .unwrap_or(0);
            // Amend lets you commit without newly-staged changes (to edit the
            // previous commit / its message).
            let can_commit = (staged_count > 0 || tab.amend) && !tab.commit_title.trim().is_empty();

            egui::TopBottomPanel::bottom("commit-actions")
                .show_separator_line(false)
                .show_inside(ui, |ui| {
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                can_commit,
                                egui::Button::new(format!("{}  Commit", icon::COMMIT)),
                            )
                            .clicked()
                        {
                            commit_push = Some(false);
                        }
                        // Pushing an amended commit needs a force-push, so the
                        // combined action is disabled while amending.
                        if ui
                            .add_enabled(
                                can_commit && !tab.amend,
                                egui::Button::new(format!("{}  Commit & Push", icon::PUSH)),
                            )
                            .clicked()
                        {
                            commit_push = Some(true);
                        }
                        ui.label(
                            egui::RichText::new(format!("{staged_count} staged"))
                                .color(Color32::from_gray(140)),
                        );
                    });
                    ui.horizontal(|ui| {
                        // Toggling amend on prefills the message from HEAD so it
                        // can be edited.
                        if ui
                            .checkbox(&mut tab.amend, "Amend")
                            .on_hover_text("Replace the previous commit")
                            .changed()
                            && tab.amend
                            && tab.commit_title.trim().is_empty()
                        {
                            if let Some(msg) = head_message(&tab.state) {
                                let (title, body) = split_message(&msg);
                                tab.commit_title = title;
                                tab.commit_body = body;
                            }
                        }
                        ui.checkbox(&mut tab.sign, "Sign")
                            .on_hover_text("Sign the commit (-S)");
                    });
                    if let Some((label, _)) = &tab.state.progress {
                        ui.label(format!("{label}…"));
                    }
                    ui.add_space(4.0);
                });

            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("COMMIT")
                    .size(11.0)
                    .color(Color32::from_gray(140))
                    .strong(),
            );
            ui.add_space(4.0);
            ui.add(
                egui::TextEdit::singleline(&mut tab.commit_title)
                    .hint_text("Summary (required)")
                    .desired_width(f32::INFINITY),
            );
            ui.add_space(4.0);
            // The description fills the remaining height above the action row.
            let body_size = ui.available_size();
            ui.add_sized(
                body_size,
                egui::TextEdit::multiline(&mut tab.commit_body).hint_text("Description (optional)"),
            );
        });

    // File lists fill the remaining area. Actions are collected and applied
    // after the borrow of `tab.state` ends.
    let mut stage_all = false;
    let mut unstage_all = false;
    let mut stage: Vec<PathBuf> = Vec::new();
    let mut unstage: Vec<PathBuf> = Vec::new();
    let mut discard: Option<(Vec<PathBuf>, Vec<PathBuf>)> = None;
    let mut stash: Option<bool> = None;
    let mut toggle_multi: Vec<PathBuf> = Vec::new();
    let mut select: Option<(ChangeSel, bool)> = None;

    // The two file sections render as one flat virtualized list (headers,
    // action rows, and entries all get one fixed-height row each), so huge
    // working trees only lay out the rows actually on screen.
    'lists: {
        let Some(status) = &tab.state.status else {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                ui.spinner();
                ui.label("Loading status…");
            });
            break 'lists;
        };
        if status.entries.is_empty() {
            ui.add_space(12.0);
            ui.weak("  Working tree clean.");
            break 'lists;
        }

        let unstaged: Vec<&StatusEntry> =
            status.entries.iter().filter(|e| e.has_unstaged()).collect();
        let staged: Vec<&StatusEntry> = status.entries.iter().filter(|e| e.is_staged()).collect();

        /// One virtual row of the changes list.
        enum Row {
            Header(&'static str, usize),
            Actions(bool),
            Entry(bool, usize),
        }
        let mut rows = Vec::with_capacity(unstaged.len() + staged.len() + 4);
        rows.push(Row::Header("Unstaged", unstaged.len()));
        if !unstaged.is_empty() {
            rows.push(Row::Actions(false));
        }
        rows.extend((0..unstaged.len()).map(|i| Row::Entry(false, i)));
        rows.push(Row::Header("Staged", staged.len()));
        if !staged.is_empty() {
            rows.push(Row::Actions(true));
        }
        rows.extend((0..staged.len()).map(|i| Row::Entry(true, i)));

        ui.add_space(4.0);
        ui.spacing_mut().item_spacing.y = 0.0;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, FILE_ROW_HEIGHT, rows.len(), |ui, range| {
                for i in range {
                    let (rect, _) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), FILE_ROW_HEIGHT),
                        egui::Sense::hover(),
                    );
                    match rows[i] {
                        Row::Header(name, n) => {
                            ui.painter_at(rect).text(
                                egui::pos2(rect.left() + 8.0, rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                format!("{name} ({n})"),
                                egui::FontId::proportional(12.0),
                                Color32::from_gray(170),
                            );
                        }
                        Row::Actions(staged_side) => {
                            let label = if staged_side {
                                "Unstage all"
                            } else {
                                "Stage all"
                            };
                            let btn = egui::Rect::from_min_size(
                                egui::pos2(rect.left() + 8.0, rect.top() + 1.0),
                                egui::vec2(90.0, FILE_ROW_HEIGHT - 3.0),
                            );
                            if ui
                                .put(
                                    btn,
                                    egui::Button::new(egui::RichText::new(label).size(11.0)),
                                )
                                .clicked()
                            {
                                if staged_side {
                                    unstage_all = true;
                                } else {
                                    stage_all = true;
                                }
                            }
                        }
                        Row::Entry(staged_side, k) => {
                            let e = if staged_side { staged[k] } else { unstaged[k] };
                            let kind = if staged_side { e.staged } else { e.unstaged };
                            let is_sel = matches!(&tab.selected_change,
                                Some(s) if s.path == e.path && s.staged == staged_side);
                            let hint = if staged_side {
                                "Double-click to unstage · right-click for more"
                            } else {
                                "Double-click to stage · right-click for more"
                            };
                            let checked = tab.multi.contains(&e.path);
                            let (toggled, resp) =
                                change_entry_row(ui, rect, e, kind, is_sel, checked, hint);
                            if toggled {
                                toggle_multi.push(e.path.clone());
                            }
                            if resp.double_clicked() {
                                if staged_side {
                                    unstage.push(e.path.clone());
                                } else {
                                    stage.push(e.path.clone());
                                }
                            } else if resp.clicked() {
                                select = Some((
                                    ChangeSel {
                                        path: e.path.clone(),
                                        staged: staged_side,
                                    },
                                    !staged_side && e.unstaged == ChangeKind::Untracked,
                                ));
                            }
                            resp.context_menu(|ui| {
                                let section = if staged_side { &staged } else { &unstaged };
                                let targets = menu_targets(e, section, &tab.multi);
                                let n = targets.len();
                                let verb = if staged_side { "Unstage" } else { "Stage" };
                                if ui.button(count_label(verb, n)).clicked() {
                                    let out = if staged_side {
                                        &mut unstage
                                    } else {
                                        &mut stage
                                    };
                                    out.extend(targets.iter().map(|x| x.path.clone()));
                                    ui.close();
                                }
                                if !staged_side
                                    && ui
                                        .button(format!("{}…", count_label("Discard", n)))
                                        .clicked()
                                {
                                    discard = Some(split_discard(&targets));
                                    ui.close();
                                }
                                changes_menu_common(
                                    ui,
                                    &mut stash,
                                    &mut stage_all,
                                    &mut unstage_all,
                                );
                            });
                        }
                    }
                }
            });
    }

    // Apply collected actions (now that `tab.state` is no longer borrowed).
    if stage_all {
        tab.handle.send(Command::StageAll);
    }
    if unstage_all {
        tab.handle.send(Command::UnstageAll);
    }
    if !stage.is_empty() {
        tab.handle.send(Command::Stage(stage));
    }
    if !unstage.is_empty() {
        tab.handle.send(Command::Unstage(unstage));
    }
    for p in toggle_multi {
        if !tab.multi.remove(&p) {
            tab.multi.insert(p);
        }
    }
    if let Some(include_untracked) = stash {
        tab.handle.send(Command::Stash { include_untracked });
    }
    if let Some((sel, untracked)) = select {
        tab.handle.send(Command::LoadDiff {
            path: sel.path.clone(),
            staged: sel.staged,
            untracked,
        });
        tab.selected_change = Some(sel);
    }
    if let Some(hi) = hunk_toggle {
        if let Some(dv) = &tab.state.diff {
            if let Some(patch) = gg_diff::single_hunk_patch(&dv.raw, hi) {
                let staged = dv.staged;
                let path = dv.path.clone();
                // Staged view → unstage the hunk (reverse-apply); unstaged view
                // → stage it (forward-apply).
                tab.handle.send(Command::ApplyHunk {
                    patch,
                    reverse: staged,
                });
                // Reload the same side so the pane drops the moved hunk. The
                // worker runs commands in order, so this sees the new index.
                tab.handle.send(Command::LoadDiff {
                    path,
                    staged,
                    untracked: false,
                });
            }
        }
    }
    if let Some(push) = commit_push {
        let message = build_commit_message(&tab.commit_title, &tab.commit_body);
        if push {
            tab.handle.send(Command::CommitAndPush { message });
        } else {
            tab.handle.send(Command::Commit {
                message,
                opts: CommitOpts {
                    amend: tab.amend,
                    sign: tab.sign,
                    ..Default::default()
                },
            });
        }
    }

    discard
}

/// Render one fixed-height changes row (checkbox, badge, elided path) inside
/// `rect`. Returns whether the bulk-select checkbox toggled and the label's
/// response (for click / double-click / context-menu handling).
fn change_entry_row(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    e: &StatusEntry,
    kind: ChangeKind,
    is_sel: bool,
    checked: bool,
    hint: &str,
) -> (bool, egui::Response) {
    let inner = egui::Rect::from_min_max(egui::pos2(rect.left() + 8.0, rect.top()), rect.max);
    let mut row = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(inner)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    row.spacing_mut().item_spacing.x = 6.0;
    row.spacing_mut().button_padding = egui::vec2(6.0, 1.0);
    let mut c = checked;
    let toggled = row
        .checkbox(&mut c, "")
        .on_hover_text("Select for bulk actions")
        .changed();
    let (badge, color) = change_badge(kind);
    row.colored_label(color, badge);
    let full = path_label(e);
    let body = egui::TextStyle::Body.resolve(row.style());
    let shown = elide_left(&row, &full, &body, label_budget(&row));
    let resp = row
        .selectable_label(is_sel, shown)
        .on_hover_text(format!("{full}\n\n{hint}"));
    (toggled, resp)
}

/// Width available for an elided file-name label in a changes row, leaving room
/// for the selectable's button padding so it never spills past the list column.
fn label_budget(ui: &egui::Ui) -> f32 {
    let sp = ui.spacing();
    (ui.available_width() - (sp.item_spacing.x + sp.button_padding.x * 2.0 + 8.0)).max(40.0)
}

/// The files a row's context-menu action targets: the whole multi-selection if
/// the right-clicked file is part of it, otherwise just that file.
fn menu_targets<'a>(
    clicked: &'a StatusEntry,
    section: &[&'a StatusEntry],
    multi: &HashSet<PathBuf>,
) -> Vec<&'a StatusEntry> {
    if multi.contains(&clicked.path) {
        section
            .iter()
            .copied()
            .filter(|x| multi.contains(&x.path))
            .collect()
    } else {
        vec![clicked]
    }
}

/// A menu label that pluralizes by count, e.g. `Stage` or `Stage 3 files`.
fn count_label(verb: &str, n: usize) -> String {
    if n > 1 {
        format!("{verb} {n} files")
    } else {
        verb.to_string()
    }
}

/// Split target entries into tracked paths (to restore) and untracked paths (to
/// delete) for a discard.
fn split_discard(targets: &[&StatusEntry]) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut tracked = Vec::new();
    let mut untracked = Vec::new();
    for x in targets {
        if x.unstaged == ChangeKind::Untracked {
            untracked.push(x.path.clone());
        } else {
            tracked.push(x.path.clone());
        }
    }
    (tracked, untracked)
}

/// The stash + bulk items shared by both changes context menus.
fn changes_menu_common(
    ui: &mut egui::Ui,
    stash: &mut Option<bool>,
    stage_all: &mut bool,
    unstage_all: &mut bool,
) {
    ui.separator();
    if ui.button("Stash all changes").clicked() {
        *stash = Some(false);
        ui.close();
    }
    if ui.button("Stash all (incl. untracked)").clicked() {
        *stash = Some(true);
        ui.close();
    }
    ui.separator();
    if ui.button("Stage all").clicked() {
        *stage_all = true;
        ui.close();
    }
    if ui.button("Unstage all").clicked() {
        *unstage_all = true;
        ui.close();
    }
}

/// Build a history commit's right-click context menu, recording the chosen
/// action in `action` (applied by the caller).
fn commit_context_menu(
    ui: &mut egui::Ui,
    commit: &CommitMeta,
    action: &mut Option<CommitMenuAction>,
) {
    let hex = commit.oid.to_hex();
    let oid = commit.oid;
    if ui
        .button(format!("Checkout {}", commit.oid.short(8)))
        .clicked()
    {
        *action = Some(CommitMenuAction::Checkout(hex.clone()));
        ui.close();
    }
    if ui.button("Create branch here…").clicked() {
        *action = Some(CommitMenuAction::NewBranchHere(hex.clone()));
        ui.close();
    }
    ui.separator();
    if ui.button("Cherry-pick onto current").clicked() {
        *action = Some(CommitMenuAction::CherryPick(oid));
        ui.close();
    }
    if ui.button("Revert commit").clicked() {
        *action = Some(CommitMenuAction::Revert(oid));
        ui.close();
    }
    if ui.button("Create tag here…").clicked() {
        *action = Some(CommitMenuAction::CreateTagHere(hex.clone()));
        ui.close();
    }
    ui.separator();
    if ui.button("Merge into current branch").clicked() {
        *action = Some(CommitMenuAction::Merge(hex.clone()));
        ui.close();
    }
    if ui.button("Rebase current onto this").clicked() {
        *action = Some(CommitMenuAction::Rebase(hex.clone()));
        ui.close();
    }
    ui.menu_button("Reset current branch to here", |ui| {
        if ui.button("Soft (keep index & working tree)").clicked() {
            *action = Some(CommitMenuAction::Reset(hex.clone(), ResetMode::Soft));
            ui.close();
        }
        if ui.button("Mixed (keep working tree)").clicked() {
            *action = Some(CommitMenuAction::Reset(hex.clone(), ResetMode::Mixed));
            ui.close();
        }
        if ui.button("Hard (discard all changes)").clicked() {
            *action = Some(CommitMenuAction::ResetHardConfirm(hex.clone()));
            ui.close();
        }
    });
    ui.separator();
    if ui.button("Copy SHA").clicked() {
        ui.ctx().copy_text(hex.clone());
        ui.close();
    }
}

/// First 8 chars of a commit hex, for compact display.
fn short_hex(s: &str) -> &str {
    &s[..s.len().min(8)]
}

/// Pick the remote for fetch/pull: the current branch's upstream remote, else
/// `origin`, else the first configured remote.
fn derive_remote(state: &AppState) -> Option<String> {
    if let Some((remote, _)) = state
        .status
        .as_ref()
        .and_then(|s| s.upstream.as_deref())
        .and_then(|up| up.split_once('/'))
    {
        return Some(remote.to_string());
    }
    if state.remotes.iter().any(|r| r.name == "origin") {
        return Some("origin".to_string());
    }
    state.remotes.first().map(|r| r.name.clone())
}

/// The toolbar stash menu: create a stash, and apply/pop/drop existing ones.
fn stash_menu(ui: &mut egui::Ui, tab: &RepoTab, stash_cmd: &mut Option<StashCmd>) {
    let n = tab.state.stashes.len();
    let label = if n > 0 {
        format!("Stash ({n})  {}", icon::CARET_DOWN)
    } else {
        format!("Stash  {}", icon::CARET_DOWN)
    };
    ui.menu_button(label, |ui| {
        ui.set_min_width(260.0);
        if ui.button("Stash all changes").clicked() {
            *stash_cmd = Some(StashCmd::Push(false));
            ui.close();
        }
        if ui.button("Stash all (incl. untracked)").clicked() {
            *stash_cmd = Some(StashCmd::Push(true));
            ui.close();
        }
        if tab.state.stashes.is_empty() {
            return;
        }
        ui.separator();
        ui.label(
            egui::RichText::new("STASHES")
                .size(10.0)
                .color(Color32::from_gray(130)),
        );
        egui::ScrollArea::vertical()
            .id_salt("stash-list")
            .max_height(260.0)
            .show(ui, |ui| {
                for s in &tab.state.stashes {
                    let idx = s.index;
                    let body = egui::TextStyle::Body.resolve(ui.style());
                    let shown = elide_left(ui, &s.message, &body, 210.0);
                    ui.menu_button(format!("stash@{{{idx}}}: {shown}"), |ui| {
                        if ui.button("Apply").clicked() {
                            *stash_cmd = Some(StashCmd::Apply(idx));
                            ui.close();
                        }
                        if ui.button("Pop (apply & drop)").clicked() {
                            *stash_cmd = Some(StashCmd::Pop(idx));
                            ui.close();
                        }
                        if ui.button("Drop").clicked() {
                            *stash_cmd = Some(StashCmd::Drop(idx));
                            ui.close();
                        }
                    });
                }
            });
    });
}

fn build_commit_message(title: &str, body: &str) -> String {
    let title = title.trim();
    let body = body.trim();
    if body.is_empty() {
        title.to_string()
    } else {
        format!("{title}\n\n{body}")
    }
}

/// The full message of the current HEAD commit, if it is in the loaded history
/// (used to prefill the commit box when amending).
fn head_message(state: &AppState) -> Option<String> {
    let head = state
        .refs
        .iter()
        .find(|r| r.is_head && r.kind == RefKind::LocalBranch)?;
    let view = state.history.as_ref()?;
    view.commits
        .iter()
        .find(|c| c.oid == head.target)
        .map(|c| c.message.clone())
}

/// Split a commit message into its subject (first line) and body (the rest).
fn split_message(msg: &str) -> (String, String) {
    let mut parts = msg.trim_end().splitn(2, '\n');
    let subject = parts.next().unwrap_or("").trim().to_string();
    let body = parts
        .next()
        .unwrap_or("")
        .trim_start_matches('\n')
        .trim_start()
        .to_string();
    (subject, body)
}

/// Open the OS terminal at `path` (best-effort, per platform).
fn open_in_terminal(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg("-a")
            .arg("Terminal")
            .arg(path)
            .spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "cmd"])
            .current_dir(path)
            .spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        for term in ["x-terminal-emulator", "gnome-terminal", "konsole", "xterm"] {
            if std::process::Command::new(term)
                .current_dir(path)
                .spawn()
                .is_ok()
            {
                break;
            }
        }
    }
}

/// Inject a synthetic key press into egui, translating a native menu pick
/// back into the in-app shortcut it represents.
#[cfg(target_os = "macos")]
fn inject_key(ctx: &egui::Context, key: egui::Key, modifiers: egui::Modifiers) {
    ctx.input_mut(|i| {
        i.events.push(egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        });
    });
}

/// The `gg-askpass` helper binary shipped next to the app executable, if
/// present (it answers git/ssh credential prompts from env vars we set).
fn askpass_helper() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let name = if cfg!(windows) {
        "gg-askpass.exe"
    } else {
        "gg-askpass"
    };
    let path = exe.parent()?.join(name);
    path.is_file().then_some(path)
}

/// Does this git error indicate missing/rejected credentials (rather than a
/// network or repository problem)?
fn is_auth_error(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("could not read username")
        || e.contains("could not read password")
        || e.contains("authentication failed")
        || e.contains("terminal prompts disabled")
        || e.contains("no credential available") // gg-askpass's marker
        || e.contains("permission denied (publickey")
}

/// Session credentials from the prompt fields. The one secret answers either
/// a password or an SSH-passphrase prompt; git only ever asks for the kind it
/// needs, so the classification in `gg-credentials` picks the right one.
fn session_credentials(username: &str, secret: &str) -> Credentials {
    Credentials {
        username: (!username.trim().is_empty()).then(|| username.trim().to_string()),
        password: (!secret.is_empty()).then(|| secret.to_string()),
        passphrase: (!secret.is_empty()).then(|| secret.to_string()),
    }
}

/// Derive the checkout folder name from a clone URL (`…/name.git` → `name`).
/// `None` until the URL looks plausibly like one (has a path separator and a
/// non-empty last segment).
fn repo_name_from_url(url: &str) -> Option<String> {
    let t = url.trim().trim_end_matches('/');
    if !t.contains(['/', ':']) {
        return None;
    }
    let last = t.rsplit(['/', ':']).next()?;
    let name = last.strip_suffix(".git").unwrap_or(last).trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Run `git clone` on a background thread; the cloned path (or git's stderr)
/// arrives on the returned channel, and `ctx` is woken when it does.
/// Credential prompts route through `gg-askpass`: with `creds` they are
/// answered, without they fail fast with a classifiable error that makes the
/// clone dialog show its username/password fields.
fn spawn_clone(
    url: String,
    target: PathBuf,
    creds: Option<Credentials>,
    ctx: Option<egui::Context>,
) -> std::sync::mpsc::Receiver<Result<PathBuf, String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut cmd = std::process::Command::new("git");
        cmd.arg("clone")
            .arg("--")
            .arg(&url)
            .arg(&target)
            // Fail with a readable error instead of hanging on a credential
            // prompt no terminal will ever show.
            .env("GIT_TERMINAL_PROMPT", "0");
        if let Some(askpass) = askpass_helper() {
            cmd.env("GIT_ASKPASS", &askpass);
            cmd.env("SSH_ASKPASS", &askpass);
            cmd.env("SSH_ASKPASS_REQUIRE", "force");
        }
        for (k, v) in creds.map(|c| c.to_env()).unwrap_or_default() {
            cmd.env(k, v);
        }
        let out = cmd.output();
        let res = match out {
            Ok(o) if o.status.success() => Ok(target),
            Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
            Err(e) => Err(format!("Failed to run git: {e}")),
        };
        let _ = tx.send(res);
        if let Some(ctx) = ctx {
            ctx.request_repaint();
        }
    });
    rx
}

/// How many directory levels below the chosen root a folder scan descends.
const SCAN_DEPTH: usize = 3;

/// Collect git repositories under `root`: depth-limited, skips hidden and
/// heavy build/dependency folders, and does not descend into a repository
/// once found (nested repos inside a repo are the outer repo's business).
fn scan_for_repos(root: &std::path::Path, depth: usize, out: &mut Vec<PathBuf>) {
    if root.join(".git").exists() {
        out.push(root.to_path_buf());
        return;
    }
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            !name.starts_with('.') && !matches!(name, "node_modules" | "target" | "vendor")
        })
        .collect();
    dirs.sort();
    for dir in dirs {
        scan_for_repos(&dir, depth - 1, out);
    }
}

/// Create `target` (which must not already hold anything) and `git init` it.
fn init_repo(target: &std::path::Path) -> Result<(), String> {
    let occupied = target.exists()
        && target
            .read_dir()
            .map(|mut d| d.next().is_some())
            .unwrap_or(true);
    if occupied {
        return Err(format!(
            "{} already exists and is not empty.",
            target.display()
        ));
    }
    std::fs::create_dir_all(target).map_err(|e| format!("Could not create the folder: {e}"))?;
    let out = std::process::Command::new("git")
        .arg("init")
        .arg(target)
        .output()
        .map_err(|e| format!("Failed to run git: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// What the OS calls its file manager, for the "Open in" menu label.
#[cfg(target_os = "macos")]
const FILE_MANAGER_NAME: &str = "Finder";
#[cfg(target_os = "windows")]
const FILE_MANAGER_NAME: &str = "Explorer";
#[cfg(all(unix, not(target_os = "macos")))]
const FILE_MANAGER_NAME: &str = "File manager";

/// Reveal the repository folder in the OS file manager.
fn open_in_file_manager(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(path).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

/// Open the repository folder in the first GUI editor whose CLI launcher is
/// on PATH (best effort; does nothing if none are installed).
fn open_in_editor(path: &std::path::Path) {
    for editor in ["code", "cursor", "zed", "subl"] {
        if std::process::Command::new(editor).arg(path).spawn().is_ok() {
            break;
        }
    }
}

/// The top-right workspace selector: a menu listing the workspace tree
/// (indented, active marked) plus a "Manage workspaces…" entry. The chosen
/// workspace id is written to `select`.
fn workspace_dropdown(
    ui: &mut egui::Ui,
    store: &WorkspaceStore,
    select: &mut Option<u64>,
    settings: &mut bool,
) {
    let name = store
        .active_node()
        .map(|w| w.name.clone())
        .unwrap_or_else(|| "Workspace".to_string());
    ui.menu_button(
        format!("{}  {name}  {}", icon::FOLDER, icon::CARET_DOWN),
        |ui| {
            ui.set_min_width(220.0);
            ws_menu_items(ui, &store.roots, 0, store.active, select);
            ui.separator();
            if ui
                .button(format!("{}  Manage workspaces…", icon::SETTINGS))
                .clicked()
            {
                *settings = true;
                ui.close();
            }
        },
    );
}

fn ws_menu_items(
    ui: &mut egui::Ui,
    nodes: &[WsNode],
    depth: usize,
    active: u64,
    select: &mut Option<u64>,
) {
    for n in nodes {
        let indent = "    ".repeat(depth);
        let marker = if n.id == active { icon::DOT } else { " " };
        if ui.button(format!("{indent}{marker} {}", n.name)).clicked() {
            *select = Some(n.id);
            ui.close();
        }
        ws_menu_items(ui, &n.children, depth + 1, active, select);
    }
}

/// Mutations collected while rendering the manage-workspaces tree.
#[derive(Default)]
struct WsActions {
    select: Option<u64>,
    start_rename: Option<u64>,
    commit_rename: bool,
    cancel_rename: bool,
    add_child: Option<u64>,
    add_root: bool,
    delete: Option<u64>,
    /// (node to move, new parent (`None` = top level), sibling index).
    reparent: Option<(u64, Option<u64>, usize)>,
    toggle_expand: Option<u64>,
}

/// Recursively render the workspace tree with inline rename, per-node actions,
/// and drag-and-drop reparenting (drop a node onto another to nest it).
/// Height of one row in the manage-workspaces tree.
const WS_ROW_H: f32 = 30.0;

/// Render one level of the manage-workspaces tree. `blocked` marks rows
/// inside the subtree currently being dragged, which must reject drops onto
/// themselves.
#[allow(clippy::too_many_arguments)]
fn ws_tree_ui(
    ui: &mut egui::Ui,
    nodes: &[WsNode],
    parent: Option<u64>,
    depth: usize,
    active: u64,
    blocked: bool,
    rename: &mut Option<(u64, String)>,
    act: &mut WsActions,
) {
    let dragging = egui::DragAndDrop::payload::<u64>(ui.ctx()).map(|p| *p);
    for (index, n) in nodes.iter().enumerate() {
        let (rect, row) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), WS_ROW_H),
            egui::Sense::click_and_drag(),
        );
        let accent = ui.visuals().selection.bg_fill;
        // Full-width row background.
        let bg = if n.id == active {
            accent
        } else if row.hovered() {
            Color32::from_white_alpha(12)
        } else {
            Color32::from_white_alpha(4)
        };
        ui.painter().rect_filled(
            rect.shrink2(egui::vec2(0.0, 1.0)),
            egui::CornerRadius::same(4),
            bg,
        );

        let indent = depth as f32 * 18.0;
        let cy = rect.center().y;
        let flat_hover = Color32::from_white_alpha(18);

        // Fixed first column: a flat caret, centered both ways, only when the
        // node has children.
        let caret_center = egui::pos2(rect.left() + 8.0 + indent + 9.0, cy);
        if !n.children.is_empty() {
            let caret_rect = egui::Rect::from_center_size(caret_center, egui::vec2(18.0, 18.0));
            let cresp = ui.interact(
                caret_rect,
                ui.id().with(("ws-caret", n.id)),
                egui::Sense::click(),
            );
            if cresp.hovered() {
                ui.painter()
                    .rect_filled(caret_rect, egui::CornerRadius::same(4), flat_hover);
            }
            ui.painter().text(
                caret_center,
                egui::Align2::CENTER_CENTER,
                if n.expanded {
                    icon::CARET_DOWN
                } else {
                    icon::CARET_RIGHT
                },
                egui::FontId::proportional(11.0),
                Color32::from_gray(200),
            );
            if cresp.clicked() {
                act.toggle_expand = Some(n.id);
            }
        }

        // Right-aligned flat action buttons (delete, add child, rename).
        let mut bx = rect.right() - 6.0;
        let mut flat_button = |ui: &mut egui::Ui, glyph: &str, tip: &str| -> bool {
            let brect =
                egui::Rect::from_center_size(egui::pos2(bx - 10.0, cy), egui::vec2(20.0, 20.0));
            bx -= 24.0;
            let resp = ui
                .interact(
                    brect,
                    ui.id().with(("ws-act", n.id, tip)),
                    egui::Sense::click(),
                )
                .on_hover_text(tip);
            if resp.hovered() {
                ui.painter()
                    .rect_filled(brect, egui::CornerRadius::same(4), flat_hover);
            }
            ui.painter().text(
                brect.center(),
                egui::Align2::CENTER_CENTER,
                glyph,
                egui::FontId::proportional(12.0),
                Color32::from_gray(200),
            );
            resp.clicked()
        };
        if flat_button(ui, icon::DELETE, "Delete") {
            act.delete = Some(n.id);
        }
        if flat_button(ui, icon::ADD, "Add sub-workspace") {
            act.add_child = Some(n.id);
        }
        if flat_button(ui, icon::RENAME, "Rename") {
            act.start_rename = Some(n.id);
        }

        // Name (or the rename editor) between the caret column and buttons.
        let name_left = rect.left() + 8.0 + indent + 22.0;
        let editing = matches!(rename, Some((rid, _)) if *rid == n.id);
        if editing {
            if let Some((_, buf)) = rename.as_mut() {
                let edit_rect = egui::Rect::from_min_max(
                    egui::pos2(name_left, rect.top() + 4.0),
                    egui::pos2(bx - 8.0, rect.bottom() - 4.0),
                );
                let resp = ui.put(edit_rect, egui::TextEdit::singleline(buf));
                if !resp.has_focus() {
                    resp.request_focus();
                }
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    act.commit_rename = true;
                } else if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    act.cancel_rename = true;
                }
            }
        } else {
            let name_galley = ui.fonts(|f| {
                f.layout_no_wrap(
                    n.name.clone(),
                    egui::FontId::proportional(13.0),
                    ui.visuals().text_color(),
                )
            });
            let name_pos = egui::pos2(name_left, cy - name_galley.size().y / 2.0);
            let name_w = name_galley.size().x;
            ui.painter()
                .with_clip_rect(egui::Rect::from_min_max(
                    egui::pos2(name_left, rect.top()),
                    egui::pos2(bx - 8.0, rect.bottom()),
                ))
                .galley(name_pos, name_galley, ui.visuals().text_color());
            if !n.repos.is_empty() {
                ui.painter().text(
                    egui::pos2(name_left + name_w + 8.0, cy),
                    egui::Align2::LEFT_CENTER,
                    format!("({} tabs)", n.repos.len()),
                    egui::FontId::proportional(11.0),
                    Color32::from_gray(130),
                );
            }
        }

        // Row interactions: click activates, dragging carries the id.
        if row.clicked() {
            act.select = Some(n.id);
        }
        if !editing {
            row.dnd_set_drag_payload(n.id);
        }
        let row = row.on_hover_text(
            "Click to activate · drag to reorder or nest · drop between rows to reorder",
        );

        // Drop target: the row's top/bottom quarters insert before/after it,
        // the middle nests inside it. Rows of the dragged subtree are inert.
        let row_blocked = blocked || dragging == Some(n.id);
        if !row_blocked {
            if let Some(src) = row.dnd_hover_payload::<u64>() {
                if *src != n.id {
                    let y = ui.ctx().pointer_hover_pos().map(|p| p.y).unwrap_or(cy);
                    let frac = ((y - rect.top()) / rect.height()).clamp(0.0, 1.0);
                    let stroke = egui::Stroke::new(2.0_f32, Color32::from_rgb(0x6f, 0xa8, 0xff));
                    let target = if frac < 0.25 {
                        ui.painter()
                            .line_segment([rect.left_top(), rect.right_top()], stroke);
                        (parent, index)
                    } else if frac > 0.75 {
                        ui.painter()
                            .line_segment([rect.left_bottom(), rect.right_bottom()], stroke);
                        (parent, index + 1)
                    } else {
                        ui.painter().rect_stroke(
                            rect.shrink(1.0),
                            egui::CornerRadius::same(4),
                            stroke,
                            egui::StrokeKind::Inside,
                        );
                        (Some(n.id), usize::MAX)
                    };
                    if let Some(src) = row.dnd_release_payload::<u64>() {
                        act.reparent = Some((*src, target.0, target.1));
                    }
                }
            }
        }

        if n.expanded && !n.children.is_empty() {
            ws_tree_ui(
                ui,
                &n.children,
                Some(n.id),
                depth + 1,
                active,
                row_blocked,
                rename,
                act,
            );
        }
    }
}

fn path_label(e: &StatusEntry) -> String {
    match &e.orig_path {
        Some(orig) => format!("{} {} {}", orig.display(), icon::ARROW, e.path.display()),
        None => e.path.display().to_string(),
    }
}

fn change_badge(kind: ChangeKind) -> (&'static str, Color32) {
    use ChangeKind::*;
    let green = Color32::from_rgb(0x4c, 0xa6, 0x6b);
    let amber = Color32::from_rgb(0xc9, 0x8a, 0x3a);
    let red = Color32::from_rgb(0xcc, 0x5b, 0x5b);
    let blue = Color32::from_rgb(0x4f, 0x83, 0xcc);
    let gray = Color32::from_gray(130);
    match kind {
        Added => ("A", green),
        Modified => ("M", amber),
        Deleted => ("D", red),
        Renamed => ("R", blue),
        Copied => ("C", blue),
        TypeChanged => ("T", amber),
        Conflicted => ("!", red),
        Untracked => ("?", gray),
        Ignored => ("·", gray),
        Unmodified => (" ", gray),
    }
}

/// A flat, cheap-to-build row model of a file diff: one entry per hunk header
/// and per line, so the viewer can virtualize over uniform-height rows. Built
/// once per loaded diff and cached on the tab; per-frame work is then
/// proportional to the rows on screen, not the diff size.
struct DiffDoc {
    rows: Vec<DiffRowRef>,
    /// Longest line in characters, for horizontal-scroll content sizing.
    max_chars: usize,
}

/// One virtual diff row: a hunk header, or line `1` of hunk `0`.
enum DiffRowRef {
    Header(usize),
    Line(usize, usize),
}

fn build_diff_doc(diff: &FileDiff) -> DiffDoc {
    let mut rows = Vec::new();
    let mut max_chars = 0;
    for (hi, hunk) in diff.hunks.iter().enumerate() {
        rows.push(DiffRowRef::Header(hi));
        for (li, line) in hunk.lines.iter().enumerate() {
            max_chars = max_chars.max(line.text.chars().count());
            rows.push(DiffRowRef::Line(hi, li));
        }
    }
    DiffDoc { rows, max_chars }
}

/// Fetch the cached row model for a diff, rebuilding it when `key` changed
/// (i.e. a different diff was loaded).
fn ensure_diff_doc<'c, K: PartialEq + Copy>(
    cache: &'c mut Option<(K, DiffDoc)>,
    key: K,
    diff: &FileDiff,
) -> &'c DiffDoc {
    let stale = !matches!(cache, Some((k, _)) if *k == key);
    if stale {
        *cache = Some((key, build_diff_doc(diff)));
    }
    &cache.as_ref().expect("just ensured").1
}

/// Render the diff preview for the selected file. Returns the index of a hunk
/// whose stage / unstage button was clicked this frame, if any.
fn draw_diff_preview(
    ui: &mut egui::Ui,
    diff: Option<&DiffView>,
    cache: &mut Option<(u64, DiffDoc)>,
    gen: u64,
) -> Option<usize> {
    let Some(dv) = diff else {
        ui.add_space(12.0);
        ui.vertical_centered(|ui| {
            ui.weak("Select a file to preview its changes.");
        });
        return None;
    };

    // Header: file path (left-elided so the file name stays visible) + side tag.
    ui.add_space(6.0);
    let path_str = dv.path.display().to_string();
    let side = if dv.staged { "staged" } else { "unstaged" };
    ui.horizontal(|ui| {
        ui.add_space(8.0);
        let body = egui::TextStyle::Body.resolve(ui.style());
        let avail = (ui.available_width() - 90.0).max(60.0);
        let shown = elide_left(ui, &path_str, &body, avail);
        ui.add(egui::Label::new(egui::RichText::new(shown).strong()).truncate())
            .on_hover_text(&path_str);
        ui.label(
            egui::RichText::new(side)
                .small()
                .color(Color32::from_gray(140)),
        );
    });
    ui.separator();

    // Per-hunk staging needs git's raw patch; untracked/synthesized diffs lack
    // one, so only offer the buttons when raw text is available.
    let stage_buttons = (!dv.raw.is_empty()).then_some(dv.staged);
    let doc = ensure_diff_doc(cache, gen, &dv.diff);
    draw_file_diff(ui, &dv.diff, doc, stage_buttons)
}

/// Render a file's diff body: a binary/empty notice, or a virtualized
/// two-axis-scrolling list of uniform-height rows (hunk headers + lines). Only
/// visible rows are laid out each frame; add/delete backgrounds span the full
/// scrollable width. When `stage_buttons` is `Some(staged)`, each hunk header
/// carries a stage/unstage action and the clicked hunk's index is returned.
fn draw_file_diff(
    ui: &mut egui::Ui,
    diff: &FileDiff,
    doc: &DiffDoc,
    stage_buttons: Option<bool>,
) -> Option<usize> {
    if diff.is_binary {
        ui.add_space(8.0);
        ui.weak("  Binary file: no preview.");
        return None;
    }
    if diff.hunks.is_empty() {
        ui.add_space(8.0);
        ui.weak("  No textual changes.");
        return None;
    }

    let gutter_w = 84.0;
    let mono = egui::FontId::monospace(12.0);
    let text_color = Color32::from_gray(210);
    // Monospace: content width follows straight from the longest line's
    // character count (the `+ 3` covers the sign column and slack).
    let char_w = ui.fonts(|f| f.glyph_width(&mono, '0'));
    let content_w = gutter_w + (doc.max_chars as f32 + 3.0) * char_w + 12.0;
    let viewport_w = ui.available_width();

    let mut toggle = None;
    ui.spacing_mut().item_spacing.y = 0.0;
    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show_rows(ui, DIFF_ROW_HEIGHT, doc.rows.len(), |ui, range| {
            let row_w = content_w.max(viewport_w - 8.0);
            for i in range {
                let (rect, _) = ui
                    .allocate_exact_size(egui::vec2(row_w, DIFF_ROW_HEIGHT), egui::Sense::hover());
                match doc.rows[i] {
                    DiffRowRef::Header(hi) => {
                        let hunk = &diff.hunks[hi];
                        let p = ui.painter_at(rect);
                        p.rect_filled(rect, egui::CornerRadius::ZERO, Color32::from_white_alpha(5));
                        let mut text_x = rect.left() + 8.0;
                        if let Some(staged) = stage_buttons {
                            let (label, hover) = if staged {
                                ("Unstage hunk", "Move this hunk out of the index")
                            } else {
                                ("Stage hunk", "Move this hunk into the index")
                            };
                            let btn = egui::Rect::from_min_size(
                                egui::pos2(rect.left() + 4.0, rect.top()),
                                egui::vec2(92.0, DIFF_ROW_HEIGHT),
                            );
                            if ui
                                .put(
                                    btn,
                                    egui::Button::new(egui::RichText::new(label).size(10.0)),
                                )
                                .on_hover_text(hover)
                                .clicked()
                            {
                                toggle = Some(hi);
                            }
                            text_x = btn.right() + 10.0;
                        }
                        p.text(
                            egui::pos2(text_x, rect.center().y),
                            egui::Align2::LEFT_CENTER,
                            format!(
                                "@@ -{},{} +{},{} @@ {}",
                                hunk.old_start,
                                hunk.old_lines,
                                hunk.new_start,
                                hunk.new_lines,
                                hunk.header
                            ),
                            mono.clone(),
                            Color32::from_gray(150),
                        );
                    }
                    DiffRowRef::Line(hi, li) => {
                        let line = &diff.hunks[hi].lines[li];
                        let (bg, intra_bg, sign) = match line.kind {
                            LineKind::Addition => (
                                Some(Color32::from_rgb(0x12, 0x2e, 0x1c)),
                                Color32::from_rgb(0x1f, 0x5c, 0x33),
                                '+',
                            ),
                            LineKind::Deletion => (
                                Some(Color32::from_rgb(0x33, 0x18, 0x1d)),
                                Color32::from_rgb(0x6b, 0x24, 0x2d),
                                '-',
                            ),
                            LineKind::Context => (None, Color32::TRANSPARENT, ' '),
                        };
                        let p = ui.painter_at(rect);
                        if let Some(bg) = bg {
                            p.rect_filled(rect, egui::CornerRadius::ZERO, bg);
                        }
                        let old = line.old_lineno.map(|n| n.to_string()).unwrap_or_default();
                        let new = line.new_lineno.map(|n| n.to_string()).unwrap_or_default();
                        p.text(
                            egui::pos2(rect.left() + 6.0, rect.center().y),
                            egui::Align2::LEFT_CENTER,
                            format!("{old:>4} {new:>4}"),
                            mono.clone(),
                            Color32::from_gray(90),
                        );
                        let mut job = egui::text::LayoutJob::default();
                        job.wrap.max_width = f32::INFINITY;
                        let base = egui::TextFormat {
                            font_id: mono.clone(),
                            color: text_color,
                            ..Default::default()
                        };
                        job.append(&format!("{sign} "), 0.0, base.clone());
                        if let Some(span) = line.intra.first() {
                            let (a, b, c) = split3(&line.text, span.start, span.end);
                            job.append(a, 0.0, base.clone());
                            job.append(
                                b,
                                0.0,
                                egui::TextFormat {
                                    font_id: mono.clone(),
                                    color: Color32::WHITE,
                                    background: intra_bg,
                                    ..Default::default()
                                },
                            );
                            job.append(c, 0.0, base.clone());
                        } else {
                            job.append(&line.text, 0.0, base.clone());
                        }
                        // egui memoizes layout jobs across frames, so visible
                        // rows re-render from the galley cache.
                        let galley = ui.fonts(|f| f.layout_job(job));
                        let ty = rect.top() + (DIFF_ROW_HEIGHT - galley.size().y) / 2.0;
                        p.galley(egui::pos2(rect.left() + gutter_w, ty), galley, text_color);
                    }
                }
            }
        });

    toggle
}

/// The right-hand pane in History: the selected commit's changed-files list
/// (top, virtualized) and the selected file's diff (below). A clicked file
/// index is written into `file_click`.
fn commit_detail_pane(
    ui: &mut egui::Ui,
    state: &AppState,
    commit_doc: &mut Option<((u64, usize), DiffDoc)>,
    commit_gen: u64,
    oid: Oid,
    selected_file: Option<usize>,
    file_click: &mut Option<usize>,
) {
    let Some(cd) = state.commit_diff.as_ref().filter(|c| c.oid == oid) else {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui.spinner();
            ui.label("Loading commit…");
        });
        return;
    };
    let files = &cd.files;
    if files.is_empty() {
        ui.add_space(8.0);
        ui.weak("  No file changes in this commit.");
        return;
    }
    let idx = selected_file.unwrap_or(0).min(files.len() - 1);

    // The changed files as one horizontally scrollable tab strip, so the diff
    // below keeps the full remaining height.
    ui.add_space(4.0);
    egui::ScrollArea::horizontal()
        .id_salt("commit-file-tabs")
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add_space(4.0);
                ui.spacing_mut().item_spacing.x = 2.0;
                ui.label(
                    egui::RichText::new(format!("{} files", files.len()))
                        .small()
                        .color(Color32::from_gray(140)),
                );
                ui.add_space(4.0);
                for (i, f) in files.iter().enumerate() {
                    let (badge, color) = file_change_badge(f.change);
                    let name = f
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| f.path.display().to_string());
                    let font = egui::TextStyle::Body.resolve(ui.style());
                    let mut job = egui::text::LayoutJob::default();
                    job.append(
                        badge,
                        0.0,
                        egui::TextFormat {
                            font_id: font.clone(),
                            color,
                            ..Default::default()
                        },
                    );
                    job.append(
                        &name,
                        6.0,
                        egui::TextFormat {
                            font_id: font,
                            color: ui.visuals().text_color(),
                            ..Default::default()
                        },
                    );
                    if ui
                        .selectable_label(i == idx, job)
                        .on_hover_text(format!(
                            "{}\n+{}  -{}",
                            f.path.display(),
                            f.additions(),
                            f.deletions()
                        ))
                        .clicked()
                    {
                        *file_click = Some(i);
                    }
                }
            });
        });
    ui.separator();

    // Full-width, full-height diff of the selected file.
    let f = &files[idx];
    let doc = ensure_diff_doc(commit_doc, (commit_gen, idx), f);
    draw_file_diff(ui, f, doc, None);
}

/// Badge letter + color for a commit file change (mirrors [`change_badge`] for
/// the [`FileChange`] enum produced by the diff parser).
fn file_change_badge(change: FileChange) -> (&'static str, Color32) {
    let green = Color32::from_rgb(0x4c, 0xa6, 0x6b);
    let amber = Color32::from_rgb(0xc9, 0x8a, 0x3a);
    let red = Color32::from_rgb(0xcc, 0x5b, 0x5b);
    let blue = Color32::from_rgb(0x4f, 0x83, 0xcc);
    match change {
        FileChange::Added => ("A", green),
        FileChange::Modified => ("M", amber),
        FileChange::Deleted => ("D", red),
        FileChange::Renamed => ("R", blue),
        FileChange::Copied => ("C", blue),
        FileChange::TypeChanged => ("T", amber),
    }
}

/// The toolbar branch menu: shows the current branch and lets the user check
/// out / create / rename / delete branches. Chosen actions are written into
/// `branch_cmd` / `open_dialog` and applied by the caller once the panel's
/// borrow of `self` ends.
fn branch_menu(
    ui: &mut egui::Ui,
    sel: usize,
    tab: &RepoTab,
    branch_cmd: &mut Option<(usize, BranchCmd)>,
    open_dialog: &mut Option<BranchDialog>,
) {
    let current = tab.state.status.as_ref().and_then(|s| s.branch.clone());
    let label = match &current {
        Some(b) => format!("{b}  {}", icon::CARET_DOWN),
        None => format!("detached  {}", icon::CARET_DOWN),
    };
    ui.menu_button(label, |ui| {
        ui.set_min_width(240.0);
        if ui.button(format!("{}  New branch…", icon::ADD)).clicked() {
            *open_dialog = Some(BranchDialog::New {
                repo: sel,
                name: String::new(),
                start: None,
                checkout: true,
            });
            ui.close();
        }

        let locals: Vec<(String, bool)> = tab
            .state
            .refs
            .iter()
            .filter(|r| r.kind == RefKind::LocalBranch)
            .map(|r| (r.name.short().to_string(), r.is_head))
            .collect();
        if !locals.is_empty() {
            ui.separator();
            ui.label(
                egui::RichText::new("LOCAL BRANCHES")
                    .size(10.0)
                    .color(Color32::from_gray(130)),
            );
            egui::ScrollArea::vertical()
                .id_salt("local-branches")
                .max_height(260.0)
                .show(ui, |ui| {
                    for (name, is_head) in &locals {
                        ui.horizontal(|ui| {
                            let marker = if *is_head { icon::DOT } else { " " };
                            if ui
                                .add(egui::Button::new(format!("{marker} {name}")).frame(false))
                                .clicked()
                                && !is_head
                            {
                                *branch_cmd = Some((sel, BranchCmd::Checkout(name.clone())));
                                ui.close();
                            }
                            if ui
                                .small_button(icon::RENAME)
                                .on_hover_text("Rename")
                                .clicked()
                            {
                                *open_dialog = Some(BranchDialog::Rename {
                                    repo: sel,
                                    old: name.clone(),
                                    name: name.clone(),
                                });
                                ui.close();
                            }
                            if ui
                                .add_enabled(!is_head, egui::Button::new(icon::DELETE).small())
                                .on_hover_text("Delete")
                                .clicked()
                            {
                                *open_dialog = Some(BranchDialog::Delete {
                                    repo: sel,
                                    name: name.clone(),
                                    force: false,
                                });
                                ui.close();
                            }
                        });
                    }
                });
        }

        let remotes: Vec<String> = tab
            .state
            .refs
            .iter()
            .filter(|r| r.kind == RefKind::RemoteBranch)
            .map(|r| r.name.short().to_string())
            .collect();
        if !remotes.is_empty() {
            ui.separator();
            ui.label(
                egui::RichText::new("REMOTE BRANCHES")
                    .size(10.0)
                    .color(Color32::from_gray(130)),
            );
            egui::ScrollArea::vertical()
                .id_salt("remote-branches")
                .max_height(200.0)
                .show(ui, |ui| {
                    for full in &remotes {
                        if ui
                            .add(
                                egui::Button::new(format!("{}  {full}", icon::REMOTE)).frame(false),
                            )
                            .on_hover_text("Check out as a local tracking branch")
                            .clicked()
                        {
                            // Strip the remote name: `origin/feature` → `feature`.
                            let local = full
                                .split_once('/')
                                .map(|(_, rest)| rest)
                                .unwrap_or(full.as_str())
                                .to_string();
                            *branch_cmd = Some((
                                sel,
                                BranchCmd::CheckoutTracking {
                                    local,
                                    start: full.clone(),
                                },
                            ));
                            ui.close();
                        }
                    }
                });
        }

        let tags: Vec<String> = tab
            .state
            .refs
            .iter()
            .filter(|r| r.kind == RefKind::Tag)
            .map(|r| r.name.short().to_string())
            .collect();
        if !tags.is_empty() {
            ui.separator();
            ui.label(
                egui::RichText::new("TAGS")
                    .size(10.0)
                    .color(Color32::from_gray(130)),
            );
            egui::ScrollArea::vertical()
                .id_salt("tags")
                .max_height(180.0)
                .show(ui, |ui| {
                    for t in &tags {
                        ui.horizontal(|ui| {
                            ui.label(format!("{}  {t}", icon::TAG));
                            if ui
                                .small_button(icon::DELETE)
                                .on_hover_text("Delete tag")
                                .clicked()
                            {
                                *branch_cmd = Some((sel, BranchCmd::DeleteTag(t.clone())));
                                ui.close();
                            }
                        });
                    }
                });
        }

        ui.separator();
        if ui.button("Manage remotes…").clicked() {
            *branch_cmd = Some((sel, BranchCmd::ManageRemotes));
            ui.close();
        }
    });
}

/// One fixed-size repository tab. Returns `true` when the tab body was clicked
/// (select). The close control only appears while the tab is hovered or
/// active, and needs its own small hit target, so stray clicks on a tab edge
/// can't close it; middle-click anywhere on the tab also closes.
fn draw_repo_tab(
    ui: &mut egui::Ui,
    name: &str,
    path: &Path,
    active: bool,
    loading: bool,
    tab_close: &mut Option<usize>,
    index: usize,
) -> bool {
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(TAB_WIDTH, TAB_HEIGHT), egui::Sense::click());
    let hovered = resp.hovered() || ui.rect_contains_pointer(rect);
    let fill = if active {
        Color32::from_rgb(0x2c, 0x44, 0x66)
    } else if hovered {
        Color32::from_white_alpha(14)
    } else {
        Color32::from_white_alpha(5)
    };
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(6), fill);

    let mut text_left = rect.left() + 10.0;
    if loading {
        ui.put(
            egui::Rect::from_center_size(
                egui::pos2(rect.left() + 14.0, rect.center().y),
                egui::vec2(12.0, 12.0),
            ),
            egui::Spinner::new().size(12.0),
        );
        text_left = rect.left() + 24.0;
    }
    let show_close = active || hovered;
    let text_right = if show_close {
        rect.right() - 26.0
    } else {
        rect.right() - 8.0
    };
    let text_rect = egui::Rect::from_min_max(
        egui::pos2(text_left, rect.top()),
        egui::pos2(text_right.max(text_left), rect.bottom()),
    );
    let color = if active {
        Color32::WHITE
    } else {
        Color32::from_gray(200)
    };
    ui.put(
        text_rect,
        egui::Label::new(egui::RichText::new(name).size(13.0).color(color))
            .truncate()
            .selectable(false),
    );

    let mut closed = false;
    if show_close {
        let cb = egui::Rect::from_center_size(
            egui::pos2(rect.right() - 15.0, rect.center().y),
            egui::vec2(16.0, 16.0),
        );
        if ui
            .put(
                cb,
                egui::Button::new(egui::RichText::new(icon::REMOVE).size(10.0)).frame(false),
            )
            .on_hover_text("Close tab (middle-click also closes)")
            .clicked()
        {
            *tab_close = Some(index);
            closed = true;
        }
    }
    if resp.middle_clicked() {
        *tab_close = Some(index);
        closed = true;
    }
    let resp = resp.on_hover_text(path.display().to_string());
    !closed && resp.clicked()
}

/// A captioned ribbon group: its widgets in a row, with a small centered
/// category caption painted underneath.
fn ribbon_group<R>(ui: &mut egui::Ui, caption: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let out = ui.vertical(|ui| {
        ui.add_space(3.0);
        let inner = ui.horizontal(add);
        let rect = inner.response.rect;
        ui.painter().text(
            egui::pos2(rect.center().x, rect.bottom() + 9.0),
            egui::Align2::CENTER_CENTER,
            caption,
            egui::FontId::proportional(9.5),
            Color32::from_gray(110),
        );
        ui.add_space(14.0);
        inner.inner
    });
    out.inner
}

/// Compact ahead/behind badge text (zero counts omitted), e.g. `⤴2  ⬇1`.
fn ahead_behind_label(ahead: usize, behind: usize) -> String {
    let mut parts = Vec::new();
    if ahead > 0 {
        parts.push(format!("{}{ahead}", icon::PUSH));
    }
    if behind > 0 {
        parts.push(format!("{}{behind}", icon::PULL));
    }
    parts.join("  ")
}

/// The Fork-style repository sidebar: the repo's identity up top, then a
/// filter box over collapsible Branches / Remotes / Tags / Stashes sections.
/// Rows act like Fork's: double-click checks out, right-click offers the rest.
fn sidebar_ui(ui: &mut egui::Ui, sel: usize, tab: &mut RepoTab, out: &mut SidebarOut) {
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.add_space(4.0);
        ui.label(egui::RichText::new(&tab.name).size(14.0).strong());
        // The collapse control lives inside the sidebar (GitKraken-style),
        // not in the ribbon.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(2.0);
            if ui
                .small_button(icon::CARET_LEFT)
                .on_hover_text("Collapse the sidebar")
                .clicked()
            {
                out.collapse = true;
            }
        });
    });
    let (branch, ahead, behind) = tab
        .state
        .status
        .as_ref()
        .map(|s| (s.branch.clone(), s.ahead, s.behind))
        .unwrap_or((None, 0, 0));
    ui.horizontal(|ui| {
        ui.add_space(4.0);
        let b = branch.as_deref().unwrap_or("detached");
        ui.label(
            egui::RichText::new(format!("{} {b}", icon::DOT))
                .color(Color32::from_rgb(0x2e, 0xa0, 0x43)),
        );
        if ahead > 0 || behind > 0 {
            ui.label(
                egui::RichText::new(ahead_behind_label(ahead, behind))
                    .color(Color32::from_gray(150)),
            )
            .on_hover_text("Commits ahead / behind the upstream branch");
        }
    });
    ui.add_space(6.0);
    // View switch (Fork's "Local Changes / All Commits" section).
    let changed = tab
        .state
        .status
        .as_ref()
        .map(|s| s.entries.len())
        .unwrap_or(0);
    let changes_label = if changed > 0 {
        format!("{}  Local Changes ({changed})", icon::RENAME)
    } else {
        format!("{}  Local Changes", icon::RENAME)
    };
    if ui
        .selectable_label(matches!(tab.view, View::Changes), changes_label)
        .clicked()
    {
        out.set_view = Some(View::Changes);
    }
    if ui
        .selectable_label(
            matches!(tab.view, View::History),
            format!("{}  All Commits", icon::COMMIT),
        )
        .clicked()
    {
        out.set_view = Some(View::History);
    }
    ui.add_space(6.0);
    ui.separator();
    ui.add_space(4.0);
    ui.add(
        egui::TextEdit::singleline(&mut tab.sidebar_filter)
            .hint_text("Filter…")
            .desired_width(f32::INFINITY),
    );
    ui.add_space(2.0);
    let filter = tab.sidebar_filter.to_lowercase();
    let matches = |s: &str| filter.is_empty() || s.to_lowercase().contains(&filter);

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // --- local branches ---
            let locals: Vec<(String, bool)> = tab
                .state
                .refs
                .iter()
                .filter(|r| r.kind == RefKind::LocalBranch)
                .map(|r| (r.name.short().to_string(), r.is_head))
                .filter(|(n, _)| matches(n))
                .collect();
            egui::CollapsingHeader::new(format!("Branches ({})", locals.len()))
                .default_open(true)
                .show(ui, |ui| {
                    if ui.button(format!("{}  New branch…", icon::ADD)).clicked() {
                        out.open_dialog = Some(BranchDialog::New {
                            repo: sel,
                            name: String::new(),
                            start: None,
                            checkout: true,
                        });
                    }
                    for (name, is_head) in &locals {
                        let text = if *is_head {
                            egui::RichText::new(format!("{} {name}", icon::DOT)).strong()
                        } else {
                            egui::RichText::new(format!("   {name}"))
                        };
                        let resp = ui
                            .selectable_label(*is_head, text)
                            .on_hover_text("Double-click to check out · right-click for more");
                        if resp.double_clicked() && !is_head {
                            out.branch_cmd = Some((sel, BranchCmd::Checkout(name.clone())));
                        }
                        resp.context_menu(|ui| {
                            if !is_head && ui.button("Checkout").clicked() {
                                out.branch_cmd = Some((sel, BranchCmd::Checkout(name.clone())));
                                ui.close();
                            }
                            if ui.button("Rename…").clicked() {
                                out.open_dialog = Some(BranchDialog::Rename {
                                    repo: sel,
                                    old: name.clone(),
                                    name: name.clone(),
                                });
                                ui.close();
                            }
                            if !is_head && ui.button("Delete…").clicked() {
                                out.open_dialog = Some(BranchDialog::Delete {
                                    repo: sel,
                                    name: name.clone(),
                                    force: false,
                                });
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("Copy name").clicked() {
                                ui.ctx().copy_text(name.clone());
                                ui.close();
                            }
                        });
                    }
                    if locals.is_empty() {
                        ui.weak("No branches.");
                    }
                });

            // --- remote branches, grouped per remote ---
            let mut by_remote: Vec<(String, Vec<String>)> = Vec::new();
            for r in tab
                .state
                .refs
                .iter()
                .filter(|r| r.kind == RefKind::RemoteBranch)
            {
                let full = r.name.short().to_string();
                if !matches(&full) {
                    continue;
                }
                let remote = full
                    .split_once('/')
                    .map(|(remote, _)| remote)
                    .unwrap_or(full.as_str())
                    .to_string();
                match by_remote.iter_mut().find(|(n, _)| *n == remote) {
                    Some((_, v)) => v.push(full),
                    None => by_remote.push((remote, vec![full])),
                }
            }
            let n_remote: usize = by_remote.iter().map(|(_, v)| v.len()).sum();
            egui::CollapsingHeader::new(format!("Remotes ({n_remote})"))
                .default_open(false)
                .show(ui, |ui| {
                    if ui
                        .button(format!("{}  Manage remotes…", icon::SETTINGS))
                        .clicked()
                    {
                        out.branch_cmd = Some((sel, BranchCmd::ManageRemotes));
                    }
                    for (remote, branches) in &by_remote {
                        egui::CollapsingHeader::new(remote)
                            .default_open(true)
                            .show(ui, |ui| {
                                for full in branches {
                                    let short = full
                                        .split_once('/')
                                        .map(|(_, rest)| rest)
                                        .unwrap_or(full.as_str());
                                    let resp =
                                        ui.selectable_label(false, short).on_hover_text(format!(
                                        "{full}\n\nDouble-click to check out as a local tracking branch"
                                    ));
                                    let mut go = resp.double_clicked();
                                    resp.context_menu(|ui| {
                                        if ui.button("Checkout as local branch").clicked() {
                                            go = true;
                                            ui.close();
                                        }
                                        if ui.button("Copy name").clicked() {
                                            ui.ctx().copy_text(full.clone());
                                            ui.close();
                                        }
                                    });
                                    if go {
                                        out.branch_cmd = Some((
                                            sel,
                                            BranchCmd::CheckoutTracking {
                                                local: short.to_string(),
                                                start: full.clone(),
                                            },
                                        ));
                                    }
                                }
                            });
                    }
                    if by_remote.is_empty() {
                        ui.weak("No remote branches.");
                    }
                });

            // --- tags ---
            let tags: Vec<String> = tab
                .state
                .refs
                .iter()
                .filter(|r| r.kind == RefKind::Tag)
                .map(|r| r.name.short().to_string())
                .filter(|n| matches(n))
                .collect();
            egui::CollapsingHeader::new(format!("Tags ({})", tags.len()))
                .default_open(false)
                .show(ui, |ui| {
                    if ui.button(format!("{}  New tag…", icon::TAG)).clicked() {
                        out.tag_dialog_at = Some(None);
                    }
                    for t in &tags {
                        let resp = ui.selectable_label(false, format!("{}  {t}", icon::TAG));
                        resp.context_menu(|ui| {
                            if ui.button("Delete tag").clicked() {
                                out.branch_cmd = Some((sel, BranchCmd::DeleteTag(t.clone())));
                                ui.close();
                            }
                            if ui.button("Copy name").clicked() {
                                ui.ctx().copy_text(t.clone());
                                ui.close();
                            }
                        });
                    }
                    if tags.is_empty() {
                        ui.weak("No tags.");
                    }
                });

            // --- stashes ---
            let stashes: Vec<(usize, String)> = tab
                .state
                .stashes
                .iter()
                .map(|s| (s.index, s.message.clone()))
                .filter(|(_, m)| matches(m))
                .collect();
            egui::CollapsingHeader::new(format!("Stashes ({})", stashes.len()))
                .default_open(false)
                .show(ui, |ui| {
                    for (idx, msg) in &stashes {
                        let body = egui::TextStyle::Body.resolve(ui.style());
                        let budget = (ui.available_width() - 24.0).max(60.0);
                        let shown = elide_left(ui, msg, &body, budget);
                        let resp = ui
                            .selectable_label(false, format!("stash@{{{idx}}}: {shown}"))
                            .on_hover_text(msg);
                        resp.context_menu(|ui| {
                            if ui.button("Apply").clicked() {
                                out.stash_cmd = Some(StashCmd::Apply(*idx));
                                ui.close();
                            }
                            if ui.button("Pop (apply & drop)").clicked() {
                                out.stash_cmd = Some(StashCmd::Pop(*idx));
                                ui.close();
                            }
                            if ui.button("Drop").clicked() {
                                out.stash_cmd = Some(StashCmd::Drop(*idx));
                                ui.close();
                            }
                        });
                    }
                    if stashes.is_empty() {
                        ui.weak("No stashes.");
                    }
                });
        });
}

/// Elide `text` from the left so its tail (for a path, the file name) stays
/// visible, fitting `max_width` in `font` and prefixing `…` when truncated.
fn elide_left(ui: &egui::Ui, text: &str, font: &egui::FontId, max_width: f32) -> String {
    let width = |s: &str| {
        ui.fonts(|f| {
            f.layout_no_wrap(s.to_owned(), font.clone(), Color32::WHITE)
                .size()
                .x
        })
    };
    if width(text) <= max_width {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    // Smallest start index whose `…`+suffix fits (fit is monotonic in start).
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi) / 2;
        let candidate: String = std::iter::once('…')
            .chain(chars[mid..].iter().copied())
            .collect();
        if width(&candidate) <= max_width {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    if lo >= chars.len() {
        "…".to_string()
    } else {
        std::iter::once('…')
            .chain(chars[lo..].iter().copied())
            .collect()
    }
}

fn split3(s: &str, a: usize, b: usize) -> (&str, &str, &str) {
    let a = a.min(s.len());
    let b = b.min(s.len()).max(a);
    (&s[..a], &s[a..b], &s[b..])
}

fn meta_row(ui: &mut egui::Ui, key: &str, value: &str) {
    ui.label(
        egui::RichText::new(key)
            .color(Color32::from_gray(140))
            .strong(),
    );
    ui.label(egui::RichText::new(value).monospace());
    ui.end_row();
}

/// Column x-coordinates for one full-width row, shared by header and rows.
struct Cols {
    msg_l: f32,
    msg_r: f32,
    author_l: f32,
    date_l: f32,
    sha_l: f32,
    right: f32,
}

fn columns(rect: egui::Rect, gutter: f32) -> Cols {
    let right = rect.right() - COL_PAD;
    let sha_l = right - COL_SHA_W;
    let date_l = sha_l - COL_GAP - COL_DATE_W;
    let author_l = date_l - COL_GAP - COL_AUTHOR_W;
    let msg_l = rect.left() + gutter + 10.0;
    let msg_r = (author_l - COL_GAP).max(msg_l);
    Cols {
        msg_l,
        msg_r,
        author_l,
        date_l,
        sha_l,
        right,
    }
}

fn col_rect(rect: egui::Rect, l: f32, r: f32) -> egui::Rect {
    egui::Rect::from_min_max(
        egui::pos2(l, rect.top()),
        egui::pos2(r.max(l), rect.bottom()),
    )
}

/// Paint the column header strip.
fn draw_header(ui: &egui::Ui, rect: egui::Rect, gutter: f32) {
    let painter = ui.painter_at(rect);
    let cols = columns(rect, gutter);
    let muted = Color32::from_gray(140);
    let font = egui::FontId::proportional(11.0);
    let cy = rect.center().y;
    let head = |x: f32, s: &str| {
        painter.text(
            egui::pos2(x, cy),
            egui::Align2::LEFT_CENTER,
            s,
            font.clone(),
            muted,
        );
    };
    head(cols.msg_l, "SUBJECT");
    head(cols.author_l, "AUTHOR");
    head(cols.date_l, "DATE");
    head(cols.sha_l, "COMMIT");
    painter.line_segment(
        [
            egui::pos2(rect.left(), rect.bottom() - 0.5),
            egui::pos2(rect.right(), rect.bottom() - 0.5),
        ],
        egui::Stroke::new(1.0_f32, Color32::from_gray(60)),
    );
}

/// Width of the subject column in the minified history strip.
const MINI_SUBJECT_W: f32 = 340.0;
/// Gutter assumed when sizing the strip before the layout is known (a few
/// lanes of graph).
const MINI_DEFAULT_GUTTER: f32 = 72.0;

/// Column x-coordinates for the minified strip: graph, date, and SHA up
/// front; subject and author reachable by horizontal scrolling.
struct MiniCols {
    date_l: f32,
    sha_l: f32,
    msg_l: f32,
    author_l: f32,
}

fn mini_columns(rect: egui::Rect, gutter: f32) -> MiniCols {
    let date_l = rect.left() + gutter + 10.0;
    let sha_l = date_l + COL_DATE_W + COL_GAP;
    let msg_l = sha_l + COL_SHA_W + COL_GAP;
    let author_l = msg_l + MINI_SUBJECT_W + COL_GAP;
    MiniCols {
        date_l,
        sha_l,
        msg_l,
        author_l,
    }
}

/// The strip width that shows graph + date + SHA without scrolling.
fn mini_content_width(gutter: f32) -> f32 {
    gutter + 10.0 + COL_DATE_W + COL_GAP + COL_SHA_W + COL_PAD
}

/// The full scrollable content width of the strip (all columns).
fn mini_full_width(gutter: f32) -> f32 {
    mini_content_width(gutter) + COL_GAP + MINI_SUBJECT_W + COL_GAP + COL_AUTHOR_W + COL_PAD
}

/// Paint the minified strip's column header.
fn draw_mini_header(ui: &egui::Ui, rect: egui::Rect, gutter: f32) {
    let painter = ui.painter_at(rect);
    let cols = mini_columns(rect, gutter);
    let muted = Color32::from_gray(140);
    let font = egui::FontId::proportional(11.0);
    let cy = rect.center().y;
    let head = |x: f32, s: &str| {
        painter.text(
            egui::pos2(x, cy),
            egui::Align2::LEFT_CENTER,
            s,
            font.clone(),
            muted,
        );
    };
    head(cols.date_l, "DATE");
    head(cols.sha_l, "COMMIT");
    head(cols.msg_l, "SUBJECT");
    head(cols.author_l, "AUTHOR");
    painter.line_segment(
        [
            egui::pos2(rect.left(), rect.bottom() - 0.5),
            egui::pos2(rect.right(), rect.bottom() - 0.5),
        ],
        egui::Stroke::new(1.0_f32, Color32::from_gray(60)),
    );
}

/// Paint one minified commit row: background, graph gutter, date, short SHA,
/// then (behind the horizontal scroll) ref pills + subject and author.
#[allow(clippy::too_many_arguments)]
fn draw_commit_row_mini(
    ui: &egui::Ui,
    rect: egui::Rect,
    row: &gg_core::GraphRow,
    commit: &CommitMeta,
    index: usize,
    gutter: f32,
    selected: bool,
    hovered: bool,
    chips: Option<&Vec<RefChip>>,
) {
    let painter = ui.painter_at(rect);
    let visuals = ui.visuals();

    if selected {
        painter.rect_filled(rect, egui::CornerRadius::ZERO, visuals.selection.bg_fill);
    } else if hovered {
        painter.rect_filled(
            rect,
            egui::CornerRadius::ZERO,
            Color32::from_white_alpha(10),
        );
    } else if index % 2 == 1 {
        painter.rect_filled(rect, egui::CornerRadius::ZERO, Color32::from_white_alpha(4));
    }

    let cols = mini_columns(rect, gutter);
    let cy = rect.center().y;
    let text_color = visuals.text_color();

    // Graph gutter (clipped so wide graphs never spill into the date column).
    let gutter_rect = egui::Rect::from_min_size(rect.min, egui::vec2(gutter, ROW_HEIGHT));
    let gpainter = ui.painter_at(gutter_rect);
    let mut canvas = EguiCanvas::new(&gpainter);
    let metrics = GraphMetrics {
        row_height: ROW_HEIGHT,
        lane_width: LANE_WIDTH,
        node_radius: 4.5,
        edge_width: 2.0,
        x_offset: gutter_rect.left() + 8.0,
        y_offset: rect.top(),
    };
    draw_row(
        &mut canvas,
        row,
        Viewport {
            first_row: index,
            visible_rows: 1,
        },
        &metrics,
    );

    // Date column.
    let date_rect = col_rect(rect, cols.date_l, cols.sha_l - COL_GAP);
    ui.painter_at(date_rect).text(
        egui::pos2(cols.date_l, cy),
        egui::Align2::LEFT_CENTER,
        fmt_time(commit.author.time),
        egui::FontId::monospace(12.0),
        Color32::from_gray(150),
    );

    // SHA column.
    let sha_rect = col_rect(rect, cols.sha_l, cols.msg_l - COL_GAP);
    ui.painter_at(sha_rect).text(
        egui::pos2(cols.sha_l, cy),
        egui::Align2::LEFT_CENTER,
        commit.oid.short(8),
        egui::FontId::monospace(12.0),
        Color32::from_gray(150),
    );

    // Subject column (ref pills + summary), clipped to its column.
    let msg_rect = col_rect(rect, cols.msg_l, cols.author_l - COL_GAP);
    let mpainter = ui.painter_at(msg_rect);
    let mut x = cols.msg_l;
    if let Some(chips) = chips {
        for chip in chips {
            let galley = ui.fonts(|f| {
                f.layout_no_wrap(chip.text.clone(), egui::FontId::proportional(11.0), chip.fg)
            });
            let w = galley.size().x + 12.0;
            let pill = egui::Rect::from_min_size(egui::pos2(x, cy - 8.0), egui::vec2(w, 16.0));
            mpainter.rect_filled(pill, egui::CornerRadius::same(8), chip.fill);
            mpainter.galley(
                egui::pos2(x + 6.0, cy - galley.size().y / 2.0),
                galley,
                chip.fg,
            );
            x += w + 5.0;
        }
    }
    let summary = if commit.summary.is_empty() {
        commit.message.lines().next().unwrap_or("")
    } else {
        &commit.summary
    };
    mpainter.text(
        egui::pos2(x + 2.0, cy),
        egui::Align2::LEFT_CENTER,
        summary,
        egui::FontId::proportional(13.0),
        text_color,
    );

    // Author column: colored avatar + name.
    let author_rect = col_rect(rect, cols.author_l, rect.right() - COL_PAD);
    let apainter = ui.painter_at(author_rect);
    let avatar = avatar_color(&commit.author.email, &commit.author.name);
    let center = egui::pos2(cols.author_l + 8.0, cy);
    apainter.circle_filled(center, 8.0, avatar);
    apainter.text(
        center,
        egui::Align2::CENTER_CENTER,
        initials(&commit.author.name),
        egui::FontId::proportional(10.0),
        Color32::WHITE,
    );
    apainter.text(
        egui::pos2(cols.author_l + 22.0, cy),
        egui::Align2::LEFT_CENTER,
        &commit.author.name,
        egui::FontId::proportional(13.0),
        text_color,
    );
}

/// Paint one commit row: background, graph gutter, ref pills, subject, author
/// (with avatar), date, and short SHA.
#[allow(clippy::too_many_arguments)]
fn draw_commit_row(
    ui: &egui::Ui,
    rect: egui::Rect,
    row: &gg_core::GraphRow,
    commit: &CommitMeta,
    index: usize,
    gutter: f32,
    selected: bool,
    hovered: bool,
    chips: Option<&Vec<RefChip>>,
) {
    let painter = ui.painter_at(rect);
    let visuals = ui.visuals();

    if selected {
        painter.rect_filled(rect, egui::CornerRadius::ZERO, visuals.selection.bg_fill);
    } else if hovered {
        painter.rect_filled(
            rect,
            egui::CornerRadius::ZERO,
            Color32::from_white_alpha(10),
        );
    } else if index % 2 == 1 {
        painter.rect_filled(rect, egui::CornerRadius::ZERO, Color32::from_white_alpha(4));
    }

    let cols = columns(rect, gutter);
    let cy = rect.center().y;
    let text_color = visuals.text_color();

    // Graph gutter (clipped so wide graphs never spill into the subject column).
    let gutter_rect = egui::Rect::from_min_size(rect.min, egui::vec2(gutter, ROW_HEIGHT));
    let gpainter = ui.painter_at(gutter_rect);
    let mut canvas = EguiCanvas::new(&gpainter);
    let metrics = GraphMetrics {
        row_height: ROW_HEIGHT,
        lane_width: LANE_WIDTH,
        node_radius: 4.5,
        edge_width: 2.0,
        x_offset: gutter_rect.left() + 8.0,
        y_offset: rect.top(),
    };
    draw_row(
        &mut canvas,
        row,
        Viewport {
            first_row: index,
            visible_rows: 1,
        },
        &metrics,
    );

    // Subject column: ref pills then the summary, clipped to the column.
    let msg_rect = col_rect(rect, cols.msg_l, cols.msg_r);
    let mpainter = ui.painter_at(msg_rect);
    let mut x = cols.msg_l;
    if let Some(chips) = chips {
        for chip in chips {
            let galley = ui.fonts(|f| {
                f.layout_no_wrap(chip.text.clone(), egui::FontId::proportional(11.0), chip.fg)
            });
            let w = galley.size().x + 12.0;
            let pill = egui::Rect::from_min_size(egui::pos2(x, cy - 8.0), egui::vec2(w, 16.0));
            mpainter.rect_filled(pill, egui::CornerRadius::same(8), chip.fill);
            mpainter.galley(
                egui::pos2(x + 6.0, cy - galley.size().y / 2.0),
                galley,
                chip.fg,
            );
            x += w + 5.0;
        }
    }
    let summary = if commit.summary.is_empty() {
        commit.message.lines().next().unwrap_or("")
    } else {
        &commit.summary
    };
    mpainter.text(
        egui::pos2(x + 2.0, cy),
        egui::Align2::LEFT_CENTER,
        summary,
        egui::FontId::proportional(13.0),
        text_color,
    );

    // Author column: colored avatar + name.
    let author_rect = col_rect(rect, cols.author_l, cols.date_l - COL_GAP);
    let apainter = ui.painter_at(author_rect);
    let avatar = avatar_color(&commit.author.email, &commit.author.name);
    let center = egui::pos2(cols.author_l + 8.0, cy);
    apainter.circle_filled(center, 8.0, avatar);
    apainter.text(
        center,
        egui::Align2::CENTER_CENTER,
        initials(&commit.author.name),
        egui::FontId::proportional(10.0),
        Color32::WHITE,
    );
    apainter.text(
        egui::pos2(cols.author_l + 22.0, cy),
        egui::Align2::LEFT_CENTER,
        &commit.author.name,
        egui::FontId::proportional(13.0),
        text_color,
    );

    // Date column.
    let date_rect = col_rect(rect, cols.date_l, cols.sha_l - COL_GAP);
    ui.painter_at(date_rect).text(
        egui::pos2(cols.date_l, cy),
        egui::Align2::LEFT_CENTER,
        fmt_time(commit.author.time),
        egui::FontId::monospace(12.0),
        Color32::from_gray(150),
    );

    // SHA column.
    let sha_rect = col_rect(rect, cols.sha_l, cols.right);
    ui.painter_at(sha_rect).text(
        egui::pos2(cols.sha_l, cy),
        egui::Align2::LEFT_CENTER,
        commit.oid.short(8),
        egui::FontId::monospace(12.0),
        Color32::from_gray(130),
    );
}

/// A decorated reference pill (branch/tag/HEAD) shown next to a commit.
struct RefChip {
    text: String,
    fill: Color32,
    fg: Color32,
}

/// Map a commit oid to its ref pills, colored by kind.
fn build_label_map(refs: &[RefRecord]) -> HashMap<Oid, Vec<RefChip>> {
    let mut map: HashMap<Oid, Vec<RefChip>> = HashMap::new();
    for r in refs {
        let (text, fill) = match r.kind {
            RefKind::Tag => (
                format!("{} {}", icon::TAG, r.name.short()),
                Color32::from_rgb(0xb7, 0x86, 0x12),
            ),
            RefKind::RemoteBranch => (
                r.name.short().to_string(),
                Color32::from_rgb(0x7a, 0x52, 0xc4),
            ),
            _ => (
                r.name.short().to_string(),
                Color32::from_rgb(0x2d, 0x6c, 0xdf),
            ),
        };
        let (text, fill) = if r.is_head {
            (
                format!("HEAD {} {text}", icon::ARROW),
                Color32::from_rgb(0x2e, 0xa0, 0x43),
            )
        } else {
            (text, fill)
        };
        map.entry(r.target).or_default().push(RefChip {
            text,
            fill,
            fg: Color32::WHITE,
        });
    }
    map
}

/// A muted, pleasant avatar palette.
const AVATAR_PALETTE: [Color32; 10] = [
    Color32::from_rgb(0x4f, 0x83, 0xcc),
    Color32::from_rgb(0xcc, 0x5b, 0x5b),
    Color32::from_rgb(0x4c, 0xa6, 0x6b),
    Color32::from_rgb(0xc9, 0x8a, 0x3a),
    Color32::from_rgb(0x8a, 0x63, 0xc9),
    Color32::from_rgb(0x3a, 0xa6, 0xb0),
    Color32::from_rgb(0xc9, 0x6a, 0x3a),
    Color32::from_rgb(0xc4, 0x5b, 0x8f),
    Color32::from_rgb(0x6a, 0x8f, 0x3a),
    Color32::from_rgb(0x5b, 0x6a, 0xc4),
];

fn avatar_color(email: &str, name: &str) -> Color32 {
    let key = if email.is_empty() { name } else { email };
    let mut h: u32 = 2166136261;
    for b in key.bytes() {
        h = (h ^ b as u32).wrapping_mul(16777619);
    }
    AVATAR_PALETTE[(h as usize) % AVATAR_PALETTE.len()]
}

fn initials(name: &str) -> String {
    name.split_whitespace()
        .filter_map(|w| w.chars().next())
        .take(2)
        .collect::<String>()
        .to_uppercase()
}

/// Apply a modern dark theme: comfortable spacing, rounded widgets, accents.
fn configure_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    let mut v = egui::Visuals::dark();

    v.panel_fill = Color32::from_rgb(0x1b, 0x1d, 0x21);
    v.window_fill = Color32::from_rgb(0x1b, 0x1d, 0x21);
    v.extreme_bg_color = Color32::from_rgb(0x14, 0x15, 0x18);
    v.faint_bg_color = Color32::from_rgb(0x22, 0x24, 0x29);
    v.selection.bg_fill = Color32::from_rgb(0x2c, 0x44, 0x66);
    v.selection.stroke = egui::Stroke::new(1.0_f32, Color32::from_rgb(0x6f, 0xa8, 0xff));
    v.hyperlink_color = Color32::from_rgb(0x6f, 0xa8, 0xff);

    let radius = egui::CornerRadius::same(6);
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = radius;
    }
    v.widgets.inactive.bg_fill = Color32::from_rgb(0x2a, 0x2d, 0x33);
    v.widgets.inactive.weak_bg_fill = Color32::from_rgb(0x26, 0x29, 0x2f);
    v.widgets.hovered.bg_fill = Color32::from_rgb(0x33, 0x37, 0x3f);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x33, 0x37, 0x3f);
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, Color32::from_rgb(0x4a, 0x50, 0x5c));
    v.widgets.active.bg_fill = Color32::from_rgb(0x2c, 0x44, 0x66);
    v.window_stroke = egui::Stroke::new(1.0_f32, Color32::from_rgb(0x34, 0x38, 0x40));
    v.window_shadow = egui::Shadow {
        offset: [0, 6],
        blur: 18,
        spread: 0,
        color: Color32::from_black_alpha(120),
    };

    style.visuals = v;
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 6.0);
    style.spacing.scroll = egui::style::ScrollStyle::solid();
    ctx.set_style(style);
}

/// Format an author/committer time as `YYYY-MM-DD HH:MM` in its own offset.
fn fmt_time(t: Time) -> String {
    let secs = t.seconds + (t.offset_minutes as i64) * 60;
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (y, m, d) = ymd_from_days(days);
    let (hh, mm) = (rem / 3600, (rem % 3600) / 60);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}")
}

/// Howard Hinnant's days-from-civil, inverted: days since the Unix epoch to (y, m, d).
fn ymd_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (y + if m <= 2 { 1 } else { 0 }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_time_in_utc() {
        assert_eq!(fmt_time(Time::new(1_700_000_000, 0)), "2023-11-14 22:13");
    }

    #[test]
    fn time_respects_offset() {
        assert_eq!(fmt_time(Time::new(1_700_000_000, 120)), "2023-11-15 00:13");
    }

    #[test]
    fn initials_take_first_two_words() {
        assert_eq!(initials("Ada Lovelace"), "AL");
        assert_eq!(initials("madonna"), "M");
        assert_eq!(initials("  Grace  Brewster  Hopper "), "GB");
        assert_eq!(initials(""), "");
    }

    #[test]
    fn columns_are_ordered_left_to_right() {
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1000.0, ROW_HEIGHT));
        let c = columns(rect, 50.0);
        assert!(c.msg_l < c.msg_r);
        assert!(c.msg_r <= c.author_l);
        assert!(c.author_l < c.date_l);
        assert!(c.date_l < c.sha_l);
        assert!(c.sha_l < c.right);
    }

    #[test]
    fn build_commit_message_joins_title_and_body() {
        assert_eq!(build_commit_message("Title", ""), "Title");
        assert_eq!(build_commit_message(" Title ", " Body "), "Title\n\nBody");
        assert_eq!(
            build_commit_message("T", "line1\nline2"),
            "T\n\nline1\nline2"
        );
    }

    #[test]
    fn split3_is_safe_on_bounds() {
        assert_eq!(split3("hello", 1, 3), ("h", "el", "lo"));
        assert_eq!(split3("hi", 5, 9), ("hi", "", ""));
    }

    #[test]
    fn repo_name_from_clone_urls() {
        let name = |u: &str| repo_name_from_url(u);
        assert_eq!(
            name("https://github.com/a/repo.git").as_deref(),
            Some("repo")
        );
        assert_eq!(name("git@github.com:a/repo.git").as_deref(), Some("repo"));
        assert_eq!(name("https://github.com/a/repo/").as_deref(), Some("repo"));
        assert_eq!(name("/local/path/repo").as_deref(), Some("repo"));
        assert_eq!(name("repo"), None);
        assert_eq!(name(""), None);
        assert_eq!(name("https://github.com/a/.git"), None);
    }

    #[test]
    fn classifies_auth_errors() {
        assert!(is_auth_error(
            "fatal: could not read Username for 'https://github.com': terminal prompts disabled"
        ));
        assert!(is_auth_error(
            "remote: Invalid username or token.\nfatal: Authentication failed for 'https://x'"
        ));
        assert!(is_auth_error(
            "git@github.com: Permission denied (publickey)."
        ));
        assert!(is_auth_error(
            "gg-askpass: no credential available for prompt: Password for 'https://x':"
        ));
        assert!(!is_auth_error("fatal: repository 'x' does not exist"));
        assert!(!is_auth_error(
            "fatal: unable to access 'x': Could not resolve host: github.com"
        ));
    }

    #[test]
    fn session_credentials_fill_both_secret_kinds() {
        let c = session_credentials(" user ", "s3cret");
        assert_eq!(c.username.as_deref(), Some("user"));
        assert_eq!(c.password.as_deref(), Some("s3cret"));
        assert_eq!(c.passphrase.as_deref(), Some("s3cret"));
        let empty = session_credentials("", "");
        assert!(empty.username.is_none());
        assert!(empty.password.is_none());
        assert!(empty.passphrase.is_none());
    }

    #[test]
    fn scan_finds_repos_and_respects_limits() {
        let root = std::env::temp_dir().join(format!("gittify-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // root/a (repo), root/group/b (repo), root/.hidden/c (repo, skipped),
        // root/node_modules/d (repo, skipped), root/a/nested (repo inside a
        // repo: not descended into), root/x/y/z/deep (repo at depth 4: too deep).
        for dir in [
            "a/.git",
            "a/nested/.git",
            "group/b/.git",
            ".hidden/c/.git",
            "node_modules/d/.git",
            "x/y/z/deep/.git",
        ] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let mut found = Vec::new();
        scan_for_repos(&root, SCAN_DEPTH, &mut found);
        assert_eq!(found, vec![root.join("a"), root.join("group/b")]);
        // A root that is itself a repo yields exactly itself.
        let mut only_root = Vec::new();
        scan_for_repos(&root.join("a"), SCAN_DEPTH, &mut only_root);
        assert_eq!(only_root, vec![root.join("a")]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn init_repo_creates_a_repository() {
        let target = std::env::temp_dir().join(format!("gittify-init-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&target);
        init_repo(&target).expect("init");
        assert!(target.join(".git").is_dir());
        // A second init into the now-occupied folder is refused.
        assert!(init_repo(&target).is_err());
        std::fs::remove_dir_all(&target).ok();
    }
}
