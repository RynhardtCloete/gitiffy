//! `gg-git-read` — the read path. A thin wrapper around gitoxide (`gix`),
//! exposed behind the [`RepoReader`] trait, so the rest of the app depends only
//! on `gg-core` types and can swap the engine (e.g. to git2) per the spec's
//! per-operation fallback strategy.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod convert;
mod reader;

pub use reader::{GixRepo, RepoReader, WalkOpts};
