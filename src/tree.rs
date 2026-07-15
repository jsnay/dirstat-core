//! In-memory directory tree (CORE-TREE-*).
//!
//! The tree is an arena of nodes indexed by [`NodeId`]. Hosts hold `NodeId`s
//! (opaque `u64` across FFI) and pull data lazily; the tree itself never
//! crosses the FFI boundary.

use std::ffi::OsString;
use std::time::SystemTime;

/// Stable opaque node handle. `0` is reserved as "invalid"; the root of a
/// model is always `NodeId(1)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u64);

impl NodeId {
    pub const INVALID: NodeId = NodeId(0);
    pub const ROOT: NodeId = NodeId(1);

    #[inline]
    pub fn index(self) -> usize {
        (self.0 - 1) as usize
    }
    #[inline]
    pub fn from_index(i: usize) -> NodeId {
        NodeId(i as u64 + 1)
    }
    #[inline]
    pub fn is_valid(self) -> bool {
        self.0 != 0
    }
}

/// Node kind (CORE-TREE-1). Symlinks are leaves counted at their own size
/// (CORE-SCAN-6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NodeKind {
    File = 0,
    Dir = 1,
    Symlink = 2,
}

/// Attribute bits exposed raw to the host (CORE-TREE-7).
pub mod flags {
    /// Name starts with a dot.
    pub const HIDDEN: u32 = 1;
    /// A hard-link duplicate: bytes are counted once on the first-seen path
    /// (CORE-SCAN-5); this node contributes 0 bytes to aggregates.
    pub const HARDLINK_DUP: u32 = 1 << 1;
    /// The directory could not be read (permission denied / vanished);
    /// details are in the scan report (CORE-SCAN-13).
    pub const UNREADABLE: u32 = 1 << 2;
    /// Sizes below this node may still be growing (set while a scan is
    /// in flight; cleared when the subtree is complete).
    pub const SCANNING: u32 = 1 << 3;
}

/// One node in the arena.
///
/// Sizes on directories are *aggregates over descendants only*
/// (CORE-SCAN-3); files carry their own logical/physical sizes.
#[derive(Debug)]
pub struct Node {
    pub name: OsString,
    pub parent: NodeId,
    pub children: Vec<NodeId>,
    pub kind: NodeKind,
    /// Logical (apparent) size in bytes; for dirs, subtree aggregate.
    pub logical: u64,
    /// Physical (allocated) size in bytes; for dirs, subtree aggregate.
    pub physical: u64,
    /// Files in subtree (files count themselves as 1).
    pub files: u64,
    /// Directories in subtree (excluding self).
    pub subdirs: u64,
    /// mtime, seconds since Unix epoch (0 when unknown).
    pub mtime: i64,
    pub flags: u32,
    /// Kind bucket, see [`crate::classify::Category`].
    pub category: u8,
    /// Interned extension id; `u32::MAX` = none (dirs, extension-less files).
    pub ext_id: u32,
}

impl Node {
    /// CORE-TREE-6: `items == files + subdirs` at every node.
    #[inline]
    pub fn items(&self) -> u64 {
        self.files + self.subdirs
    }
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.kind == NodeKind::Dir
    }
}

/// Child sort keys (CORE-TREE-3). Sorting is within one parent only and uses
/// name as the stable secondary key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SortKey {
    Size = 0,
    Name = 1,
    Items = 2,
    Mtime = 3,
}

impl SortKey {
    pub fn from_u8(v: u8) -> SortKey {
        match v {
            1 => SortKey::Name,
            2 => SortKey::Items,
            3 => SortKey::Mtime,
            _ => SortKey::Size,
        }
    }
}

/// The arena. All mutation happens during scan/refresh under the model's
/// write lock; readers (FFI accessors, treemap) take the read lock.
#[derive(Debug, Default)]
pub struct Tree {
    nodes: Vec<Node>,
    /// Absolute path of the root node.
    pub root_path: OsString,
}

impl Tree {
    pub fn new() -> Tree {
        Tree::default()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    #[inline]
    pub fn get(&self, id: NodeId) -> Option<&Node> {
        if !id.is_valid() {
            return None;
        }
        self.nodes.get(id.index())
    }

    #[inline]
    pub fn get_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        if !id.is_valid() {
            return None;
        }
        self.nodes.get_mut(id.index())
    }

