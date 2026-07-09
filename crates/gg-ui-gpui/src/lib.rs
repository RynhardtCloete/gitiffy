//! GPUI implementation of the gittify rendering abstraction — the primary
//! backend.
//!
//! Per the spec this is the highest-risk dependency: GPUI is pre-1.0, primarily
//! developed inside the Zed monorepo, and pinned via a git dependency rather
//! than a stable crates.io release. It is therefore stood up behind a Phase 0
//! decision gate (build + render on all three OSes, plus a `cargo deny`
//! licensing check) before any application code commits to it. The crate is
//! excluded from the default workspace build until that gate passes.
//!
//! The integration shape mirrors `gg-ui-egui`: implement [`gg_ui_traits::GraphCanvas`]
//! over a GPUI custom `Element`'s paint phase, reuse `gg_ui_traits::draw_row`
//! for the graph gutter, and use `gpui-component`'s virtualized `Table` /
//! `VirtualList` for the history list and diff text. Heavy gix reads and git
//! subprocess calls run on `cx.background_executor()`, with results applied on
//! the foreground executor via `cx.notify()` — the same command/event flow
//! `gg_app::AppHandle` already models with channels.

#![forbid(unsafe_code)]

// Intentionally minimal until the Phase 0 GPUI decision gate is passed. The
// canvas bridge will wrap GPUI's paint context the way `EguiCanvas` wraps
// `egui::Painter`; the layout engine and app state need no changes.
