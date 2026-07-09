//! Unit tests for the layout engine.
//!
//! Commits are referenced by small integer ids mapped to distinct oids. Tests
//! assert both concrete lane/segment topology for hand-checkable graphs and
//! structural invariants for trickier shapes (octopus, slot reuse).

use gg_core::graph::SegmentKind;
use gg_core::{CommitMeta, Oid, Signature, Time};

use crate::layout::{CommitInput, GraphLayout};
use crate::topo::topo_order;

/// Distinct oid for a small integer id.
fn oid(n: u32) -> Oid {
    let mut b = [0u8; 20];
    b[0..4].copy_from_slice(&n.to_be_bytes());
    // Make the tail non-uniform so short() looks oid-ish; not required.
    b[19] = n as u8;
    Oid::from_bytes(&b).unwrap()
}

/// `ci(child, &[parents])` using integer ids.
fn ci(id: u32, parents: &[u32]) -> CommitInput {
    CommitInput::new(oid(id), parents.iter().copied().map(oid).collect())
}

fn node_lanes(layout: &GraphLayout) -> Vec<usize> {
    layout.rows().iter().map(|r| r.node_lane).collect()
}

fn count_kind(layout: &GraphLayout, row: usize, kind: SegmentKind) -> usize {
    layout.rows()[row].segments_of(kind).count()
}

#[test]
fn linear_history_is_one_lane() {
    // 1 -> 2 -> 3 (newest first), 3 is root.
    let commits = [ci(1, &[2]), ci(2, &[3]), ci(3, &[])];
    let layout = GraphLayout::from_commits(&commits);

    assert_eq!(layout.len(), 3);
    assert_eq!(node_lanes(&layout), vec![0, 0, 0]);
    assert_eq!(layout.max_width(), 1);

    // Root has no outgoing branch.
    assert_eq!(count_kind(&layout, 2, SegmentKind::BranchOut), 0);
    // Middle commit merges in from above and branches out below.
    assert_eq!(count_kind(&layout, 1, SegmentKind::MergeIn), 1);
    assert_eq!(count_kind(&layout, 1, SegmentKind::BranchOut), 1);
}

#[test]
fn feature_branch_and_merge() {
    // M(merge of A,B) -> A -> B -> C(root); A and B share parent C.
    //   M:1 parents [A:2, B:3]
    //   A:2 parent [C:4]
    //   B:3 parent [C:4]
    //   C:4 root
    let commits = [ci(1, &[2, 3]), ci(2, &[4]), ci(3, &[4]), ci(4, &[])];
    let layout = GraphLayout::from_commits(&commits);

    // Merge commit fans out to two lanes.
    assert_eq!(node_lanes(&layout)[0], 0);
    assert_eq!(count_kind(&layout, 0, SegmentKind::BranchOut), 2);
    assert_eq!(layout.rows()[0].lanes, 2);

    // B (row 2) reuses C's existing lane (convergence reuse), so the graph
    // never exceeds two lanes wide.
    assert_eq!(layout.max_width(), 2);

    // C (root, last row) collects both branches: it is reached via one lane and
    // has at least one merge-in, no branch-out.
    let last = layout.len() - 1;
    assert_eq!(count_kind(&layout, last, SegmentKind::BranchOut), 0);
    assert!(count_kind(&layout, last, SegmentKind::MergeIn) >= 1);
}

#[test]
fn octopus_merge_fans_out_to_all_parents() {
    // Octopus: commit with three parents.
    let commits = [
        ci(1, &[2, 3, 4]),
        ci(2, &[5]),
        ci(3, &[5]),
        ci(4, &[5]),
        ci(5, &[]),
    ];
    let layout = GraphLayout::from_commits(&commits);

    // Three outgoing edges from the octopus node.
    assert_eq!(count_kind(&layout, 0, SegmentKind::BranchOut), 3);
    // Needs at least three lanes at the merge row.
    assert_eq!(layout.rows()[0].lanes, 3);
    // All three branches converge back at the shared root (row 4).
    assert!(count_kind(&layout, 4, SegmentKind::MergeIn) >= 1);
}

#[test]
fn freed_lanes_are_reused() {
    // A short branch that ends should free its lane for a later branch rather
    // than pushing the graph ever wider.
    //
    //   1 -> 2 (root)              main line
    //   plus an independent tip 3 -> 2 that ends quickly
    //   then another independent tip 4 -> 2
    // Order: 1, 3, 4, 2
    let commits = [ci(1, &[2]), ci(3, &[2]), ci(4, &[2]), ci(2, &[])];
    let layout = GraphLayout::from_commits(&commits);

    // All three tips share parent 2, which they converge into via reuse.
    // Width should stay bounded (<= number of simultaneously-live lanes).
    assert!(layout.max_width() <= 3);
    // The shared parent is the final row and only merges in.
    let last = layout.len() - 1;
    assert_eq!(count_kind(&layout, last, SegmentKind::BranchOut), 0);
}

#[test]
fn incremental_extend_matches_single_pass() {
    let commits = [
        ci(1, &[2, 3]),
        ci(2, &[4]),
        ci(3, &[4]),
        ci(4, &[5]),
        ci(5, &[]),
    ];

    let full = GraphLayout::from_commits(&commits);

    let mut incremental = GraphLayout::new();
    for c in &commits {
        incremental.extend(std::slice::from_ref(c));
    }

    assert_eq!(full.rows(), incremental.rows());
}

#[test]
fn every_node_lane_is_within_reported_width() {
    let commits = [
        ci(1, &[2, 3]),
        ci(2, &[4]),
        ci(3, &[6]),
        ci(4, &[5]),
        ci(6, &[5]),
        ci(5, &[]),
    ];
    let layout = GraphLayout::from_commits(&commits);
    for r in layout.rows() {
        assert!(
            r.node_lane < r.lanes,
            "node lane escapes width on row {}",
            r.row
        );
        for s in &r.segments {
            assert!(s.from_lane < r.lanes);
            assert!(s.to_lane < r.lanes);
        }
    }
}

// ---- topo ordering ----

fn sig(seconds: i64) -> Signature {
    Signature {
        name: "t".into(),
        email: "t@e".into(),
        time: Time::new(seconds, 0),
    }
}

fn meta(id: u32, parents: &[u32], seconds: i64) -> CommitMeta {
    CommitMeta {
        oid: oid(id),
        parents: parents.iter().copied().map(oid).collect(),
        author: sig(seconds),
        committer: sig(seconds),
        summary: format!("commit {id}"),
        message: format!("commit {id}"),
    }
}

#[test]
fn topo_order_places_children_before_parents() {
    // Supplied in a deliberately scrambled order.
    let commits = vec![
        meta(4, &[], 100),
        meta(1, &[2, 3], 400),
        meta(3, &[4], 200),
        meta(2, &[4], 300),
    ];
    let ordered = topo_order(&commits);

    let pos = |id: u32| ordered.iter().position(|c| c.oid == oid(id)).unwrap();
    // Every commit must come before each of its parents.
    assert!(pos(1) < pos(2));
    assert!(pos(1) < pos(3));
    assert!(pos(2) < pos(4));
    assert!(pos(3) < pos(4));
    // The result lays out cleanly (no panics, bounded width).
    let layout = GraphLayout::from_commits(&ordered);
    assert_eq!(layout.len(), 4);
}
