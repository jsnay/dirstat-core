//! Treemap layout geometry (CORE-TM-*).
//!
//! The engine computes rectangles, not pixels: each layout call returns one
//! flat buffer of `LayoutRect` (CORE-FFI-6 bulk-buffer rule). Rendering —
//! fills, hairlines, gradients, selection rings, labels — is the host's job.
//!
//! Two algorithms (CORE-TM-2):
//! * `KDirStat` — classic row layout (default, parity).
//! * `Squarified` — Bruls/Huizing/van Wijk aspect-ratio-minimizing layout,
//!   what the shipped design mocks use.

use crate::tree::{NodeId, Tree};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Algorithm {
    KDirStat = 0,
    Squarified = 1,
}

impl Algorithm {
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
    pub node: NodeId,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub depth: u16,
    pub is_dir: bool,
    pub category: u8,
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

struct Ctx<'a> {
    tree: &'a Tree,
    params: LayoutParams,
    ext_slot: &'a dyn Fn(u32) -> u8,
    age: &'a dyn Fn(i64) -> u8,
    out: Vec<LayoutRect>,
}

/// Lay out the subtree under `root` into `(x, y, w, h)` (CORE-TM-1).
/// Pure function of its inputs (CORE-TM-6); zoom = call with a different
/// root (CORE-TM-5).
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
    // definition (area ∝ size, CORE-TM-1).
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
    let scale = (w as f64 * h as f64) / total as f64;
    let mut i = 0;
    while i < kids.len() {
        let short = w.min(h) as f64;
        if short <= 0.0 {
            return;
        }
        // Grow the current row while the worst aspect ratio improves.
        let mut row_end = i + 1;
        let mut row_sum = kids[i].1 as f64 * scale;
        let mut worst = worst_aspect(kids[i].1 as f64 * scale, row_sum, short);
        while row_end < kids.len() {
            let a = kids[row_end].1 as f64 * scale;
            let new_sum = row_sum + a;
            // Worst ratio across first and candidate areas approximates the
            // classic formulation (areas are sorted desc).
            let new_worst = worst_aspect(kids[i].1 as f64 * scale, new_sum, short)
                .max(worst_aspect(a, new_sum, short));
            if new_worst > worst {
                break;
            }
            worst = new_worst;
            row_sum = new_sum;
            row_end += 1;
        }
        // Lay the row along the short side.
        let thickness = (row_sum / short) as f32;
        if w >= h {
            let mut cy = y;
            for &(cid, cs) in &kids[i..row_end] {
                let ch = ((cs as f64 * scale) / thickness as f64) as f32;
                emit(ctx, cid, x, cy, thickness, ch, depth);
                cy += ch;
            }
            x += thickness;
            w -= thickness;
        } else {
            let mut cx = x;
            for &(cid, cs) in &kids[i..row_end] {
                let cw = ((cs as f64 * scale) / thickness as f64) as f32;
                emit(ctx, cid, cx, y, cw, thickness, depth);
                cx += cw;
            }
            y += thickness;
            h -= thickness;
        }
        i = row_end;
    }
}

fn worst_aspect(area: f64, row_sum: f64, short: f64) -> f64 {
    if area <= 0.0 || row_sum <= 0.0 {
        return f64::MAX;
    }
    let thickness = row_sum / short;
    let len = area / thickness;
    (thickness / len).max(len / thickness)
}

/// KDirStat-style strip layout: children fill fixed-direction rows,
/// alternating with depth.
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
/// the deepest directory rect.
pub fn hit_test(rects: &[LayoutRect], px: f32, py: f32) -> NodeId {
    let mut best = NodeId::INVALID;
    let mut best_depth = -1i32;
    let mut best_leaf = false;
    for r in rects {
        if px >= r.x && px < r.x + r.w && py >= r.y && py < r.y + r.h {
            let leaf = !r.is_dir;
            if (leaf && !best_leaf) || (leaf == best_leaf && r.depth as i32 > best_depth) {
                best = r.node;
                best_depth = r.depth as i32;
                best_leaf = leaf;
            }
        }
    }
    best
}
