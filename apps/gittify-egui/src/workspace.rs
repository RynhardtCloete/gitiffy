//! The workspace tree: nestable groups of open repository tabs.
//!
//! A [`WsNode`] holds repo tabs (paths) and/or child workspaces, so a "folder"
//! is just a node used for grouping. The [`WorkspaceStore`] owns the roots and
//! tracks which node is active (its tabs are what the window shows). The tree
//! ops are pure functions so they can be unit-tested without any UI.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One workspace: a name, its open repo tabs, and nested child workspaces.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WsNode {
    /// Stable unique id (unique across the whole tree).
    pub id: u64,
    /// Display name.
    pub name: String,
    /// Repo tabs open in this workspace, in tab order.
    #[serde(default)]
    pub repos: Vec<PathBuf>,
    /// Repos available in this workspace's library (shown on the landing
    /// page; a superset of the open tabs). The same path may appear in
    /// several workspaces' libraries.
    #[serde(default)]
    pub library: Vec<PathBuf>,
    /// Nested child workspaces.
    #[serde(default)]
    pub children: Vec<WsNode>,
    /// Index of the active tab within `repos`.
    #[serde(default)]
    pub active_tab: usize,
    /// Whether this node is expanded in tree views.
    #[serde(default = "yes")]
    pub expanded: bool,
}

fn yes() -> bool {
    true
}

impl WsNode {
    /// A new empty workspace node with the given id and name.
    pub fn new(id: u64, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            repos: Vec::new(),
            library: Vec::new(),
            children: Vec::new(),
            active_tab: 0,
            expanded: true,
        }
    }

    /// Add `path` to this workspace's library (deduplicated).
    pub fn add_to_library(&mut self, path: &Path) {
        if !self.library.iter().any(|p| p == path) {
            self.library.push(path.to_path_buf());
        }
    }
}

/// The persisted workspace tree plus the active-node pointer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceStore {
    /// Id of the active workspace (its tabs are shown).
    pub active: u64,
    /// Top-level workspaces.
    pub roots: Vec<WsNode>,
    /// Next id to hand out (kept ahead of every existing id).
    #[serde(default)]
    pub next_id: u64,
    /// Most-recently-opened repos across all workspaces, newest first.
    #[serde(default)]
    pub recent: Vec<PathBuf>,
}

impl Default for WorkspaceStore {
    fn default() -> Self {
        Self::with_repos(Vec::new())
    }
}

impl WorkspaceStore {
    /// A fresh store with a single "Workspace" holding `repos`.
    pub fn with_repos(repos: Vec<PathBuf>) -> Self {
        let mut root = WsNode::new(1, "Workspace");
        root.library = repos.clone();
        root.repos = repos;
        Self {
            active: 1,
            roots: vec![root],
            next_id: 2,
            recent: Vec::new(),
        }
    }

    /// Allocate a fresh id (always greater than every id currently in the tree).
    pub fn next_id(&mut self) -> u64 {
        let id = self.next_id.max(max_id(&self.roots) + 1);
        self.next_id = id + 1;
        id
    }

    /// Repair invariants after loading: ensure `next_id` leads all ids and
    /// `active` points at a node that exists (else the first root, creating one
    /// if the tree is empty).
    pub fn normalize(&mut self) {
        if self.roots.is_empty() {
            self.roots.push(WsNode::new(1, "Workspace"));
        }
        self.next_id = self.next_id.max(max_id(&self.roots) + 1);
        if find(&self.roots, self.active).is_none() {
            self.active = self.roots[0].id;
        }
        // Stores from before the library existed: every open tab is at least
        // available in its workspace's library.
        seed_library(&mut self.roots);
    }

    /// How many recently-opened entries are kept.
    pub const MAX_RECENT: usize = 10;

    /// Record `path` as the most recently opened repo (dedupes, caps).
    pub fn touch_recent(&mut self, path: &Path) {
        self.recent.retain(|p| p != path);
        self.recent.insert(0, path.to_path_buf());
        self.recent.truncate(Self::MAX_RECENT);
    }

    pub fn find(&self, id: u64) -> Option<&WsNode> {
        find(&self.roots, id)
    }

    pub fn find_mut(&mut self, id: u64) -> Option<&mut WsNode> {
        find_mut(&mut self.roots, id)
    }

    /// The active workspace node.
    pub fn active_node(&self) -> Option<&WsNode> {
        self.find(self.active)
    }

    pub fn active_node_mut(&mut self) -> Option<&mut WsNode> {
        let id = self.active;
        self.find_mut(id)
    }

    /// Remove the node `id` from wherever it lives and return it (with its
    /// subtree). Returns `None` if not found.
    pub fn remove(&mut self, id: u64) -> Option<WsNode> {
        remove(&mut self.roots, id)
    }

