//! The `RepoReader` trait and its gitoxide-backed implementation.
//!
//! All read operations (refs, history walk, commit decode, blob access) go
//! through gix. The handle stores a [`gix::ThreadSafeRepository`] and derives a
//! cheap thread-local [`gix::Repository`] per call, so the reader is `Send +
//! Sync` and safe to share across the background executor.

use std::path::{Path, PathBuf};

use gg_core::{CommitMeta, GitError, Oid, RefKind, RefName, RefRecord, Result};

use crate::convert::{to_gix, to_oid, to_signature};

/// Options controlling a history walk.
#[derive(Clone, Debug, Default)]
pub struct WalkOpts {
    /// Commits to start from. Empty means "all refs plus HEAD".
    pub tips: Vec<Oid>,
    /// Maximum number of commits to return (`None` == unbounded).
    pub limit: Option<usize>,
    /// Follow only first parents.
    pub first_parent: bool,
}

/// The read side of a git repository.
///
/// Commits are returned newest-first by commit time. Because committer-date
/// order is not guaranteed to be a valid topological order, callers that feed a
/// graph layout should normalize with `gg_graph::topo_order` first.
pub trait RepoReader: Send + Sync {
    /// The working-tree root, if this is not a bare repo.
    fn workdir(&self) -> Option<PathBuf>;
    /// The current HEAD as a ref record (branch, or detached HEAD), if born.
    fn head(&self) -> Result<Option<RefRecord>>;
    /// All references (branches, remotes, tags), peeled to commits.
    fn refs(&self) -> Result<Vec<RefRecord>>;
    /// Walk history from the given tips (or all refs) newest-first.
    fn walk(&self, opts: &WalkOpts) -> Result<Vec<CommitMeta>>;
    /// Decode a single commit's metadata.
    fn commit(&self, oid: Oid) -> Result<CommitMeta>;
    /// Read a file's bytes as of a commit, or `None` if absent at that path.
    fn read_blob_at(&self, commit: Oid, path: &Path) -> Result<Option<Vec<u8>>>;
}

/// `RepoReader` backed by gitoxide.
#[derive(Clone)]
pub struct GixRepo {
    repo: gix::ThreadSafeRepository,
}

impl GixRepo {
    /// Open a repository at an exact path.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let repo = gix::ThreadSafeRepository::open(path.into())
            .map_err(|e| GitError::NotARepository(e.to_string()))?;
        Ok(Self { repo })
    }

    /// Discover a repository at or above `path`.
    pub fn discover(path: impl AsRef<Path>) -> Result<Self> {
        let repo = gix::ThreadSafeRepository::discover(path.as_ref())
            .map_err(|e| GitError::NotARepository(e.to_string()))?;
        Ok(Self { repo })
    }

    fn local(&self) -> gix::Repository {
        self.repo.to_thread_local()
    }
}

fn decode_commit(commit: &gix::Commit<'_>) -> Result<CommitMeta> {
    let oid = to_oid(commit.id().detach());
    let parents = commit.parent_ids().map(|i| to_oid(i.detach())).collect();
    let author = to_signature(commit.author().map_err(other)?);
    let committer = to_signature(commit.committer().map_err(other)?);
    let summary = commit
        .message()
        .map(|m| m.summary().to_string())
        .unwrap_or_default();
    let message = commit.message_raw_sloppy().to_string();
    Ok(CommitMeta {
        oid,
        parents,
        author,
        committer,
        summary,
        message,
    })
}

fn other<E: std::fmt::Display>(e: E) -> GitError {
    GitError::Other(e.to_string())
}

fn classify(name: &gix::refs::FullNameRef) -> RefKind {
    use gix::refs::Category;
    match name.category() {
        Some(Category::LocalBranch) => RefKind::LocalBranch,
        Some(Category::RemoteBranch) => RefKind::RemoteBranch,
        Some(Category::Tag) => RefKind::Tag,
        Some(Category::PseudoRef | Category::MainPseudoRef) => RefKind::Head,
        _ => RefKind::Other,
    }
}

