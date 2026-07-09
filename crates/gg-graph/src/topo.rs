//! Topological ordering helper.
//!
//! A revwalk from `gg-git-read` already yields commits in topo order, so the
//! layout engine normally consumes its output directly. This helper exists for
//! the cases where input arrives unordered (or only partially ordered): it
//! produces a display order in which every commit precedes all of its parents,
//! using committer date (newest first) as the tiebreak between otherwise-equal
//! candidates — matching git's `--topo-order` intent.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

use gg_core::{CommitMeta, Oid};

use crate::layout::CommitInput;

/// Heap entry ordered by committer time descending, then by activation
/// sequence (most recently made ready first), then oid for full determinism.
///
/// The activation-sequence tiebreak is what keeps a topic branch's commits
/// contiguous instead of interleaving with equal-timestamped commits on other
/// branches: when emitting a commit makes a parent ready, that parent gets the
/// highest sequence and is preferred next, so the walk follows the branch down.
struct Candidate {
    seconds: i64,
    seq: u64,
    oid: Oid,
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.seconds == other.seconds && self.seq == other.seq && self.oid == other.oid
    }
}
impl Eq for Candidate {}
impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a max-heap: newest committer time pops first; ties break
        // toward the most-recently-activated commit, then on oid.
        self.seconds
            .cmp(&other.seconds)
            .then_with(|| self.seq.cmp(&other.seq))
            .then_with(|| self.oid.cmp(&other.oid))
    }
}
impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Order commits topologically (each commit before its parents) with a
/// newest-committer-date tiebreak. Parents not present in `commits` are ignored
/// (the history window may be truncated).
pub fn topo_order(commits: &[CommitMeta]) -> Vec<CommitInput> {
    let present: HashMap<Oid, &CommitMeta> = commits.iter().map(|c| (c.oid, c)).collect();

    // child_count[oid] = how many in-window commits list oid as a parent. A
    // commit can only be emitted once all of its children have been emitted.
    let mut child_count: HashMap<Oid, usize> = commits.iter().map(|c| (c.oid, 0usize)).collect();
    for c in commits {
        for p in &c.parents {
            if let Some(n) = child_count.get_mut(p) {
                *n += 1;
            }
        }
    }

    // Monotonic activation counter; higher == made ready more recently.
    let mut seq: u64 = 0;
    let mut heap: BinaryHeap<Candidate> = BinaryHeap::new();
    for c in commits {
        if child_count[&c.oid] == 0 {
            heap.push(Candidate {
                seconds: c.committer.time.seconds,
                seq,
                oid: c.oid,
            });
            seq += 1;
        }
    }

    let mut order = Vec::with_capacity(commits.len());
    while let Some(Candidate { oid, .. }) = heap.pop() {
        let commit = present[&oid];
        order.push(CommitInput::new(oid, commit.parents.clone()));
        for p in &commit.parents {
            if let Some(n) = child_count.get_mut(p) {
                *n -= 1;
                if *n == 0 {
                    let pc = present[p];
                    heap.push(Candidate {
                        seconds: pc.committer.time.seconds,
                        seq,
                        oid: pc.oid,
                    });
                    seq += 1;
                }
            }
        }
    }

    order
}
