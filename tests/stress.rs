//! ============================================================================
//! FILE: tests/stress.rs
//!
//! ============================================================================
//!
//! # Purpose
//! The contention pattern the progressive UI actually creates (dirstat-core#7):
//! a large parallel scan read by many threads at once — snapshotting
//! aggregates, laying out the treemap, enumerating children — plus a
//! cancel/restart cycle. The functional suite proves correctness on small
//! trees; this proves the only-grow / determinism / thread-safety invariants
//! hold under real concurrency and at scale.
//!
//! Size is env-tunable (`DIRSTAT_STRESS_FILES`, default 40k) so CI stays
//! fast while a developer can crank it up locally.
//!
//! # Upstream dependencies
//! - dirstat_core::scan (Scan/ScanOptions/Model), tree (NodeId/SortKey),
//!   treemap (layout)
//! - std::thread / std::sync::atomic — the reader fleet and its stop flag
//!
//! ============================================================================

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dirstat_core::scan::{Scan, ScanOptions};
use dirstat_core::tree::{NodeId, SortKey};
use dirstat_core::treemap::{self, LayoutParams};

struct Fx {
    root: PathBuf,
    files: usize,
    bytes_per: usize,
}
impl Fx {
    /// Build a mixed breadth+depth tree with a known total, once, and reuse
    /// it across runs. Layout: `d{0..W}/e{0..W}/f{n}.dat`, each file the same
    /// size, so ground-truth totals are exact.
    fn build(name: &str, files: usize) -> Fx {
        let root =
            std::env::temp_dir().join(format!("dirstat-stress-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let bytes_per = 64usize;
        let w = 32usize; // branching factor per level
        for i in 0..files {
            let d = i % w;
            let e = (i / w) % w;
            let dir = root.join(format!("d{d}/e{e}"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(format!("f{i}.dat")), vec![0u8; bytes_per]).unwrap();
        }
        Fx {
            root,
            files,
            bytes_per,
        }
    }
    fn total_bytes(&self) -> u64 {
        (self.files * self.bytes_per) as u64
    }
}
impl Drop for Fx {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn file_count() -> usize {
    std::env::var("DIRSTAT_STRESS_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(40_000)
}

/// Scan a large tree while a fleet of readers hammers it, then verify exact
/// totals. The readers assert the only-grow + items invariants continuously
/// (design 1d); the treemap and sorted_children calls exercise the
/// documented "reads are safe during a scan" contract (CORE-FFI-SAFE-4).
#[test]
fn large_scan_under_concurrent_readers() {
    let fx = Fx::build("readers", file_count());
    let scan = Scan::begin(&fx.root, ScanOptions::default(), None).unwrap();
    let model = Arc::clone(&scan.model);

    let stop = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let m = Arc::clone(&model);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut last = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let tree = m.tree.read().unwrap();
                    let root = tree.get(NodeId::ROOT).unwrap();
                    assert!(root.logical >= last, "totals must only grow");
                    assert_eq!(root.items(), root.files + root.subdirs);
                    last = root.logical;
                    // Layout + a sorted enumeration on the live tree.
                    let _ = treemap::layout(
                        &tree,
                        NodeId::ROOT,
                        0.0,
                        0.0,
                        600.0,
                        400.0,
                        LayoutParams::default(),
                        &|_| 12,
                        &|_| 0,
                    );
                    let _ = tree.sorted_children(NodeId::ROOT, SortKey::Size, true);
                }
            })
        })
        .collect();

    let mut scan = scan;
    scan.join();
    stop.store(true, Ordering::Relaxed);
    for r in readers {
        r.join().unwrap();
    }

    let tree = model.tree.read().unwrap();
    assert_eq!(tree.get(NodeId::ROOT).unwrap().logical, fx.total_bytes());
    assert_eq!(tree.get(NodeId::ROOT).unwrap().files, fx.files as u64);
}

/// Determinism at scale (CORE-SCAN-9): three independent scans of the same
/// tree agree on totals regardless of thread interleaving.
#[test]
fn deterministic_totals_at_scale() {
    let fx = Fx::build("determ", file_count().min(20_000));
    let mut seen = Vec::new();
    for _ in 0..3 {
        let mut s = Scan::begin(&fx.root, ScanOptions::default(), None).unwrap();
        s.join();
        let tree = s.model.tree.read().unwrap();
        let r = tree.get(NodeId::ROOT).unwrap();
        seen.push((r.logical, r.physical, r.files, r.subdirs, tree.len()));
    }
    assert!(
        seen.windows(2).all(|w| w[0] == w[1]),
        "runs diverged: {seen:?}"
    );
    assert_eq!(seen[0].0, fx.total_bytes());
}

/// Cancel mid-scan stops fast and leaves a consistent partial tree; a fresh
/// scan afterward still completes to the exact total.
#[test]
fn cancel_then_rescan_at_scale() {
    let fx = Fx::build("cancel", file_count());
    let scan = Scan::begin(&fx.root, ScanOptions::default(), None).unwrap();
    // Let it get going, then cancel.
    std::thread::sleep(Duration::from_millis(20));
    scan.cancel();
    let t0 = Instant::now();
    let mut scan = scan;
    scan.join();
    assert!(
        t0.elapsed() < Duration::from_secs(1),
        "cancel must stop < 1s"
    );
    {
        let tree = scan.model.tree.read().unwrap();
        for i in 0..tree.len() {
            let n = tree.get(NodeId::from_index(i)).unwrap();
            assert_eq!(n.items(), n.files + n.subdirs);
        }
    }
    drop(scan);
    // A clean rescan reaches the full total.
    let mut s2 = Scan::begin(&fx.root, ScanOptions::default(), None).unwrap();
    s2.join();
    assert_eq!(
        s2.model
            .tree
            .read()
            .unwrap()
            .get(NodeId::ROOT)
            .unwrap()
            .logical,
        fx.total_bytes()
    );
}

/// Same read-during-scan pattern driven once through the FFI, validating the
/// thread-safety contract where hosts actually touch it.
#[test]
fn ffi_reads_during_scan() {
    use dirstat_core::ffi::*;
    use std::ffi::CString;

    let fx = Fx::build("ffi", file_count().min(20_000));
    let root_c = CString::new(fx.root.to_str().unwrap()).unwrap();
    let scan = ds_scan_begin(
        root_c.as_ptr(),
        std::ptr::null(),
        None,
        std::ptr::null_mut(),
    );
    assert!(!scan.is_null());
    let model = ds_scan_model(scan);

    // Hammer treemap layout + stats from another thread while scanning.
    let model_addr = model as usize;
    let reader = std::thread::spawn(move || {
        let model = model_addr as *const DsModel;
        for _ in 0..200 {
            let mut stats = DsScanStats {
                items: 0,
                bytes: 0,
                complete: 0,
                error_count: 0,
            };
            assert_eq!(ds_model_stats(model, &mut stats), 0);
            let root = ds_model_root(model);
            if root != 0 {
                let mut rects: *mut DsTmRect = std::ptr::null_mut();
                let mut len = 0usize;
                if ds_treemap_layout(model, root, 500.0, 300.0, 1, 2.0, 1, &mut rects, &mut len)
                    == 0
                {
                    ds_treemap_free(rects, len);
                }
            }
        }
    });

    ds_scan_join(scan);
    reader.join().unwrap();

    let mut stats = DsScanStats {
        items: 0,
        bytes: 0,
        complete: 0,
        error_count: 0,
    };
    ds_model_stats(model, &mut stats);
    assert_eq!(stats.bytes, fx.total_bytes());
    ds_model_free(model);
    ds_scan_free(scan);
}
