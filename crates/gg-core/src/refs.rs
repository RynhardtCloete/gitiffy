//! References (branches, remote branches, tags, HEAD).

use crate::oid::Oid;

/// The category of a reference, used to drive iconography and grouping in the UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RefKind {
    /// A local branch under `refs/heads/`.
    LocalBranch,
    /// A remote-tracking branch under `refs/remotes/`.
    RemoteBranch,
    /// A tag under `refs/tags/` (annotated or lightweight).
    Tag,
    /// The `HEAD` symbolic ref.
    Head,
    /// Any other ref (stash, notes, custom namespaces).
    Other,
}

/// A fully-qualified reference name (e.g. `refs/heads/main`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RefName(pub String);

impl RefName {
    /// The short, human-facing form (strips the well-known prefixes).
    pub fn short(&self) -> &str {
        let s = &self.0;
        for prefix in ["refs/heads/", "refs/remotes/", "refs/tags/"] {
            if let Some(rest) = s.strip_prefix(prefix) {
                return rest;
            }
        }
        s
    }
}

impl std::fmt::Display for RefName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A resolved reference: its name, kind, and the commit it points at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefRecord {
    /// Fully-qualified name.
    pub name: RefName,
    /// Category.
    pub kind: RefKind,
    /// The commit the ref ultimately resolves to (peeled for tags).
    pub target: Oid,
    /// True if this ref is the current HEAD.
    pub is_head: bool,
}
