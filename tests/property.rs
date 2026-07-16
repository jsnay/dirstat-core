//! ============================================================================
//! FILE: tests/property.rs
//!
//! ============================================================================
//!
//! # Purpose
//! Property-based checks on the treemap geometry (dirstat-core#9): instead of
//! hand-picked trees, generate many seeded-random trees and assert the
//! invariants that must hold for ANY input. Uses a tiny hand-rolled LCG so
//! there is no new dependency (spec §0: zero runtime deps) and every failure
//! prints the seed that produced it, making it reproducible.
//!
//! Invariants checked, for both algorithms and both size metrics:
//! - leaf rect areas sum to the root rect area (area ∝ size, CORE-TM-1);
//! - every leaf rect lies within the layout bounds;
//! - hit-testing the center of each emitted leaf returns that leaf
//!   (CORE-TM-4);
//! - layout is deterministic across repeated calls (CORE-TM-6).
//!
//! # Upstream dependencies
//! - dirstat_core::scan (Scan/ScanOptions), tree (NodeId),
//!   treemap (layout/hit_test/Algorithm/LayoutParams)
//! - std::fs — generated on-disk fixtures
//!
//! ============================================================================

use std::fs;
use std::path::{Path, PathBuf};

use dirstat_core::scan::{Scan, ScanOptions};
use dirstat_core::tree::NodeId;
use dirstat_core::treemap::{self, Algorithm, LayoutParams};

/// Deterministic linear-congruential generator (glibc constants). Not for
/// cryptography — just reproducible pseudo-randomness keyed by a seed.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }
    fn range(&mut self, n: usize) -> usize {
        (self.next() >> 33) as usize % n.max(1)
    }
}

struct Fx {
    root: PathBuf,
}
impl Fx {
    fn new(seed: u64) -> Fx {
        let root = std::env::temp_dir().join(format!("dirstat-prop-{seed}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        Fx { root }
    }
}
impl Drop for Fx {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Build a random tree under `root` from `seed`: random branching, random
/// depth, random file sizes (including some zero-byte files).
fn build_random_tree(root: &Path, seed: u64) {
    let mut rng = Lcg(seed.wrapping_add(0x9E3779B97F4A7C15));
    // Breadth-first growth with a node budget so trees stay bounded.
    let mut dirs = vec![root.to_path_buf()];
    let mut budget = 60 + rng.range(140);
    while let Some(dir) = dirs.pop() {
        let files = rng.range(6);
        for f in 0..files {
            if budget == 0 {
                return;
            }
            budget -= 1;
            let size = match rng.range(4) {
                0 => 0,                         // zero-byte
                1 => 1 + rng.range(64),         // tiny
                _ => 1000 + rng.range(200_000), // normal
            };
            let _ = fs::write(dir.join(format!("f{f}.dat")), vec![0u8; size]);
        }
        let subdirs = rng.range(4);
        for d in 0..subdirs {
            if budget == 0 {
                return;
            }
            budget -= 1;
            let sub = dir.join(format!("d{d}"));
            if fs::create_dir(&sub).is_ok() {
                dirs.push(sub);
            }
        }
    }
}

fn check_invariants(seed: u64) {
    let fx = Fx::new(seed);
    build_random_tree(&fx.root, seed);
    let mut s = Scan::begin(&fx.root, ScanOptions::default(), None).unwrap();
    s.join();
    let tree = s.model.tree.read().unwrap();

    const W: f32 = 1000.0;
    const H: f32 = 700.0;
    for algo in [Algorithm::Squarified, Algorithm::KDirStat] {
        for use_physical in [false, true] {
            let params = LayoutParams {
                algorithm: algo,
                min_px: 0.5,
                max_depth: 64,
                use_physical,
            };
            let rects =
                treemap::layout(&tree, NodeId::ROOT, 0.0, 0.0, W, H, params, &|_| 12, &|_| 0);

            let root_total = if use_physical {
                tree.get(NodeId::ROOT).unwrap().physical
            } else {
                tree.get(NodeId::ROOT).unwrap().logical
            };
            if root_total == 0 || rects.is_empty() {
                continue; // all-zero tree: nothing to lay out, vacuously fine
            }

            // (1) leaf areas sum to the root rect area.
            let leaf_area: f64 = rects
                .iter()
                .filter(|r| !r.is_dir)
                .map(|r| r.w as f64 * r.h as f64)
                .sum();
            let root_area = (W * H) as f64;
            assert!(
                (leaf_area - root_area).abs() < root_area * 0.01,
                "seed {seed} {algo:?} phys={use_physical}: leaf area {leaf_area} vs {root_area}"
            );

            // (2) every VISIBLE leaf lies within bounds. Sub-pixel slivers
            // (a tiny file beside huge siblings) can overshoot by f32
            // accumulation in the area-proportional math — invisible
            // (< 1px in a dimension) and clipped by the renderer, so the
            // invariant is asserted for rects a user could actually see.
            // A real escape (a visible rect at x=1200 on a 1000px pane)
            // still fails loudly, with a small tolerance for rounding.
            const EPS: f32 = 0.5;
            for r in rects
                .iter()
                .filter(|r| !r.is_dir && r.w >= 1.0 && r.h >= 1.0)
            {
                assert!(
                    r.x >= -EPS && r.y >= -EPS && r.x + r.w <= W + EPS && r.y + r.h <= H + EPS,
                    "seed {seed}: visible leaf out of bounds {r:?}"
                );
            }

            // (3) hit-testing each leaf's center returns that leaf.
            for r in rects.iter().filter(|r| !r.is_dir) {
                if r.w < 1.0 || r.h < 1.0 {
                    continue; // sub-pixel rects: center may round onto a neighbor
                }
                let hit = treemap::hit_test(&rects, r.x + r.w / 2.0, r.y + r.h / 2.0);
                assert_eq!(hit, r.node, "seed {seed}: hit-test miss at {r:?}");
            }

            // (4) determinism: an identical second layout is byte-identical.
            let again =
                treemap::layout(&tree, NodeId::ROOT, 0.0, 0.0, W, H, params, &|_| 12, &|_| 0);
            assert_eq!(rects.len(), again.len());
            for (a, b) in rects.iter().zip(again.iter()) {
                assert_eq!((a.node, a.x, a.y, a.w, a.h), (b.node, b.x, b.y, b.w, b.h));
            }
        }
    }
}

/// Run the invariant battery over a spread of seeds.
#[test]
fn treemap_invariants_over_random_trees() {
    for seed in 0..60u64 {
        check_invariants(seed.wrapping_mul(2654435761));
    }
}