    /// Insert `node` under `parent` (or at the top level when `parent` is
    /// `None`) at `index`, clamped to the destination length.
    pub fn insert(&mut self, parent: Option<u64>, index: usize, node: WsNode) {
        match parent {
            None => {
                let i = index.min(self.roots.len());
                self.roots.insert(i, node);
            }
            Some(pid) => {
                if let Some(p) = find_mut(&mut self.roots, pid) {
                    let i = index.min(p.children.len());
                    p.children.insert(i, node);
                } else {
                    self.roots.push(node);
                }
            }
        }
    }

    /// True if `id` is within the subtree rooted at `ancestor` (inclusive). Used
    /// to forbid dropping a node into its own descendant during drag-and-drop.
    pub fn is_descendant(&self, ancestor: u64, id: u64) -> bool {
        find(&self.roots, ancestor).is_some_and(|a| contains(a, id))
    }

    /// The parent id (`None` = top level) and sibling index of `id`.
    pub fn locate(&self, id: u64) -> Option<(Option<u64>, usize)> {
        locate(&self.roots, None, id)
    }

    /// Move `id` so it sits under `new_parent` at sibling position `index`,
    /// where `index` is interpreted against the sibling list as it looks
    /// *before* the move (so reordering within one parent lands where the
    /// user aimed). No-op on cycles or a missing node.
    pub fn move_to(&mut self, id: u64, new_parent: Option<u64>, mut index: usize) {
        if let Some(p) = new_parent {
            if p == id || self.is_descendant(id, p) {
                return; // cycle
            }
        }
        if let Some((old_parent, old_index)) = self.locate(id) {
            // Removing the node first shifts later siblings down by one.
            if old_parent == new_parent && old_index < index {
                index -= 1;
            }
        }
        if let Some(node) = self.remove(id) {
            self.insert(new_parent, index, node);
        }
    }

    /// True if any workspace in the tree still references `path` as a tab.
    pub fn references(&self, path: &Path) -> bool {
        references(&self.roots, path)
    }
}

fn max_id(nodes: &[WsNode]) -> u64 {
    nodes
        .iter()
        .map(|n| n.id.max(max_id(&n.children)))
        .max()
        .unwrap_or(0)
}

fn find(nodes: &[WsNode], id: u64) -> Option<&WsNode> {
    for n in nodes {
        if n.id == id {
            return Some(n);
        }
        if let Some(f) = find(&n.children, id) {
            return Some(f);
        }
    }
    None
}

fn find_mut(nodes: &mut [WsNode], id: u64) -> Option<&mut WsNode> {
    for n in nodes.iter_mut() {
        if n.id == id {
            return Some(n);
        }
        if let Some(f) = find_mut(&mut n.children, id) {
            return Some(f);
        }
    }
    None
}

fn seed_library(nodes: &mut [WsNode]) {
    for n in nodes {
        let open: Vec<PathBuf> = n.repos.clone();
        for path in open {
            n.add_to_library(&path);
        }
        seed_library(&mut n.children);
    }
}

fn locate(nodes: &[WsNode], parent: Option<u64>, id: u64) -> Option<(Option<u64>, usize)> {
    for (i, n) in nodes.iter().enumerate() {
        if n.id == id {
            return Some((parent, i));
        }
        if let Some(found) = locate(&n.children, Some(n.id), id) {
            return Some(found);
        }
    }
    None
}

fn remove(nodes: &mut Vec<WsNode>, id: u64) -> Option<WsNode> {
    if let Some(pos) = nodes.iter().position(|n| n.id == id) {
        return Some(nodes.remove(pos));
    }
    for n in nodes.iter_mut() {
        if let Some(found) = remove(&mut n.children, id) {
            return Some(found);
        }
    }
    None
}

fn contains(node: &WsNode, id: u64) -> bool {
    node.id == id || node.children.iter().any(|c| contains(c, id))
}

