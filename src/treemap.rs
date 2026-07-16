//! ============================================================================
//! FILE: src/treemap.rs
//!
//! ============================================================================
//!
//! # Purpose
//! Treemap layout geometry (CORE-TM-*). The engine computes rectangles, not
//! pixels: each layout call walks the tree once and returns one flat buffer
//! of `LayoutRect` (the CORE-FFI-6 bulk-buffer rule — one FFI call per
//! frame, never per-rect chatter). Rendering — fills, hairlines, gradients,
//! selection rings, labels — is the host's job. Layout is a pure function
//! of its inputs (CORE-TM-6), so zooming is simply calling again with a
//! different root (CORE-TM-5), and identical inputs always produce
//! identical geometry.
//!
//! Two algorithms (CORE-TM-2):
//! * `KDirStat` — classic strip layout (parity with KDirStat/WinDirStat).
//! * `Squarified` — Bruls/Huizing/van Wijk aspect-ratio-minimizing layout,
//!   what the shipped design mocks use.
//!
//! # Upstream dependencies (what this file consumes)
//! - crate::tree — read-only walk of `Node.children` plus the per-node
//!   facts baked into each rect (sizes, kind, category, mtime, ext_id)
//! - (no std facilities beyond core; no locking — the caller holds the
//!   model's read lock for the duration of the call)
//!
//! # Downstream consumers (who depends on this file)
//! - src/ffi.rs — `ds_treemap_layout` wraps `layout` (converting
//!   `LayoutRect` to the `#[repr(C)]` `DsTmRect`), `ds_treemap_hit_test`
//!   wraps `hit_test`
//! - src/lib.rs — re-exports `hit_test`, `layout`, `Algorithm`,
//!   `LayoutParams`, `LayoutRect`
//! - tests/engine.rs — asserts area proportionality, determinism, zoom,
//!   hit-testing, and the physical-metric behavior
//!
//! # Structure
//! - `Algorithm` — layout algorithm selector (ABI byte)
//! - `LayoutRect` — one output rectangle with color-channel keys
//! - `LayoutParams` — algorithm + min_px + max_depth + metric choice
//! - `Ctx` — internal walk state (tree ref, params, color-key closures,
//!   output accumulator)
//! - `layout` — public entry: validates the root, then recurses via `emit`
//! - `emit` — emit one rect, then subdivide its children (recursive core)
//! - `squarify` — the Bruls/Huizing/van Wijk row builder
//! - `worst_aspect` — the aspect-ratio cost function squarify minimizes
//! - `rows` — the KDirStat strip layout
//! - `hit_test` — point -> deepest containing leaf (CORE-TM-4)
//!
//! # Algorithm & invariants
//! - Area invariant (CORE-TM-1): every child's area is exactly
//!   `parent_area * child_size / children_total`, so leaf areas tile their
//!   parent with no gaps or overlaps (modulo f32 rounding). Zero-size
//!   nodes get no rect at all — invisible by definition.
//! - Emission order: a directory's rect is pushed BEFORE its children's,
//!   so hosts can paint the buffer front-to-back and get correct nesting,
//!   and `depth` increases monotonically along any ancestor chain.
//! - Determinism: children are sorted (size desc, then NodeId asc as the
//!   tiebreak) before layout, so equal inputs give byte-equal output.
//! - Subdivision stops at `min_px` (subtree drawn as one aggregate block)
//!   or `max_depth` (safety valve for pathological trees); the parent rect
//!   is still emitted, so hit-tests inside resolve to the directory.
//! - The `ext_slot` / `age` closures are injected by the caller (the FFI
//!   layer computes the top-12 extension mapping and the scan-start-based
//!   age reference) so this module stays free of model/aggregation state.
//!
//! ============================================================================

use crate::tree::{NodeId, Tree};

/// Layout algorithm selector (CORE-TM-2). Values are ABI: they arrive as
/// the raw `algorithm` byte of `ds_treemap_layout`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Algorithm {
    KDirStat = 0,
    Squarified = 1,
}

