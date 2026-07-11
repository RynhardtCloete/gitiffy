//! `gg-git` — the facade that composes the read engine (gix) and the write
//! engine (system git) behind one [`GitEngine`], and encodes the decision rule
//! from the spec: **reads go through gix, mutations and network go through
//! git**. The reserve git2 backend would slot in behind the same `RepoReader`/
//! `RepoWriter` traits per-operation.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::HashMap;
use std::path::Path;

use gg_core::{CommitMeta, FileChange, FileDiff, Oid, Result, StatusSnapshot};
use gg_diff::DiffOptions;
use gg_graph::{topo_order, GraphLayout};

pub use gg_credentials::Credentials;
pub use gg_git_read::{GixRepo, RepoReader, WalkOpts};
pub use gg_git_write::{
    CancelToken, CommitOpts, FnSink, GitWriter, MergeOpts, NullSink, Progress, ProgressSink,
    RebaseOpts, RepoWriter, ResetMode, StashOp,
};

/// A topologically-ordered slice of history with its computed graph layout.
/// `commits[i]` corresponds to `layout.rows()[i]` (same display order).
pub struct HistoryView {
    /// Commits in display order (each commit precedes its parents).
    pub commits: Vec<CommitMeta>,
    /// The lane/edge layout, one row per commit.
    pub layout: GraphLayout,
}

/// The composed git engine for one repository.
pub struct GitEngine {
    reader: GixRepo,
    writer: GitWriter,
}

impl GitEngine {
    /// Discover the repository at or above `path` and wire up both engines.
    pub fn discover(path: impl AsRef<Path>) -> Result<Self> {
        let reader = GixRepo::discover(path.as_ref())?;
        let writer = GitWriter::discover(path.as_ref())?;
        Ok(Self { reader, writer })
    }

    /// The read engine (gix).
    pub fn reader(&self) -> &GixRepo {
        &self.reader
    }

    /// The write engine (system git).
    pub fn writer(&self) -> &GitWriter {
        &self.writer
    }

    /// Route the write engine's credential prompts through an askpass helper,
    /// supplying values via the environment channel.
    pub fn with_credentials(mut self, askpass: impl AsRef<Path>, creds: &Credentials) -> Self {
        let mut git = self.writer.git().clone().with_askpass(askpass.as_ref());
        for (k, v) in creds.to_env() {
            git = git.with_env(k, v);
        }
        self.writer = GitWriter::new(git);
        self
    }

    /// The working-tree status (staged, unstaged, and untracked changes).
    pub fn status(&self) -> Result<StatusSnapshot> {
        self.writer.status()
    }

    /// Compute the diff for one working-tree path.
    ///
    /// * `staged` — diff the index against HEAD; otherwise the working tree
    ///   against the index.
    /// * `untracked` — the file is not tracked, so it is shown as all-additions.
    pub fn diff_file(&self, path: &Path, staged: bool, untracked: bool) -> Result<FileDiff> {
        self.diff_file_with_raw(path, staged, untracked)
            .map(|(diff, _)| diff)
    }

    /// Like [`Self::diff_file`], but also returns the raw unified diff text git
    /// produced (empty for untracked/synthesized diffs). The raw text lets
    /// callers carve out a single hunk for partial staging via
    /// [`gg_diff::single_hunk_patch`], preserving git's exact bytes.
    pub fn diff_file_with_raw(
        &self,
        path: &Path,
        staged: bool,
        untracked: bool,
    ) -> Result<(FileDiff, String)> {
        if untracked {
            let full = self.writer.git().workdir().join(path);
            let bytes = std::fs::read(&full).unwrap_or_default();
            let diff = gg_diff::diff_file(
                &[],
                &bytes,
                path,
                None,
                FileChange::Added,
                &DiffOptions::default(),
            );
            return Ok((diff, String::new()));
        }
        let text = self.writer.raw_diff(path, staged)?;
        let diff = gg_diff::parse_unified(&text)
            .into_iter()
            .next()
            .unwrap_or_else(|| FileDiff {
                path: path.to_path_buf(),
                old_path: None,
                change: FileChange::Modified,
                is_binary: false,
                hunks: Vec::new(),
            });
        Ok((diff, text))
    }

    /// The per-file diffs a commit introduced (vs its first parent, or the empty
    /// tree for the root commit). Used to preview the files changed in a
    /// historical commit.
    pub fn commit_diff(&self, oid: Oid) -> Result<Vec<FileDiff>> {
        let text = self.writer.commit_raw_diff(oid)?;
        Ok(gg_diff::parse_unified(&text))
    }

    /// Walk history, normalize to a valid topological display order, and lay it
    /// out for rendering. Returns commits and layout aligned by row index.
    pub fn history_graph(&self, opts: &WalkOpts) -> Result<HistoryView> {
        let commits = self.reader.walk(opts)?;
        let inputs = topo_order(&commits);
        let by_oid: HashMap<Oid, CommitMeta> = commits.into_iter().map(|c| (c.oid, c)).collect();
        let ordered: Vec<CommitMeta> = inputs
            .iter()
            .filter_map(|i| by_oid.get(&i.oid).cloned())
            .collect();
        let layout = GraphLayout::from_commits(&inputs);
        Ok(HistoryView {
            commits: ordered,
            layout,
        })
    }
}
