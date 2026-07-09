//! Persistence of the workspace tree (nested groups of repository tabs), stored
//! as JSON so it survives across launches. The pre-workspace flat repo list is
//! migrated into a default workspace on first run.

use std::path::PathBuf;

use crate::workspace::WorkspaceStore;

fn config_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("gittify");
        }
    }
    #[cfg(windows)]
    if let Ok(app) = std::env::var("APPDATA") {
        if !app.is_empty() {
            return PathBuf::from(app).join("gittify");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home).join(".config").join("gittify");
        }
    }
    PathBuf::from(".gittify")
}

fn repos_file() -> PathBuf {
    config_dir().join("repos")
}

fn workspaces_file() -> PathBuf {
    config_dir().join("workspaces.json")
}

/// Load the legacy newline-delimited repo list (used only for migration).
fn load_legacy_repos() -> Vec<PathBuf> {
    match std::fs::read_to_string(repos_file()) {
        Ok(contents) => contents
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Load the workspace tree. Falls back to a default workspace seeded from the
/// legacy repo list when `workspaces.json` is missing or unreadable.
pub fn load_workspaces() -> WorkspaceStore {
    let mut store = match std::fs::read_to_string(workspaces_file()) {
        Ok(text) => serde_json::from_str::<WorkspaceStore>(&text)
            .unwrap_or_else(|_| WorkspaceStore::with_repos(load_legacy_repos())),
        Err(_) => WorkspaceStore::with_repos(load_legacy_repos()),
    };
    store.normalize();
    store
}

/// Persist the workspace tree, creating the config directory if needed.
pub fn save_workspaces(store: &WorkspaceStore) {
    let dir = config_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(json) = serde_json::to_string_pretty(store) {
        let _ = std::fs::write(workspaces_file(), json);
    }
}
