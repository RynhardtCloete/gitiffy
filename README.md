# gittify

A high-performance, cross-platform Git GUI in Rust. A **hybrid git engine**
(gitoxide for reads, system `git` for writes/network), a **renderer-independent,
virtualized commit-graph layout engine**, and a **swappable UI layer** (GPUI
primary, egui fallback) behind a thin rendering abstraction.

## Status

The renderer-independent core is implemented, compiling, and tested; the GPUI
UI sits behind a Phase 0 decision gate (per the plan) and is scaffolded but not
yet wired.

### Desktop GUI (egui)

A Fork-style desktop app: organize repository folders into **workspaces** shown
as tabs (persisted across launches) and browse each one's commit graph.

```
cargo run -p gittify-egui            # opens the window
```

Repositories are grouped into **workspaces** chosen from the dropdown at the top
right; the active workspace's repos appear as **tabs** below the toolbar. A
pinned **Home** tab (also shown when no repos are open) is the workspace's
landing page: a searchable **repository library** on the left (single-click
previews the repo's README as rendered markdown on the right; double-click
opens it as a tab) and, with nothing selected, quick actions plus the
recently-opened list. The library persists per workspace and the same repo may
live in several workspaces (it opens as the same view). The Repository menu
(and the library's `+`) adds repos four ways: **Add existing** (folder
picker), **Clone** (URL / git URL + destination folder, cloned on a background
thread with progress in the dialog), **New repository** (folder + name,
`git init`ed and opened as a tab), and **Scan a folder** (recursive,
depth-limited walk that finds git repos under a chosen root and bulk-adds the
ones you tick). Network operations honor the user's configured credential helper; when
none answers, the app shows the normal username / password (token) prompt,
routed through the bundled `gg-askpass` helper, and retries the operation.
Those credentials are kept for the session only, never written to disk. Workspaces are **nestable**: "Manage workspaces…" in the dropdown
opens a settings modal where you rename, delete, create, and **drag-and-drop**
workspaces to nest them (drop one onto another, or onto "Top level" to un-nest).
Each workspace remembers its open tabs across switches, and the graph is drawn
through the same `GraphRow` layout + `draw_row` the spec's GPUI backend will use,
on a background worker so the UI stays responsive.

The toolbar carries a **branch menu** (showing the current branch) for checking
out, creating, renaming, and deleting local branches, checking out a remote
branch as a new local tracking branch, deleting tags, and managing remotes
(add / remove), an **Open in** menu (Terminal, Finder / file manager, editor),
and **Fetch**, **Pull**, and **Push** buttons (acting on the current
branch's remote) plus a **Stash** menu to create a stash and apply / pop / drop
existing ones. The left sidebar is collapsible from a button embedded in its
header and carries the view switch (**Local Changes** / **All Commits**) plus
the filterable branches / remotes / tags / stashes tree.

On macOS the app installs a native menu bar (File / Edit / View / Go / Window /
Help) with the standard shortcuts: ⌘O add repository, ⌘W close repository,
⌘1/⌘2 switch views, ⌘B toggle sidebar, ⌘R refresh, ⇧⌘[ / ⇧⌘] switch
repositories, and the system ⌃⌘F fullscreen toggle. On Windows/Linux, F11
toggles fullscreen. Long-running operations (pull/fetch/push show a real progress
bar; merge/rebase/cherry-pick/revert show a busy indicator) display a loading
indicator in the toolbar with a **Cancel** button and a **Details** button that
opens a window streaming git's full output (remote messages, branch updates,
merge summary, and the like).

Each repo has two views (switched from the sidebar):

* **History**: commit graph + log in aligned columns (graph, subject with ref
  pills, author + avatar, date, short SHA). **Click a commit** to open the
  consolidated detail view: the table minifies into a left-hand strip
  (horizontally scrollable for the remaining columns) and the changed files
  fill the rest as a tab strip over a full-size diff; click the commit again
  to restore the full table. **Right-click a commit** to check it out, create a branch or tag
  there, cherry-pick, revert, merge it into the current branch, rebase the
  current branch onto it, reset the current branch to it (soft/mixed/hard), or
  copy its SHA.