impl Algorithm {
    /// Decode the FFI byte; anything unknown falls back to `KDirStat`.
    pub fn from_u8(v: u8) -> Algorithm {
        if v == 1 {
            Algorithm::Squarified
        } else {
            Algorithm::KDirStat
        }
    }
}

/// One laid-out rectangle. `is_dir == true` entries are group rects (emitted
/// before their children) so hosts can draw nesting hairlines and labels;
/// leaves carry the color-channel keys for design 1g (category / age /
/// extension slot).
#[derive(Debug, Clone, Copy)]
pub struct LayoutRect {
    /// Which node this rect represents (for selection / hit-test).
    pub node: NodeId,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// Nesting level relative to the layout root (root = 0).
    pub depth: u16,
    pub is_dir: bool,
    /// Kind bucket, see [`crate::classify::Category`].
    pub category: u8,
    /// Age bucket 0..=4, see [`crate::tree::age_bucket`].
    pub age_bucket: u8,
    /// Extension palette slot (0..11 = top-12, 12 = other/none), CORE-EXT-3.
    pub ext_slot: u8,
}

/// Layout parameters. `min_px` is the minimum rect side below which a
/// subtree is not subdivided further (drawn as one aggregate block).
#[derive(Debug, Clone, Copy)]
pub struct LayoutParams {
    pub algorithm: Algorithm,
    pub min_px: f32,
    /// Maximum recursion depth (safety valve for pathological trees).
    pub max_depth: u16,
    /// Area ∝ physical (allocated) bytes instead of logical bytes — the
    /// truthful channel on filesystems with sparse/clone/dataless files.
    pub use_physical: bool,
}

impl Default for LayoutParams {
    fn default() -> Self {
        LayoutParams {
            algorithm: Algorithm::Squarified,
            min_px: 2.0,
            max_depth: 64,
            use_physical: false,
        }
    }
}

/// Internal walk state threaded through the recursion: the (read-locked)
/// tree, the fixed parameters, the caller-supplied color-key closures, and
/// the flat output buffer being accumulated.
struct Ctx<'a> {
    tree: &'a Tree,
    params: LayoutParams,
    /// ext_id -> palette slot (the FFI layer passes the top-12 mapping).
    ext_slot: &'a dyn Fn(u32) -> u8,
    /// mtime -> age bucket (the FFI layer binds the scan-start reference).
    age: &'a dyn Fn(i64) -> u8,
    out: Vec<LayoutRect>,
}

/// Lay out the subtree under `root` into `(x, y, w, h)` (CORE-TM-1).
/// Pure function of its inputs (CORE-TM-6); zoom = call with a different
/// root (CORE-TM-5).
///
/// Returns the flat rect buffer, parents before children. Returns an empty
/// buffer (not an error) when `root` is invalid, the root's size under the
/// chosen metric is 0, or the target rect is degenerate — all cases where
/// there is legitimately nothing to draw.
#[allow(clippy::too_many_arguments)]
pub fn layout(
    tree: &Tree,
    root: NodeId,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    params: LayoutParams,
    ext_slot: &dyn Fn(u32) -> u8,
    age: &dyn Fn(i64) -> u8,
) -> Vec<LayoutRect> {
    let mut ctx = Ctx {
        tree,
        params,
        ext_slot,
        age,
        out: Vec::new(),
    };
    let Some(n) = tree.get(root) else {
        return ctx.out;
    };
    let root_size = if params.use_physical {
        n.physical
    } else {
        n.logical
    };
    if root_size == 0 || w <= 0.0 || h <= 0.0 {
        return ctx.out;
    }
    emit(&mut ctx, root, x, y, w, h, 0);
    ctx.out
}

