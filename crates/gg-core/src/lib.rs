//! `gg-core` — renderer- and IO-free domain types shared across the gittify
//! workspace.
//!
//! Nothing in this crate performs IO or depends on a git backend or UI toolkit.
//! It is the common vocabulary every other crate speaks: object ids, commit
//! metadata, refs, diffs, working-tree status, and the renderer-independent
//! commit-graph layout primitives.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod commit;
pub mod diff;
pub mod error;
pub mod graph;
pub mod oid;
pub mod refs;
pub mod status;

pub use commit::{CommitMeta, Signature, Time};
pub use diff::{Diff, DiffLine, FileChange, FileDiff, Hunk, LineKind, TokenSpan};
pub use error::{GitError, ParseOidError, Result};
pub use graph::{GraphRow, Segment, SegmentKind};
pub use oid::Oid;
pub use refs::{RefKind, RefName, RefRecord};
pub use status::{ChangeKind, Remote, StashEntry, StatusEntry, StatusSnapshot};