* **Changes**: the working tree as staged / unstaged / untracked files. Click a
  file to preview its diff, **double-click** to stage/unstage it, and
  **right-click** for stage / unstage / discard / stash; the per-file
  **checkbox** multi-selects files so those actions apply to the whole
  selection at once. The **diff preview** has intra-line highlighting and
  **per-hunk stage / unstage**, and long lines scroll rather than overflow. A
  **commit box** (title + description) offers **Commit** and **Commit & Push**,
  with **Amend** and **Sign** options. Discard and hard-reset ask for
  confirmation; the lists and graph refresh automatically after each action.

### CLI graph renderer

```
cargo run -p gittify-bin -- /path/to/repo        # render the commit graph
cargo run -p gittify-bin -- . --limit 50         # cap to 50 commits
```

Its output mirrors `git log --graph` topology because it consumes the exact same
`GraphRow` layout the GUI canvases consume.

## Workspace layout

| Crate | Role |
| --- | --- |
| `gg-core` | Domain types (`Oid`, `CommitMeta`, refs, diffs, status, graph rows). No IO. |
| `gg-graph` | **Crown jewel.** Virtualized, incremental lane-assignment + edge-routing engine. Renderer-independent. |
| `gg-diff` | Line + intra-line diffing via `imara-diff`, hunk model. |
| `gg-git-read` | Read path: gitoxide (`gix`) behind the `RepoReader` trait. |
| `gg-git-write` | Write path: system `git` subprocess behind the `RepoWriter` trait (hardened env, progress, cancellation). |
| `gg-git` | Facade composing read + write behind one `GitEngine` (reads→gix, mutations/network→git). |
| `gg-ui-traits` | The rendering abstraction (`GraphCanvas`) + shared `draw_row`. |
| `gg-app` | Application state, background worker, command/event channels. |
| `gg-credentials` | Askpass prompt classification + credential env plumbing. |
| `gg-ui-egui` | egui backend (fallback). Excluded from default build; CI lane keeps it compiling. |
| `gg-ui-gpui` | GPUI backend (primary). Excluded pending the Phase 0 gate. |
| `apps/gittify-bin` | Entry point: selects a UI backend; ships the CLI graph renderer. |
| `apps/gg-askpass` | Tiny helper pointed to by `GIT_ASKPASS`/`SSH_ASKPASS`. |

## Design invariants

- **The UI toolkit and git backend are both swappable.** `gg-graph`/`gg-app`
  never name a GPUI, egui, or `gix` type. Layout is 100% shared; only a small
  `GraphCanvas` bridge differs per backend.
- **gix never leaks past `gg-git-read`**; `std::process` never leaks past
  `gg-git-write`. Pinned exact versions; bump on a schedule (gix has a ~4-week
  break cadence).
- **Virtualization is mandatory.** The layout engine computes and caches only
  the rows scrolled into view; `GraphLayout::extend` pages history in.

## Develop

```
cargo test --workspace           # all unit + integration tests
cargo clippy --workspace --all-targets
cargo build --manifest-path crates/gg-ui-egui/Cargo.toml   # fallback backend
```

CI builds, tests, and lints on Linux/macOS/Windows, keeps the egui fallback
compiling on its own lane, and runs `cargo-deny` for the Apache-2.0 / GPL-3.0
licensing gate the plan flags as legal-sensitive.

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md) for development setup, design
invariants, and the contribution licensing terms (contributors grant the
maintainer relicensing rights, which keeps a future commercial edition
possible).

## License

This project is source-available under the
[PolyForm Noncommercial License 1.0.0](./LICENSE.md). You are free to use,
modify, and share it for any noncommercial purpose, provided you keep the
license and the required copyright notice intact. Commercial use, including
reselling the software or offering it as a paid product or service, is not
permitted without a separate commercial license from the copyright holder.

Required Notice: Copyright Rynhardt Cloete (https://github.com/RynhardtCloete)