/// Emit the rect for `id`, then (for directories within the depth/size
/// limits) subdivide its area among its children. This is the recursive
/// core shared by both algorithms; parent rects always precede child rects
/// in the output.
fn emit(ctx: &mut Ctx, id: NodeId, x: f32, y: f32, w: f32, h: f32, depth: u16) {
    let n = ctx.tree.get(id).unwrap();
    let rect = LayoutRect {
        node: id,
        x,
        y,
        w,
        h,
        depth,
        is_dir: n.is_dir(),
        category: n.category,
        age_bucket: (ctx.age)(n.mtime),
        ext_slot: (ctx.ext_slot)(n.ext_id),
    };
    ctx.out.push(rect);

    if !n.is_dir() || depth >= ctx.params.max_depth {
        return;
    }
    if w < ctx.params.min_px || h < ctx.params.min_px {
        return; // too small to subdivide; stays one aggregate block
    }

    // Children sorted size-desc; zero-size children are invisible by
    // definition (area ∝ size, CORE-TM-1). The desc sort matters for
    // squarify (see below); the NodeId tiebreak keeps equal-size ordering
    // deterministic.
    let use_physical = ctx.params.use_physical;
    let mut kids: Vec<(NodeId, u64)> = n
        .children
        .iter()
        .filter_map(|&c| {
            ctx.tree.get(c).map(|cn| {
                (
                    c,
                    if use_physical {
                        cn.physical
                    } else {
                        cn.logical
                    },
                )
            })
        })
        .filter(|&(_, s)| s > 0)
        .collect();
    if kids.is_empty() {
        return;
    }
    kids.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let total: u64 = kids.iter().map(|k| k.1).sum();
    if total == 0 {
        return;
    }

    match ctx.params.algorithm {
        Algorithm::Squarified => squarify(ctx, &kids, total, x, y, w, h, depth + 1),
        Algorithm::KDirStat => rows(ctx, &kids, total, x, y, w, h, depth + 1),
    }
}

/// Squarified treemap (Bruls, Huizing, van Wijk, "Squarified Treemaps").
///
/// The paper's greedy heuristic: fill the remaining free rectangle one
/// "row" at a time, laying each row along the CURRENT short side, and
/// keep adding children to the row only while doing so improves (does not
/// worsen) the row's worst aspect ratio. Because `kids` is sorted size-desc
/// (a precondition from `emit` — the paper requires it for the greedy step
/// to be monotone), each row is a run of consecutive children.
///
/// `(x, y, w, h)` shrink as rows are placed; they always describe the
/// still-unfilled portion of the parent rect.
#[allow(clippy::too_many_arguments)]
fn squarify(
    ctx: &mut Ctx,
    kids: &[(NodeId, u64)],
    total: u64,
    mut x: f32,
    mut y: f32,
    mut w: f32,
    mut h: f32,
    depth: u16,
) {
    // Bytes-to-area conversion factor. Computed once from the FULL rect
    // and total, so every child's area is size * scale exactly and the
    // areas tile the parent (CORE-TM-1). f64 keeps precision for huge
    // byte counts that f32 would truncate.
    let scale = (w as f64 * h as f64) / total as f64;
    let mut i = 0; // first child not yet placed
    while i < kids.len() {
        // Rows are laid along the short side of the REMAINING rect: that
        // is the choice that lets row members stay near-square.
        let short = w.min(h) as f64;
        if short <= 0.0 {
            return; // remaining strip has collapsed to zero: nothing to place
        }
        // --- Row-building: grow [i, row_end) while worst aspect improves.
        // Seed the row with child i alone.
        let mut row_end = i + 1;
        let mut row_sum = kids[i].1 as f64 * scale;
        let mut worst = worst_aspect(kids[i].1 as f64 * scale, row_sum, short);
        while row_end < kids.len() {
            let a = kids[row_end].1 as f64 * scale;
            let new_sum = row_sum + a;
            // The paper's `worst()` evaluates every member of the candidate
            // row; since areas are sorted desc, the extremes are the first
            // (largest) and the candidate (smallest) member, so checking
            // just those two approximates the classic formulation — adding
            // area thickens the row, which stretches the largest member and
            // squashes the smallest; the middle members sit in between.
            let new_worst = worst_aspect(kids[i].1 as f64 * scale, new_sum, short)
                .max(worst_aspect(a, new_sum, short));
            if new_worst > worst {
                break; // adding this child would worsen the row: close it
            }
            worst = new_worst;
            row_sum = new_sum;
            row_end += 1;
        }
        // --- Placement: the row is a strip of `thickness` along the short
        // side; each member's extent along the strip is proportional to
        // its area (area / thickness), so members tile the strip exactly.
        let thickness = (row_sum / short) as f32;
        if w >= h {
            // Wide remainder: short side is vertical, so the row is a
            // vertical strip on the left edge; members stack top-to-bottom.
            let mut cy = y;
            for &(cid, cs) in &kids[i..row_end] {
                let ch = ((cs as f64 * scale) / thickness as f64) as f32;
                emit(ctx, cid, x, cy, thickness, ch, depth);
                cy += ch;
            }
            // Consume the strip: remaining rect moves right and narrows.
            x += thickness;
            w -= thickness;
        } else {
            // Tall remainder: mirror image — a horizontal strip on the top
            // edge; members run left-to-right.
            let mut cx = x;
            for &(cid, cs) in &kids[i..row_end] {
                let cw = ((cs as f64 * scale) / thickness as f64) as f32;
                emit(ctx, cid, cx, y, cw, thickness, depth);
                cx += cw;
            }
            y += thickness;
            h -= thickness;
        }
        i = row_end; // next row starts after this one
    }
}

