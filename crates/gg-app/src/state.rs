//! The UI-facing data model. A front-end keeps one [`AppState`], drains events
//! from the [`crate::AppHandle`], and folds each into the state via
//! [`AppState::apply`], then renders from it.

use std::path::PathBuf;
use std::sync::Arc;

use gg_core::{FileDiff, Oid, RefRecord, Remote, StashEntry, StatusSnapshot};
use gg_git::{HistoryView, Progress};

use crate::Event;

/// A computed diff currently being previewed, with the request that produced it.
#[derive(Clone)]
pub struct DiffView {
    /// The path diffed.
    pub path: PathBuf,
    /// Whether this is the staged (index vs HEAD) diff.
    pub staged: bool,
    /// The diff contents.
    pub diff: FileDiff,
    /// Raw unified diff text git produced (empty for untracked/synthesized
    /// diffs), used to build single-hunk patches for hunk-level staging.
    pub raw: String,
}

/// The per-file diffs of a selected history commit.
#[derive(Clone)]
pub struct CommitDiffView {
    /// The commit these diffs belong to.
    pub oid: Oid,
    /// One entry per file the commit changed.
    pub files: Vec<FileDiff>,
}

/// The latest known state of the repository view.
#[derive(Default)]
pub struct AppState {
    /// Most recently loaded history + layout.
    pub history: Option<Arc<HistoryView>>,
    /// Reference list.
    pub refs: Vec<RefRecord>,
    /// Working-tree status (staged / unstaged / untracked).
    pub status: Option<StatusSnapshot>,
    /// The currently previewed file diff, if any.
    pub diff: Option<DiffView>,
    /// The selected history commit's per-file diffs, if any.
    pub commit_diff: Option<CommitDiffView>,
    /// Stash entries (most-recent first).
    pub stashes: Vec<StashEntry>,
    /// Configured remotes.
    pub remotes: Vec<Remote>,
    /// In-flight operation progress, if any.
    pub progress: Option<(String, Progress)>,
    /// Last error message surfaced to the user.
    pub last_error: Option<String>,
    /// A short status line summarizing the last completed operation.
    pub status_line: String,
}

impl AppState {
    /// Fold one event into the state.
    pub fn apply(&mut self, event: Event) {
        match event {
            Event::History(view) => {
                self.status_line = format!("loaded {} commits", view.commits.len());
                self.history = Some(view);
                self.progress = None;
            }
            Event::Refs(refs) => {
                self.refs = refs;
            }
            Event::Status(status) => {
                self.status = Some(status);
            }
            Event::Diff {
                path,
                staged,
                diff,
                raw,
            } => {
                self.diff = Some(DiffView {
                    path,
                    staged,
                    diff,
                    raw,
                });
            }
            Event::CommitDiff { oid, files } => {
                self.commit_diff = Some(CommitDiffView { oid, files });
            }
            Event::Stashes(stashes) => {
                self.stashes = stashes;
            }
            Event::Remotes(remotes) => {
                self.remotes = remotes;
            }
            Event::Progress { label, progress } => {
                self.progress = Some((label, progress));
            }
            // Raw output lines are consumed by the front-end's event drain (it
            // appends them to a per-tab transcript); nothing to fold into state.
            Event::OpOutput { .. } => {}
            Event::Completed(label) => {
                self.status_line = format!("{label} completed");
                self.last_error = None;
                self.progress = None;
            }
            Event::Failed { label, error } => {
                self.last_error = Some(format!("{label}: {error}"));
                self.progress = None;
            }
        }
    }

    /// The number of rows currently available to render.
    pub fn row_count(&self) -> usize {
        self.history.as_ref().map(|h| h.commits.len()).unwrap_or(0)
    }
}
