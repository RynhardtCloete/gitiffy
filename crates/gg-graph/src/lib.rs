//! `gg-graph` — the renderer-independent, virtualized commit-graph layout
//! engine. The crown jewel of gittify: it turns a topologically ordered commit
//! DAG into per-row lane assignments and edge segments that any UI backend can
//! draw, computing only as much as has been scrolled into view.
//!
//! See [`layout::GraphLayout`] for the engine and [`gg_core::graph`] for the
//! output primitives.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod layout;
pub mod topo;

pub use layout::{CommitInput, GraphLayout, LANE_COLORS};
pub use topo::topo_order;

#[cfg(test)]
mod tests;
