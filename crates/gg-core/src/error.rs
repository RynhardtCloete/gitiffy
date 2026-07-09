//! Shared error types for the core domain.

use thiserror::Error;

/// Failure parsing an [`crate::Oid`] from bytes or hex.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseOidError {
    /// The input was not a valid git hash length (20 or 32 bytes / 40 or 64 hex chars).
    #[error("invalid oid length: {0}")]
    BadLength(usize),
    /// A non-hex character was encountered.
    #[error("invalid hex character: {0:?}")]
    BadChar(char),
}

/// A generic, backend-agnostic git error surfaced to the application layer.
///
/// Concrete backends (`gg-git-read`, `gg-git-write`) map their own errors into
/// this so `gg-app` and the UI never depend on `gix`/`std::process` error types.
#[derive(Debug, Error)]
pub enum GitError {
    /// No repository could be discovered at or above the given path.
    #[error("not a git repository: {0}")]
    NotARepository(String),
    /// A referenced object/ref/path did not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// The system `git` binary could not be located.
    #[error("git executable not found on PATH")]
    GitBinaryMissing,
    /// A shelled-out git command failed; carries the exit status and stderr.
    #[error("git command failed ({code}): {stderr}")]
    CommandFailed {
        /// Exit code, or -1 if terminated by a signal.
        code: i32,
        /// Captured stderr (already locale-normalized).
        stderr: String,
    },
    /// The operation was cancelled via a cancellation token.
    #[error("operation cancelled")]
    Cancelled,
    /// Authentication/credentials were required but unavailable.
    #[error("authentication required for {0}")]
    AuthRequired(String),
    /// Any other backend error, preserved as a message.
    #[error("{0}")]
    Other(String),
}

impl From<ParseOidError> for GitError {
    fn from(e: ParseOidError) -> Self {
        GitError::Other(format!("oid parse error: {e}"))
    }
}

/// Convenience result alias for git operations.
pub type Result<T> = std::result::Result<T, GitError>;
