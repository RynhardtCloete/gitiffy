//! Working-tree and index status types.

use std::path::PathBuf;

/// The state of a single path in the index relative to HEAD (staged side) or
/// in the working tree relative to the index (unstaged side).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    /// Path is unchanged on this side.
    Unmodified,
    /// New file.
    Added,
    /// Content changed.
    Modified,
    /// File removed.
    Deleted,
    /// File renamed (see [`StatusEntry::orig_path`]).
    Renamed,
    /// File copied.
    Copied,
    /// Type changed (e.g. file -> symlink).
    TypeChanged,
    /// Unmerged (conflict).
    Conflicted,
    /// Untracked (working-tree only).
    Untracked,
    /// Ignored (working-tree only).
    Ignored,
}

/// One path's status, split across the staged (index vs HEAD) and unstaged
/// (worktree vs index) axes, matching git's two-column porcelain model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusEntry {
    /// Repo-relative path.
    pub path: PathBuf,
    /// Original path for renames/copies.
    pub orig_path: Option<PathBuf>,
    /// Change staged in the index (HEAD -> index).
    pub staged: ChangeKind,
    /// Change in the working tree (index -> worktree).
    pub unstaged: ChangeKind,
}

impl StatusEntry {
    /// True if there is anything staged for this path.
    pub fn is_staged(&self) -> bool {
        !matches!(self.staged, ChangeKind::Unmodified)
    }

    /// True if there is an unstaged change for this path.
    pub fn has_unstaged(&self) -> bool {
        !matches!(self.unstaged, ChangeKind::Unmodified)
    }

    /// True if the path is in a conflicted/unmerged state.
    pub fn is_conflicted(&self) -> bool {
        matches!(self.staged, ChangeKind::Conflicted)
            || matches!(self.unstaged, ChangeKind::Conflicted)
    }
}

/// A snapshot of `git status` for the whole working tree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusSnapshot {
    /// All changed/untracked entries.
    pub entries: Vec<StatusEntry>,
    /// Current branch short name, if on a branch (None when detached).
    pub branch: Option<String>,
    /// Upstream tracking branch, if configured.
    pub upstream: Option<String>,
    /// Commits ahead of upstream.
    pub ahead: usize,
    /// Commits behind upstream.
    pub behind: usize,
}

impl StatusSnapshot {
    /// True when the working tree and index are clean.
    pub fn is_clean(&self) -> bool {
        self.entries.is_empty()
    }
}

/// A stash entry from `git stash list`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StashEntry {
    /// Stash index (0 = most recent), addressed as `stash@{index}`.
    pub index: usize,
    /// Human-readable description (e.g. `WIP on main: …`).
    pub message: String,
}

/// A configured remote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remote {
    /// Remote name (e.g. `origin`).
    pub name: String,
    /// Fetch URL.
    pub url: String,
}