fn references(nodes: &[WsNode], path: &Path) -> bool {
    nodes
        .iter()
        .any(|n| n.repos.iter().any(|p| p == path) || references(&n.children, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> WorkspaceStore {
        // root(1) -> [a(2) -> [c(4)], b(3)]
        let mut store = WorkspaceStore {
            active: 1,
            roots: vec![WsNode {
                children: vec![
                    WsNode {
                        children: vec![WsNode::new(4, "c")],
                        ..WsNode::new(2, "a")
                    },
                    WsNode::new(3, "b"),
                ],
                ..WsNode::new(1, "root")
            }],
            next_id: 0,
            recent: Vec::new(),
        };
        store.normalize();
        store
    }

    #[test]
    fn find_and_next_id() {
        let mut store = sample();
        assert_eq!(store.find(4).unwrap().name, "c");
        assert!(store.find(99).is_none());
        // next_id leads all existing ids (max was 4).
        assert_eq!(store.next_id(), 5);
        assert_eq!(store.next_id(), 6);
    }

    #[test]
    fn remove_and_insert() {
        let mut store = sample();
        let c = store.remove(4).expect("remove c");
        assert_eq!(c.name, "c");
        assert!(store.find(4).is_none());
        store.insert(Some(3), 0, c); // move c under b(3)
        assert_eq!(store.find(3).unwrap().children[0].id, 4);
    }

    #[test]
    fn descendant_and_cycle_guard() {
        let mut store = sample();
        assert!(store.is_descendant(2, 4)); // c is under a
        assert!(store.is_descendant(1, 4));
        assert!(!store.is_descendant(3, 4));
        // Reparenting a node into its own descendant is a no-op (no cycle).
        store.move_to(2, Some(4), 0);
        assert!(store.find(2).is_some());
        assert!(store.is_descendant(2, 4), "tree must be unchanged");
    }

    #[test]
    fn reparent_moves_subtree() {
        let mut store = sample();
        store.move_to(2, Some(3), 0); // move a (with c) under b
        assert!(store.find(3).unwrap().children.iter().any(|n| n.id == 2));
        assert!(store.is_descendant(2, 4)); // c still under a
        assert_eq!(store.roots[0].children.len(), 1); // only b remains at root level
    }

    #[test]
    fn locate_finds_parent_and_index() {
        let store = sample();
        assert_eq!(store.locate(1), Some((None, 0)));
        assert_eq!(store.locate(2), Some((Some(1), 0)));
        assert_eq!(store.locate(3), Some((Some(1), 1)));
        assert_eq!(store.locate(4), Some((Some(2), 0)));
        assert_eq!(store.locate(99), None);
    }

    #[test]
    fn move_to_reorders_within_a_parent() {
        // Move a(2) after b(3): aiming at pre-move index 2 lands it last.
        let mut store = sample();
        store.move_to(2, Some(1), 2);
        let kids: Vec<u64> = store
            .find(1)
            .unwrap()
            .children
            .iter()
            .map(|n| n.id)
            .collect();
        assert_eq!(kids, vec![3, 2]);
        // And back before b(3).
        store.move_to(2, Some(1), 0);
        let kids: Vec<u64> = store
            .find(1)
            .unwrap()
            .children
            .iter()
            .map(|n| n.id)
            .collect();
        assert_eq!(kids, vec![2, 3]);
    }

    #[test]
    fn move_to_promotes_to_top_level_at_position() {
        let mut store = sample();
        store.move_to(4, None, 0); // c out of a, to the front of the roots
        assert_eq!(store.roots[0].id, 4);
        assert!(store.find(2).unwrap().children.is_empty());
        store.move_to(4, Some(1), usize::MAX); // and back under root, last
        assert_eq!(store.find(1).unwrap().children.last().unwrap().id, 4);
    }

    #[test]
    fn move_to_refuses_cycles() {
        let mut store = sample();
        store.move_to(2, Some(4), 0); // a into its own descendant c
        assert_eq!(store.locate(2), Some((Some(1), 0)), "tree unchanged");
    }

    #[test]
    fn touch_recent_dedupes_orders_and_caps() {
        let mut store = WorkspaceStore::default();
        for i in 0..12 {
            store.touch_recent(Path::new(&format!("/r{i}")));
        }
        assert_eq!(store.recent.len(), WorkspaceStore::MAX_RECENT);
        assert_eq!(store.recent[0], PathBuf::from("/r11"));
        // Re-touching moves to the front without duplicating.
        store.touch_recent(Path::new("/r5"));
        assert_eq!(store.recent[0], PathBuf::from("/r5"));
        assert_eq!(
            store
                .recent
                .iter()
                .filter(|p| p.as_path() == Path::new("/r5"))
                .count(),
            1
        );
    }

    #[test]
    fn normalize_seeds_library_from_open_tabs() {
        let mut store = WorkspaceStore::with_repos(vec![PathBuf::from("/x")]);
        store.roots[0].library.clear(); // simulate a pre-library store
        store.roots[0].children.push(WsNode {
            repos: vec![PathBuf::from("/y")],
            ..WsNode::new(7, "child")
        });
        store.normalize();
        assert!(store.roots[0].library.contains(&PathBuf::from("/x")));
        assert!(store.roots[0].children[0]
            .library
            .contains(&PathBuf::from("/y")));
        // Idempotent: normalizing again doesn't duplicate.
        store.normalize();
        assert_eq!(store.roots[0].library.len(), 1);
    }

    #[test]
    fn references_path() {
        let mut store = WorkspaceStore::with_repos(vec![PathBuf::from("/x")]);
        assert!(store.references(Path::new("/x")));
        assert!(!store.references(Path::new("/y")));
        store.roots[0].repos.clear();
        assert!(!store.references(Path::new("/x")));
    }

    #[test]
    fn json_round_trip() {
        let store = sample();
        let json = serde_json::to_string(&store).unwrap();
        let back: WorkspaceStore = serde_json::from_str(&json).unwrap();
        assert_eq!(store, back);
    }

    #[test]
    fn normalize_fixes_active_and_empty() {
        let mut store = WorkspaceStore {
            active: 999,
            roots: vec![],
            next_id: 0,
            recent: Vec::new(),
        };
        store.normalize();
        assert!(!store.roots.is_empty());
        assert_eq!(store.active, store.roots[0].id);
    }
}