/// Aspect-ratio cost of one member in a candidate row: given the member's
/// `area`, the row's total area `row_sum`, and the `short` side the row is
/// laid along, the row's thickness is `row_sum / short` and the member's
/// length is `area / thickness`. Returns max(thickness/len, len/thickness),
/// i.e. the aspect ratio normalized to >= 1 (1.0 = perfect square) — the
/// quantity squarify greedily minimizes. Degenerate inputs return MAX so
/// they always lose the comparison.
fn worst_aspect(area: f64, row_sum: f64, short: f64) -> f64 {
    if area <= 0.0 || row_sum <= 0.0 {
        return f64::MAX;
    }
    let thickness = row_sum / short;
    let len = area / thickness;
    (thickness / len).max(len / thickness)
}

/// KDirStat-style strip layout: all children of one directory are laid in
/// a single strip spanning the long side, each with extent proportional to
/// its size. The direction flips with the rect's own orientation, so
/// nesting alternates horizontal/vertical strips with depth. Simpler and
/// more "list-like" than squarify; kept for KDirStat/WinDirStat parity.
#[allow(clippy::too_many_arguments)]
fn rows(
    ctx: &mut Ctx,
    kids: &[(NodeId, u64)],
    total: u64,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    depth: u16,
) {
    let horizontal = w >= h;
    let mut offset = 0.0f64;
    let span = if horizontal { w } else { h } as f64;
    for &(cid, cs) in kids {
        let frac = cs as f64 / total as f64;
        let extent = span * frac;
        if horizontal {
            emit(ctx, cid, x + offset as f32, y, extent as f32, h, depth);
        } else {
            emit(ctx, cid, x, y + offset as f32, w, extent as f32, depth);
        }
        offset += extent;
    }
}

/// Hit test (CORE-TM-4): deepest leaf whose rect contains the point, else
/// the deepest directory rect. Returns `NodeId::INVALID` when the point is
/// outside every rect. Linear scan — rect buffers are per-frame and modest,
/// and directory rects overlap their children so a spatial index would
/// still need the leaf-beats-dir rule.
pub fn hit_test(rects: &[LayoutRect], px: f32, py: f32) -> NodeId {
    let mut best = NodeId::INVALID;
    let mut best_depth = -1i32;
    let mut best_leaf = false;
    for r in rects {
        // Half-open containment ([x, x+w)) so adjacent rects never both
        // claim a shared edge.
        if px >= r.x && px < r.x + r.w && py >= r.y && py < r.y + r.h {
            let leaf = !r.is_dir;
            // Preference order: any leaf beats any directory (a leaf's
            // ancestors always contain the same point); among equals,
            // deeper wins (e.g. the directory shown as an aggregate block
            // when subdivision stopped at min_px/max_depth).
            if (leaf && !best_leaf) || (leaf == best_leaf && r.depth as i32 > best_depth) {
                best = r.node;
                best_depth = r.depth as i32;
                best_leaf = leaf;
            }
        }
    }
    best
}
