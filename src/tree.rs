//! ============================================================================
//! FILE: src/tree.rs
//!
//! ============================================================================
//!
//! # Purpose
//! The in-memory directory tree (CORE-TREE-*): a flat arena of [`Node`]s
//! indexed by [`NodeId`]. An arena (one `Vec<Node>` with integer handles)
//! was chosen over boxed/linked nodes so that (a) hosts can hold stable,
//! copyable, opaque `u64` handles across the FFI boundary for the life of a
//! model (CORE-TREE-2), and (b) tens of millions of nodes stay cache-friendly
//! with no per-node allocation beyond the name and child list. The tree
//! itself never crosses FFI; hosts pull per-node facts lazily via handles.
//!
//! # Upstream dependencies (what this file consumes)
//! - std::ffi::OsString — node names and root path are stored as raw OS
//!   strings, never lossy UTF-8, so 255-byte non-UTF-8 names survive
//!   (CORE-LIMIT-3)
//! - std::time::SystemTime — reference instant for [`age_bucket`]
//! - std::path::PathBuf — reassembling absolute paths in [`Tree::abs_path`]
//! - (no other crate modules: tree.rs is the dependency root of the crate)
//!
//! # Downstream consumers (who depends on this file)
//! - src/scan.rs — the only mutator: builds nodes via `Tree::push`, updates
//!   aggregates via `Tree::propagate`, and rewires subtrees in
//!   `refresh_node`; also uses the `flags` bit constants
//! - src/treemap.rs — read-only: walks `Node.children`, reads sizes/category
//!   /mtime/ext_id to lay out rectangles
//! - src/classify.rs — none directly, but `Node.category` stores its
//!   `Category as u8`
//! - src/ffi.rs — read-only accessors (`get`, `sorted_children`,
//!   `percent_of_*`, `path_to_root`, `abs_path`, `age_bucket`) behind
//!   `ds_node_*` / `ds_model_*`; mirrors the `flags` constants as
//!   `DS_NODE_FLAG_*`
//! - tests/engine.rs — asserts the CORE-TREE invariants directly
//!
//! # Structure
//! - `NodeId` — 1-based opaque handle (0 = invalid, 1 = root)
//! - `NodeKind` — file / dir / symlink discriminant
//! - `flags` module — raw attribute bits exposed to the host (CORE-TREE-7)
//! - `Node` — one arena entry; sizes on dirs are subtree aggregates
//! - `SortKey` — child sort keys (CORE-TREE-3)
//! - `Tree` — the arena: push/get/sorted_children/percent/paths/propagate
//! - `age_bucket` — mtime -> 5-bucket age classification (free function)
//!
//! # Algorithm & invariants
//! - Handle scheme: `NodeId(n)` is index `n - 1` into the arena; ids are
//!   never reused or invalidated while the model lives, and nodes are only
//!   appended (even `refresh_node` grafts new nodes rather than reusing old
//!   indices — stale subtree nodes become unreachable garbage, which is the
//!   accepted cost of handle stability).
//! - Counts invariant (CORE-TREE-6): `items == files + subdirs` at every
//!   node, maintained because `files`/`subdirs` are only ever changed
//!   together with the same propagation walk.
//! - Aggregate rule (CORE-SCAN-3): a directory's `logical`/`physical` is
//!   solely the sum over descendants; a file's is its own size.
//! - Overflow rule (CORE-LIMIT-1): all accumulation is saturating, so sizes
//!   cap at `u64::MAX` instead of wrapping.
//! - Thread-safety contract: `Tree` itself has no interior locking. All
//!   mutation happens under the owning `Model.tree` `RwLock` write guard
//!   (see scan.rs); every reader holds the read guard, so a reader always
//!   observes a fully consistent tree.
//!
//! ============================================================================

use std::ffi::OsString;
use std::time::SystemTime;

/// Stable opaque node handle (CORE-TREE-2). `0` is reserved as "invalid";
/// the root of a model is always `NodeId(1)`. Valid for the life of the
/// loaded model — handles are never reused — and passed across FFI as a
/// plain `u64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u64);

impl NodeId {
    /// The reserved null handle (`0`). Returned by lookups that miss (e.g.
    /// [`crate::treemap::hit_test`]) and used as the root's `parent`.
    pub const INVALID: NodeId = NodeId(0);
    /// The root of every model. The scanner always pushes the root node
    /// first, so it lands at arena index 0 == `NodeId(1)`.
    pub const ROOT: NodeId = NodeId(1);