impl RepoReader for GixRepo {
    fn workdir(&self) -> Option<PathBuf> {
        self.local().workdir().map(Path::to_path_buf)
    }

    fn head(&self) -> Result<Option<RefRecord>> {
        let repo = self.local();
        match repo.head_ref().map_err(other)? {
            Some(mut r) => {
                let name = r.name().as_bstr().to_string();
                let target = to_oid(r.peel_to_id().map_err(other)?.detach());
                Ok(Some(RefRecord {
                    name: RefName(name),
                    kind: RefKind::LocalBranch,
                    target,
                    is_head: true,
                }))
            }
            None => match repo.head_id() {
                // Detached HEAD: no symbolic ref, but a concrete commit.
                Ok(id) => Ok(Some(RefRecord {
                    name: RefName("HEAD".to_string()),
                    kind: RefKind::Head,
                    target: to_oid(id.detach()),
                    is_head: true,
                })),
                // Unborn HEAD (fresh repo).
                Err(_) => Ok(None),
            },
        }
    }

    fn refs(&self) -> Result<Vec<RefRecord>> {
        let repo = self.local();
        let head_name = repo
            .head_name()
            .ok()
            .flatten()
            .map(|n| n.as_bstr().to_string());

        let platform = repo.references().map_err(other)?;
        let iter = platform.all().map_err(other)?;
        let mut out = Vec::new();
        for r in iter {
            let mut r = r.map_err(other)?;
            let name = r.name().as_bstr().to_string();
            let kind = classify(r.name());
            // Peel through tags/symrefs to the underlying commit id.
            let target = match r.peel_to_id() {
                Ok(id) => to_oid(id.detach()),
                Err(_) => continue,
            };
            let is_head = head_name.as_deref() == Some(name.as_str());
            out.push(RefRecord {
                name: RefName(name),
                kind,
                target,
                is_head,
            });
        }
        Ok(out)
    }

    fn walk(&self, opts: &WalkOpts) -> Result<Vec<CommitMeta>> {
        let repo = self.local();

        // Resolve starting tips: explicit, else every ref target plus HEAD.
        let tips: Vec<gix::ObjectId> = if opts.tips.is_empty() {
            let mut tips: Vec<gix::ObjectId> = self
                .refs()?
                .into_iter()
                .map(|r| to_gix(r.target))
                .collect::<Result<_>>()?;
            if let Ok(id) = repo.head_id() {
                tips.push(id.detach());
            }
            tips.sort();
            tips.dedup();
            tips
        } else {
            opts.tips
                .iter()
                .map(|o| to_gix(*o))
                .collect::<Result<_>>()?
        };

        if tips.is_empty() {
            return Ok(Vec::new());
        }

        let mut platform = repo
            .rev_walk(tips)
            .sorting(gix::revision::walk::Sorting::ByCommitTime(
                Default::default(),
            ));
        if opts.first_parent {
            platform = platform.first_parent_only();
        }
        let walk = platform.all().map_err(other)?;

        let mut out = Vec::new();
        for item in walk {
            if let Some(limit) = opts.limit {
                if out.len() >= limit {
                    break;
                }
            }
            let info = item.map_err(other)?;
            let commit = info.object().map_err(other)?;
            out.push(decode_commit(&commit)?);
        }
        Ok(out)
    }

    fn commit(&self, oid: Oid) -> Result<CommitMeta> {
        let repo = self.local();
        let commit = repo
            .find_commit(to_gix(oid)?)
            .map_err(|_| GitError::NotFound(format!("commit {}", oid.short(10))))?;
        decode_commit(&commit)
    }

    fn read_blob_at(&self, commit: Oid, path: &Path) -> Result<Option<Vec<u8>>> {
        let repo = self.local();
        let commit = repo
            .find_commit(to_gix(commit)?)
            .map_err(|_| GitError::NotFound(format!("commit {}", commit.short(10))))?;
        let tree = commit.tree().map_err(other)?;
        match tree.lookup_entry_by_path(path).map_err(other)? {
            Some(entry) => {
                let obj = entry.object().map_err(other)?;
                Ok(Some(obj.data.clone()))
            }
            None => Ok(None),
        }
    }
}