    pub fn push(&mut self, node: Node) -> NodeId {
        let id = NodeId::from_index(self.nodes.len());
        let parent = node.parent;
        self.nodes.push(node);
        if let Some(p) = self.get_mut(parent) {
            p.children.push(id);
        }
        id
    }

    /// Root-relative percent of logical bytes (CORE-TREE-5).
    pub fn percent_of_root(&self, id: NodeId) -> f64 {
        let (Some(n), Some(root)) = (self.get(id), self.get(NodeId::ROOT)) else {
            return 0.0;
        };
        if root.logical == 0 {
            return 0.0;
        }
        n.logical as f64 / root.logical as f64 * 100.0
    }

    /// Percent of the parent's logical bytes (CORE-TREE-5).
    pub fn percent_of_parent(&self, id: NodeId) -> f64 {
        let Some(n) = self.get(id) else { return 0.0 };
        let Some(p) = self.get(n.parent) else {
            return 100.0; // root
        };
        if p.logical == 0 {
            return 0.0;
        }
        n.logical as f64 / p.logical as f64 * 100.0
    }

    /// Path from root to `id`, inclusive (CORE-TREE-4).
    pub fn path_to_root(&self, id: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut cur = id;
        while cur.is_valid() {
            out.push(cur);
            cur = match self.get(cur) {
                Some(n) => n.parent,
                None => break,
            };
        }
        out.reverse();
        out
    }

    /// Absolute filesystem path of a node (root_path + component names).
    pub fn abs_path(&self, id: NodeId) -> std::path::PathBuf {
        let mut p = std::path::PathBuf::from(&self.root_path);
        for nid in self.path_to_root(id).into_iter().skip(1) {
            if let Some(n) = self.get(nid) {
                p.push(&n.name);
            }
        }
        p
    }

    /// Children of `id`, sorted by `key`/`descending` (CORE-TREE-3).
    /// Stable, name as secondary key; never flattens the tree.
    pub fn sorted_children(&self, id: NodeId, key: SortKey, descending: bool) -> Vec<NodeId> {
        let Some(n) = self.get(id) else {
            return Vec::new();
        };
        let mut ids = n.children.clone();
        ids.sort_by(|&a, &b| {
            let (na, nb) = (&self.nodes[a.index()], &self.nodes[b.index()]);
            let ord = match key {
                SortKey::Size => na.logical.cmp(&nb.logical),
                SortKey::Name => na.name.cmp(&nb.name),
                SortKey::Items => na.items().cmp(&nb.items()),
                SortKey::Mtime => na.mtime.cmp(&nb.mtime),
            };
            let ord = if descending { ord.reverse() } else { ord };
            ord.then_with(|| na.name.cmp(&nb.name))
        });
        ids
    }

    /// Add deltas to `start` and every ancestor up to the root.
    pub fn propagate(
        &mut self,
        start: NodeId,
        d_logical: u64,
        d_physical: u64,
        d_files: u64,
        d_subdirs: u64,
    ) {
        let mut cur = start;
        while cur.is_valid() {
            let n = &mut self.nodes[cur.index()];
            n.logical = n.logical.saturating_add(d_logical); // CORE-LIMIT-1
            n.physical = n.physical.saturating_add(d_physical);
            n.files = n.files.saturating_add(d_files);
            n.subdirs = n.subdirs.saturating_add(d_subdirs);
            cur = n.parent;
        }
    }
}

/// Age buckets for the "color by age" channel (design 1g).
/// Computed against a reference instant (scan start), not wall-clock now,
/// so results are deterministic for a loaded model.
pub fn age_bucket(mtime: i64, reference: SystemTime) -> u8 {
    let now = reference
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let age_days = (now - mtime).max(0) / 86_400;
    match age_days {
        0..=6 => 0,     // this week
        7..=29 => 1,    // this month
        30..=364 => 2,  // this year
        365..=729 => 3, // 1-2 years
        _ => 4,         // older
    }
}