    /// Arena index for this handle (`id - 1`). Only meaningful for valid
    /// ids; calling it on `INVALID` would underflow, so callers check
    /// [`Self::is_valid`] first (as [`Tree::get`] does).
    #[inline]
    pub fn index(self) -> usize {
        (self.0 - 1) as usize
    }
    /// Inverse of [`Self::index`]: arena position -> handle.
    #[inline]
    pub fn from_index(i: usize) -> NodeId {
        NodeId(i as u64 + 1)
    }
    /// True for any handle other than [`Self::INVALID`]. Does not check
    /// that the id is in range for a particular tree; `Tree::get` does.
    #[inline]
    pub fn is_valid(self) -> bool {
        self.0 != 0
    }
}

/// Node kind (CORE-TREE-1). Symlinks are leaves counted at their own size
/// (CORE-SCAN-6). The numeric values are ABI: they cross FFI unchanged in
/// `DsNodeInfo.kind`, so the discriminants must never be renumbered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NodeKind {
    File = 0,
    Dir = 1,
    Symlink = 2,
}

/// Attribute bits exposed raw to the host (CORE-TREE-7). The engine supplies
/// the bits; labeling/interpretation is the host's job. Values are ABI:
/// src/ffi.rs mirrors them as `DS_NODE_FLAG_*` constants and a unit test
/// (`flag_sync`) asserts the two sets stay equal.
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
    /// A directory whose (device, inode) was already scanned via another
    /// path — an alias (APFS firmlink, bind mount). Shown as an entry but
    /// contributes nothing and is not descended, so aliased trees are
    /// counted exactly once.
    pub const DUPLICATE: u32 = 1 << 4;
}

/// One node in the arena.
///
/// Sizes on directories are *aggregates over descendants only*
/// (CORE-SCAN-3); files carry their own logical/physical sizes. All
/// aggregate fields (`logical`, `physical`, `files`, `subdirs`) are updated
/// exclusively through [`Tree::propagate`] (and the refresh walks in
/// scan.rs) so the CORE-TREE-6 invariant holds at all times.
#[derive(Debug)]
pub struct Node {
    /// Last path component, raw OS bytes (CORE-LIMIT-3: no lossy UTF-8).
    pub name: OsString,
    /// Owning directory; `NodeId::INVALID` for the root.
    pub parent: NodeId,
    /// Direct children in insertion order (i.e. name-sorted per directory,
    /// because the scanner sorts each listing before insertion). Use
    /// [`Tree::sorted_children`] for host-facing ordering.
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
    /// Bitset of [`flags`] constants (raw; interpretation is host-side).
    pub flags: u32,
    /// Kind bucket, see [`crate::classify::Category`].
    pub category: u8,
    /// Interned extension id; `u32::MAX` = none (dirs, extension-less files).
    pub ext_id: u32,
}

impl Node {
    /// CORE-TREE-6: `items == files + subdirs` at every node. Computed
    /// rather than stored so the invariant cannot drift.
    #[inline]
    pub fn items(&self) -> u64 {
        self.files + self.subdirs
    }
    /// True for real directories only (a symlink to a directory is a leaf).
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.kind == NodeKind::Dir
    }
}

/// Child sort keys (CORE-TREE-3). Sorting is within one parent only and uses
/// name as the stable secondary key. `Size` is logical (apparent) bytes;
/// `PhysicalSize` is allocated on-disk bytes — the truthful metric on
/// filesystems with sparse files, clones, and cloud-placeholder (dataless)
/// files. The numeric values are ABI: `ds_node_children` takes them as a
/// raw `u8` (see [`SortKey::from_u8`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SortKey {
    Size = 0,
    Name = 1,
    Items = 2,
    Mtime = 3,
    PhysicalSize = 4,
}

impl SortKey {
    /// Decode a raw FFI sort-key byte. Unknown values fall back to `Size`
    /// (never an error) so older hosts stay compatible with newer engines.
    pub fn from_u8(v: u8) -> SortKey {
        match v {
            1 => SortKey::Name,
            2 => SortKey::Items,
            3 => SortKey::Mtime,
            4 => SortKey::PhysicalSize,
            _ => SortKey::Size,
        }
    }
}

