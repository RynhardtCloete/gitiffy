//! A terminal renderer for the commit graph.
//!
//! It consumes exactly the same renderer-independent [`gg_core::GraphRow`]
//! output that the GPUI/egui canvases consume, proving the layout engine
//! end-to-end and giving a `git log --graph`-style view for free. Node rows
//! already carry the vertical lanes (`|`), so a connector line is only emitted
//! when the topology shifts (a `/` or `\` is needed).

use std::collections::HashMap;

use gg_core::graph::SegmentKind;
use gg_core::{CommitMeta, Oid, RefKind, RefRecord};
use gg_git::HistoryView;

/// Build a map from commit oid to a decorated ref label list.
pub fn ref_labels(refs: &[RefRecord]) -> HashMap<Oid, Vec<String>> {
    let mut map: HashMap<Oid, Vec<String>> = HashMap::new();
    for r in refs {
        let label = match r.kind {
            RefKind::Tag => format!("tag: {}", r.name.short()),
            _ => r.name.short().to_string(),
        };
        let label = if r.is_head {
            format!("HEAD -> {label}")
        } else {
            label
        };
        map.entry(r.target).or_default().push(label);
    }
    map
}

/// Render the history view as an ASCII graph.
pub fn render(view: &HistoryView, refs: &HashMap<Oid, Vec<String>>) -> String {
    let rows = view.layout.rows();
    let width = view.layout.max_width().max(1);
    let gutter = width * 2;
    let mut out = String::new();

    for (i, row) in rows.iter().enumerate() {
        // --- node line ---
        let mut cells = vec![' '; gutter];
        for seg in &row.segments {
            if seg.kind == SegmentKind::Passthrough {
                cells[seg.from_lane * 2] = '|';
            }
        }
        cells[row.node_lane * 2] = '*';
        let graph: String = cells.iter().collect();

        let commit = &view.commits[i];
        out.push_str(&graph);
        out.push_str("  ");
        out.push_str(&commit.oid.short(8));
        if let Some(labels) = refs.get(&commit.oid) {
            if !labels.is_empty() {
                out.push_str(&format!(" ({})", labels.join(", ")));
            }
        }
        out.push(' ');
        out.push_str(&first_line(commit));
        out.push('\n');

        // --- connector line (only when lanes shift) ---
        if i + 1 < rows.len() {
            if let Some(conn) = connector(row, gutter) {
                out.push_str(&conn);
                out.push('\n');
            }
        }
    }

    out
}

fn first_line(commit: &CommitMeta) -> String {
    let s = if commit.summary.is_empty() {
        commit.message.lines().next().unwrap_or("")
    } else {
        &commit.summary
    };
    s.trim().to_string()
}

/// Produce a connector line if this row's outgoing edges include a diagonal.
fn connector(row: &gg_core::GraphRow, gutter: usize) -> Option<String> {
    let mut cells = vec![' '; gutter];
    let mut has_diagonal = false;

    // Passthrough lanes continue straight down. A branch-out that stays in the
    // node lane (the first parent) also continues straight; branch-outs to a
    // different lane are drawn only as a diagonal — the new lane's vertical bar
    // begins on the next node row.
    for seg in &row.segments {
        match seg.kind {
            SegmentKind::Passthrough => cells[seg.from_lane * 2] = '|',
            SegmentKind::BranchOut if seg.to_lane == row.node_lane => {
                cells[row.node_lane * 2] = '|'
            }
            _ => {}
        }
    }

    // Diagonals hint the direction of branch/merge transitions.
    for seg in &row.segments {
        if seg.kind == SegmentKind::BranchOut && seg.to_lane != row.node_lane {
            has_diagonal = true;
            if seg.to_lane > row.node_lane {
                let col = row.node_lane * 2 + 1;
                if col < gutter {
                    cells[col] = '\\';
                }
            } else if row.node_lane * 2 >= 1 {
                cells[row.node_lane * 2 - 1] = '/';
            }
        }
    }

    if has_diagonal {
        // Trim trailing spaces for tidiness.
        let s: String = cells.iter().collect();
        Some(s.trim_end().to_string())
    } else {
        None
    }
}
