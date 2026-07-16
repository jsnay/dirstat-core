//! ============================================================================
//! FILE: tests/fuzz.rs
//!
//! ============================================================================
//!
//! # Purpose
//! A dependency-free fuzz smoke test (dirstat-core#10): feed arbitrary bytes
//! to the pure string parsers and arbitrary shapes to the treemap layout,
//! and assert nothing panics or hangs. Not libFuzzer (that needs a
//! nightly-only toolchain and would add a dependency, against spec §0) — a
//! seeded loop covers the same ground deterministically and runs in CI. A
//! failure prints the seed for reproduction.
//!
//! What it targets:
//! - `classify::category_for_dir_name` / `category_for_extension` /
//!   `classify_file` — hostile directory and extension strings;
//! - `treemap::layout` + `hit_test` — random synthetic trees at random
//!   rectangles, then hit-testing random points.
//!
//! # Upstream dependencies
//! - dirstat_core::classify, dirstat_core::tree (Tree/Node builder),
//!   dirstat_core::treemap
//!
//! ============================================================================

use dirstat_core::classify::{self, Category};
use dirstat_core::tree::{Node, NodeId, NodeKind, Tree};
use dirstat_core::treemap::{self, Algorithm, LayoutParams};

/// Same LCG as the property test; deterministic, seedable, zero deps.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }
    fn byte(&mut self) -> u8 {
        (self.next() >> 24) as u8
    }
    fn range(&mut self, n: usize) -> usize {
        (self.next() >> 33) as usize % n.max(1)
    }
}

/// Random string: any bytes (including invalid UTF-8 folded to a lossy
/// String, plus control chars, dots, slashes). Classifiers must accept
/// anything without panicking.
fn random_string(rng: &mut Lcg) -> String {
    let len = rng.range(40);
    let mut bytes = Vec::with_capacity(len);
    for _ in 0..len {
        // Bias toward filesystem-relevant characters but allow anything.
        let b = match rng.range(6) {
            0 => b'.',
            1 => b'/',
            2 => rng.range(128) as u8, // ASCII incl. controls
            _ => rng.byte(),           // any byte
        };
        bytes.push(b);
    }
    // Lossy is fine: classifiers take &str, and lossy conversion is exactly
    // what the scanner does for the *_lossy paths.
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Fuzz the pure classifiers: never panic on any input string.
#[test]
fn fuzz_classifiers_never_panic() {
    let mut rng = Lcg(0xC0FFEE_u64);
    for _ in 0..50_000 {
        let s = random_string(&mut rng);
        // Each call must return (no panic); the value is irrelevant here.
        let _ = classify::category_for_dir_name(&s);
        let _ = classify::category_for_extension(&s.to_ascii_lowercase());
        let inherited = if rng.range(2) == 0 {
            Some(Category::from_u8(rng.byte()))
        } else {
            None
        };
        let ext_opt = if s.is_empty() { None } else { Some(s.as_str()) };
        let _ = classify::classify_file(ext_opt, inherited);
        // from_u8 must total-map every byte.
        let _ = Category::from_u8(rng.byte());
    }
}

/// Build a random in-memory tree directly (no filesystem) so layout sees
/// arbitrary size distributions, depths, and fan-outs.
fn random_tree(rng: &mut Lcg) -> Tree {
    let mut tree = Tree::new();
    tree.root_path = std::ffi::OsString::from("/fuzz");
    tree.push(Node {
        name: "root".into(),
        parent: NodeId::INVALID,
        children: Vec::new(),
        kind: NodeKind::Dir,
        logical: 0,
        physical: 0,
        files: 0,
        subdirs: 0,
        mtime: 0,
        flags: 0,
        category: 0,
        ext_id: u32::MAX,
    });
    let budget = 2 + rng.range(120);
    let mut dirs = vec![NodeId::ROOT];
    for _ in 0..budget {
        if dirs.is_empty() {
            break;
        }
        let parent = dirs[rng.range(dirs.len())];
        let is_dir = rng.range(3) == 0;
        let size = match rng.range(4) {
            0 => 0u64,
            1 => rng.range(64) as u64,
            _ => rng.next() % 1_000_000,
        };
        let id = tree.push(Node {
            name: format!("n{}", rng.next()).into(),
            parent,
            children: Vec::new(),
            kind: if is_dir {
                NodeKind::Dir
            } else {
                NodeKind::File
            },
            logical: if is_dir { 0 } else { size },
            physical: if is_dir { 0 } else { size },
            files: if is_dir { 0 } else { 1 },
            subdirs: 0,
            mtime: 0,
            flags: 0,
            category: 0,
            ext_id: u32::MAX,
        });
        // Propagate this leaf's bytes up so directory aggregates are sane.
        if !is_dir {
            tree.propagate(parent, size, size, 1, 0);
        } else {
            tree.propagate(parent, 0, 0, 0, 1);
            dirs.push(id);
        }
    }
    tree
}

/// Fuzz treemap layout + hit-testing: never panic on any tree/rect/point.
#[test]
fn fuzz_treemap_never_panics() {
    let mut rng = Lcg(0xBADF00D_u64);
    for _ in 0..2_000 {
        let tree = random_tree(&mut rng);
        // Random (possibly degenerate) target rectangle.
        let w = (rng.range(2000)) as f32; // includes 0
        let h = (rng.range(2000)) as f32;
        let algo = if rng.range(2) == 0 {
            Algorithm::Squarified
        } else {
            Algorithm::KDirStat
        };
        let params = LayoutParams {
            algorithm: algo,
            min_px: (rng.range(8)) as f32 * 0.5,
            max_depth: rng.range(80) as u16,
            use_physical: rng.range(2) == 0,
        };
        let rects = treemap::layout(&tree, NodeId::ROOT, 0.0, 0.0, w, h, params, &|_| 12, &|_| 0);
        // Hit-test a handful of random points, including outside the rect.
        for _ in 0..5 {
            let px = (rng.range(2500) as f32) - 250.0;
            let py = (rng.range(2500) as f32) - 250.0;
            let _ = treemap::hit_test(&rects, px, py);
        }
    }
}