/// The arena. All mutation happens during scan/refresh under the model's
/// write lock ([`crate::scan::Model::tree`]); readers (FFI accessors,
/// treemap) take the read lock. `Tree` itself is lock-free by design so it
/// stays trivially testable.
#[derive(Debug, Default)]
pub struct Tree {
    /// Append-only node storage; `NodeId(n)` lives at index `n - 1`.
    nodes: Vec<Node>,
    /// Absolute path of the root node (raw OS bytes). `Node.name` holds
    /// only the last component; [`Tree::abs_path`] joins this prefix with
    /// component names.
    pub root_path: OsString,
}

impl Tree {
    /// Empty tree (no root yet); the scanner pushes the root as node 1.
    pub fn new() -> Tree {
        Tree::default()
    }

    /// Total nodes in the arena (including any grafted-over garbage after a
    /// refresh — the arena is append-only).
    #[inline]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Look up a node; `None` for `NodeId::INVALID` or out-of-range ids
    /// (the FFI layer maps that to an "invalid node id" error).
    #[inline]
    pub fn get(&self, id: NodeId) -> Option<&Node> {
        if !id.is_valid() {
            return None;
        }
        self.nodes.get(id.index())
    }

    /// Mutable lookup; same validity rules as [`Tree::get`]. Only scan.rs
    /// calls this, always under the model's write lock.
    #[inline]
    pub fn get_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        if !id.is_valid() {
            return None;
        }
        self.nodes.get_mut(id.index())
    }

    /// Append `node` to the arena and link it into its parent's child list.
    /// Returns the new node's id. The parent link is best-effort: pushing
    /// the root (parent = INVALID) simply skips the child-list update.
    /// Aggregates are NOT touched here — callers batch them via
    /// [`Tree::propagate`] once per directory listing.
    pub fn push(&mut self, node: Node) -> NodeId {
        let id = NodeId::from_index(self.nodes.len());
        let parent = node.parent;
        self.nodes.push(node);
        if let Some(p) = self.get_mut(parent) {
            p.children.push(id);
        }
        id
    }

    /// Root-relative percent of logical bytes (CORE-TREE-5). Returns 0.0
    /// for invalid ids or an empty (0-byte) root rather than erroring —
    /// percentages are display sugar, not load-bearing math.
    pub fn percent_of_root(&self, id: NodeId) -> f64 {
        let (Some(n), Some(root)) = (self.get(id), self.get(NodeId::ROOT)) else {
            return 0.0;
        };
        if root.logical == 0 {
            return 0.0;
        }
        n.logical as f64 / root.logical as f64 * 100.0
    }

    /// Percent of the parent's logical bytes (CORE-TREE-5). The root has no
    /// parent and reports 100%; a 0-byte parent reports 0.0.
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

    /// Path from root to `id`, inclusive (CORE-TREE-4) — the "reveal/expand
    /// to this node" walk. Built by climbing parent links then reversing;
    /// an invalid `id` yields an empty vector.
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
    /// The root's own `name` is skipped because `root_path` already ends
    /// with it.
    pub fn abs_path(&self, id: NodeId) -> std::path::PathBuf {
        let mut p = std::path::PathBuf::from(&self.root_path);
        // skip(1): index 0 of path_to_root is the root itself.
        for nid in self.path_to_root(id).into_iter().skip(1) {
            if let Some(n) = self.get(nid) {
                p.push(&n.name);
            }
        }
        p
    }

    /// Children of `id`, sorted by `key`/`descending` (CORE-TREE-3).
    /// Stable, name as secondary key; never flattens the tree (sorting is
    /// within this one parent only). Invalid `id` yields an empty vector.
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
                SortKey::PhysicalSize => na.physical.cmp(&nb.physical),
            };
            let ord = if descending { ord.reverse() } else { ord };
            // Name tiebreak is applied AFTER the direction flip so it stays
            // ascending in both directions — the stable secondary key that
            // makes results deterministic across runs.
            ord.then_with(|| na.name.cmp(&nb.name))
        });
        ids
    }

    /// Add deltas to `start` and every ancestor up to the root. This is the
    /// single write path for aggregates during a scan: one call per
    /// directory listing keeps readers' view consistent (they either see a
    /// directory's whole contribution or none of it). Saturating adds cap
    /// totals at `u64::MAX` (CORE-LIMIT-1).
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
/// so results are deterministic for a loaded model: the same model always
/// yields the same buckets no matter when the host asks.
///
/// Returns 0 (this week), 1 (this month), 2 (this year), 3 (1-2 years),
/// 4 (older). Future mtimes clamp to bucket 0.
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
