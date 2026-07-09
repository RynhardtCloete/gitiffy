//! Diff model: files, hunks, lines, and intra-line token spans.
//!
//! These types are produced by `gg-diff` and consumed by the diff viewer. They
//! are virtualization-friendly: a viewer can render any sub-range of
//! [`FileDiff::hunks`] / [`Hunk::lines`] without touching the rest.

use std::path::PathBuf;

/// Which side of the diff a line belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    /// Unchanged context line (present on both sides).
    Context,
    /// Line added on the new side.
    Addition,
    /// Line removed from the old side.
    Deletion,
}

/// A half-open span of byte offsets within a line's text, used to highlight the
/// changed tokens within an otherwise-similar add/delete pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TokenSpan {
    /// Start byte offset within the line.
    pub start: usize,
    /// End byte offset (exclusive).
    pub end: usize,
}

/// A single line in a diff hunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    /// Add / delete / context.
    pub kind: LineKind,
    /// Line content without the trailing newline.
    pub text: String,
    /// 1-based line number on the old side, if applicable.
    pub old_lineno: Option<u32>,
    /// 1-based line number on the new side, if applicable.
    pub new_lineno: Option<u32>,
    /// Intra-line changed spans (empty unless computed for this line).
    pub intra: Vec<TokenSpan>,
}

/// A contiguous block of changes with surrounding context (a `@@ ... @@` hunk).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    /// 1-based starting line on the old side.
    pub old_start: u32,
    /// Number of lines from the old side covered by this hunk.
    pub old_lines: u32,
    /// 1-based starting line on the new side.
    pub new_start: u32,
    /// Number of lines from the new side covered by this hunk.
    pub new_lines: u32,
    /// Optional section heading git places after the `@@` marker.
    pub header: String,
    /// The lines making up the hunk.
    pub lines: Vec<DiffLine>,
}

/// How a file changed between the two diff endpoints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileChange {
    /// File added.
    Added,
    /// File deleted.
    Deleted,
    /// Content modified.
    Modified,
    /// File renamed.
    Renamed,
    /// File copied.
    Copied,
    /// Mode/type changed.
    TypeChanged,
}

/// The diff for a single file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiff {
    /// Path on the new side (or the surviving path for renames).
    pub path: PathBuf,
    /// Path on the old side for renames/copies.
    pub old_path: Option<PathBuf>,
    /// How the file changed.
    pub change: FileChange,
    /// True if either side is binary (in which case `hunks` is empty).
    pub is_binary: bool,
    /// The textual hunks (empty for binary or pure mode changes).
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    /// Total added lines across all hunks.
    pub fn additions(&self) -> usize {
        self.hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| l.kind == LineKind::Addition)
            .count()
    }

    /// Total deleted lines across all hunks.
    pub fn deletions(&self) -> usize {
        self.hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| l.kind == LineKind::Deletion)
            .count()
    }
}

/// A complete diff spanning one or more files.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Diff {
    /// Per-file diffs.
    pub files: Vec<FileDiff>,
}
