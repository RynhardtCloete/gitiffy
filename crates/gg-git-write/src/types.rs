//! Option structs for write operations.

/// Options for creating a commit.
#[derive(Clone, Debug, Default)]
pub struct CommitOpts {
    /// Replace the tip commit instead of adding a new one (`--amend`).
    pub amend: bool,
    /// Permit a commit with no staged changes (`--allow-empty`).
    pub allow_empty: bool,
    /// Override author as `Name <email>` (`--author`).
    pub author: Option<String>,
    /// GPG/SSH sign the commit (`-S`).
    pub sign: bool,
    /// Stage all tracked modifications first (`-a`).
    pub all: bool,
}

/// Options for merging.
#[derive(Clone, Debug, Default)]
pub struct MergeOpts {
    /// Always create a merge commit (`--no-ff`).
    pub no_ff: bool,
    /// Squash the merged history into the index without committing (`--squash`).
    pub squash: bool,
    /// Custom merge commit message.
    pub message: Option<String>,
}

/// Options for rebasing. Interactive rebase requires a sequence editor and is
/// rejected by the CLI writer (handled by a dedicated flow elsewhere).
#[derive(Clone, Debug, Default)]
pub struct RebaseOpts {
    /// Rebase onto a different base than the upstream (`--onto`).
    pub onto: Option<String>,
    /// Interactive rebase (not supported by the non-interactive writer).
    pub interactive: bool,
}

/// `git reset` mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetMode {
    /// Move HEAD only.
    Soft,
    /// Move HEAD and reset the index (default).
    Mixed,
    /// Move HEAD, reset index and working tree.
    Hard,
}

/// What a stash operation should do.
#[derive(Clone, Debug)]
pub enum StashOp {
    /// Save the working tree to a new stash entry, with an optional message and
    /// whether to include untracked files.
    Push {
        /// Stash description.
        message: Option<String>,
        /// Also stash untracked files (`-u`).
        include_untracked: bool,
    },
    /// Apply and drop the most recent (or indexed) stash entry.
    Pop {
        /// Stash index; `None` == latest.
        index: Option<usize>,
    },
    /// Apply without dropping.
    Apply {
        /// Stash index; `None` == latest.
        index: Option<usize>,
    },
    /// Drop an entry.
    Drop {
        /// Stash index; `None` == latest.
        index: Option<usize>,
    },
}
