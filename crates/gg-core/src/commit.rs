//! Commit metadata and authorship types.

use crate::oid::Oid;

/// A point in time expressed as a Unix timestamp plus its UTC offset, mirroring
/// how git stores author/committer dates (`<seconds> <±HHMM>`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Time {
    /// Seconds since the Unix epoch.
    pub seconds: i64,
    /// Offset from UTC in minutes (can be negative).
    pub offset_minutes: i32,
}

impl Time {
    /// Construct a time from epoch seconds and a UTC offset in minutes.
    pub fn new(seconds: i64, offset_minutes: i32) -> Self {
        Self {
            seconds,
            offset_minutes,
        }
    }
}

/// A git identity (the name/email pair attached to authorship and committership).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    /// Display name.
    pub name: String,
    /// Email address.
    pub email: String,
    /// When the action occurred.
    pub time: Time,
}

/// Decoded metadata for a single commit. This is the columnar-cache-friendly
/// shape the graph engine and history list consume; it never holds gix types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitMeta {
    /// This commit's id.
    pub oid: Oid,
    /// Parent commit ids, in order (first parent first). Empty for root commits;
    /// more than two entries denotes an octopus merge.
    pub parents: Vec<Oid>,
    /// Who wrote the change.
    pub author: Signature,
    /// Who committed it (may differ after rebase/cherry-pick/amend).
    pub committer: Signature,
    /// First line of the commit message.
    pub summary: String,
    /// Full commit message body (including the summary line).
    pub message: String,
}

impl CommitMeta {
    /// True if this commit has more than one parent (a merge).
    pub fn is_merge(&self) -> bool {
        self.parents.len() > 1
    }

    /// True if this commit has no parents (a root).
    pub fn is_root(&self) -> bool {
        self.parents.is_empty()
    }
}
