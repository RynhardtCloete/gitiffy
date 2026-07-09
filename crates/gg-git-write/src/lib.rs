//! `gg-git-write` — the write path. A thin, hardened wrapper around the system
//! `git` binary, exposed behind the [`RepoWriter`] trait. Following GitButler
//! and Jujutsu, all mutating and network operations shell out to git so the
//! user's credential helpers, hooks, and config behave exactly as on the CLI.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod cancel;
pub mod git;
pub mod progress;
pub mod status;
pub mod types;
pub mod writer;

pub use cancel::CancelToken;
pub use git::{find_git, Git};
pub use progress::{parse_progress_line, FnSink, NullSink, Progress, ProgressSink};
pub use types::{CommitOpts, MergeOpts, RebaseOpts, ResetMode, StashOp};
pub use writer::{GitWriter, RepoWriter};
