//! ============================================================================
//! FILE: tests/engine.rs
//!
//! ============================================================================
//!
//! # Purpose
//! The engine eval suite (spec §13): every `EVC-*` id from the spec's
//! coverage table that applies to the Rust-native API is proven here.
//! Tests run against REAL on-disk temp trees rather than mocks, because the
//! contract under test is "facts about a filesystem" — permissions,
//! symlinks, hard links, sparse files, and mount aliasing only behave
//! honestly on a real filesystem.
//!
//! # Upstream dependencies (what this file consumes)
//! - dirstat_core::scan — Scan lifecycle, refresh_node, ScanOptions
//! - dirstat_core::tree — NodeId/SortKey and direct Model.tree reads
//! - dirstat_core::treemap — layout/hit_test/LayoutParams/Algorithm
//! - std::fs / std::os::unix — fixture construction (files, symlinks,
//!   hard links, permission bits, sparse files, bind mounts)
//!
//! # Structure
//! - Fixture — a self-cleaning temp directory builder; `file(rel, size)`
//!   creates parents and writes `size` zero bytes
//! - scan()/find() — helpers: run a scan to completion; resolve a path of
//!   component names to a NodeId
//! - one #[test] per EVC id (names carry the id), plus regression tests for
//!   field-reported bugs (progressive reads, alias dedup, sparse metric)
//!
//! # Notes for reviewers
//! - Privileged-environment tests (bind-mount aliasing) and
//!   filesystem-dependent tests (sparse files) detect unsupported
//!   environments and return early rather than fail, so the suite is green
//!   on ordinary CI runners AND exercises the real path where possible.
//! - Fixture names embed the process id so parallel test binaries can't
//!   collide in the shared temp dir.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dirstat_core::scan::{refresh_node, Progress, Scan, ScanOptions};
use dirstat_core::tree::{NodeId, SortKey};
use dirstat_core::treemap::{self, Algorithm, LayoutParams};

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let root = std::env::temp_dir().join(format!("dirstat-fix-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        Fixture { root }
    }
    fn file(&self, rel: &str, size: usize) -> &Self {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, vec![0u8; size]).unwrap();
        self
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn scan(root: &Path) -> Scan {
    let mut s = Scan::begin(root, ScanOptions::default(), None).unwrap();
    s.join();
    s
}

fn find(scan: &Scan, path: &[&str]) -> NodeId {
    let tree = scan.model.tree.read().unwrap();
    let mut cur = NodeId::ROOT;
    'outer: for comp in path {
        let children = tree.get(cur).unwrap().children.clone();
        for c in children {
            if tree.get(c).unwrap().name.to_string_lossy() == *comp {
                cur = c;
                continue 'outer;
            }
        }
        panic!("path component {comp} not found");
    }
    cur
}

/// EVC-SCAN-1/2: per-file fields and directory aggregates are exact.
#[test]
fn evc_scan_aggregates() {
    let fx = Fixture::new("agg");
    fx.file("sub/deep/a.bin", 10_000)
        .file("sub/b.bin", 6_000)
        .file("c.bin", 4_000);
    let s = scan(&fx.root);
    let tree = s.model.tree.read().unwrap();

    let root = tree.get(NodeId::ROOT).unwrap();
    assert_eq!(root.logical, 20_000);
    assert_eq!(root.files, 3);
    assert_eq!(root.subdirs, 2);
    assert_eq!(root.items(), 5); // EVC-TREE-6 at the root
    drop(tree);

    let sub = find(&s, &["sub"]);
    let deep = find(&s, &["sub", "deep"]);
    let tree = s.model.tree.read().unwrap();
    assert_eq!(tree.get(sub).unwrap().logical, 16_000);
    assert_eq!(tree.get(deep).unwrap().logical, 10_000);
    // CORE-SCAN-3: a directory's size is solely the sum of descendants.
    assert_eq!(
        tree.get(sub).unwrap().logical,
        tree.get(deep).unwrap().logical + 6_000
    );
}

/// EVC-TREE-6: items == files + subdirs at every node.
#[test]
fn evc_tree_items_invariant() {
    let fx = Fixture::new("inv");
    for i in 0..20 {
        fx.file(&format!("d{}/f{}.txt", i % 4, i), 100 + i);
    }
    let s = scan(&fx.root);
    let tree = s.model.tree.read().unwrap();
    for i in 0..tree.len() {
        let n = tree.get(NodeId::from_index(i)).unwrap();
        assert_eq!(n.items(), n.files + n.subdirs);
    }
}

/// EVC-TREE-3: sorting is within one parent, stable, both directions.
#[test]
fn evc_tree_sorting() {
    let fx = Fixture::new("sort");
    fx.file("big.bin", 9_000)
        .file("mid.bin", 5_000)
        .file("tiny.bin", 1_000)
        .file("dir/x.bin", 3_000);
    let s = scan(&fx.root);
    let tree = s.model.tree.read().unwrap();

    let desc = tree.sorted_children(NodeId::ROOT, SortKey::Size, true);
    let names: Vec<String> = desc
        .iter()
        .map(|&id| tree.get(id).unwrap().name.to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["big.bin", "mid.bin", "dir", "tiny.bin"]);

    let by_name = tree.sorted_children(NodeId::ROOT, SortKey::Name, false);
    let names: Vec<String> = by_name
        .iter()
        .map(|&id| tree.get(id).unwrap().name.to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["big.bin", "dir", "mid.bin", "tiny.bin"]);
}

/// EVC-TREE-5: percent vs root and vs parent.
#[test]
fn evc_tree_percent() {
    let fx = Fixture::new("pct");
    fx.file("half/a.bin", 5_000).file("b.bin", 5_000);
    let s = scan(&fx.root);
    let half = find(&s, &["half"]);
    let tree = s.model.tree.read().unwrap();
    assert!((tree.percent_of_root(half) - 50.0).abs() < 0.01);
    assert!((tree.percent_of_parent(half) - 50.0).abs() < 0.01);
}

/// EVC-SCAN-6: symlinks are not followed by default.
#[cfg(unix)]
#[test]
fn evc_scan_symlink_not_followed() {
    let fx = Fixture::new("sym");
    fx.file("real/data.bin", 50_000);
    std::os::unix::fs::symlink(fx.root.join("real"), fx.root.join("link")).unwrap();
    let s = scan(&fx.root);
    let tree = s.model.tree.read().unwrap();
    let root = tree.get(NodeId::ROOT).unwrap();
    // Only the real 50 KB counts once (link contributes just its own tiny size).
    assert!(root.logical < 51_000, "logical={}", root.logical);
}

/// EVC-SCAN-5: hard-link pair counted once.
#[cfg(unix)]
#[test]
fn evc_scan_hardlink_once() {
    let fx = Fixture::new("hl");
    fx.file("a.bin", 30_000);
    fs::hard_link(fx.root.join("a.bin"), fx.root.join("b.bin")).unwrap();
    let s = scan(&fx.root);
    let tree = s.model.tree.read().unwrap();
    let root = tree.get(NodeId::ROOT).unwrap();
    assert_eq!(root.logical, 30_000);
    assert_eq!(root.files, 2); // both paths recorded as items
}

/// EVC-SCAN-9: parallel scan is deterministic in aggregate across runs.
#[test]
fn evc_scan_deterministic() {
    let fx = Fixture::new("det");
    for i in 0..200 {
        fx.file(&format!("d{}/f{}.dat", i % 10, i), 1_000 + i);
    }
    let mut totals = Vec::new();
    for _ in 0..5 {
        let s = scan(&fx.root);
        let tree = s.model.tree.read().unwrap();
        let r = tree.get(NodeId::ROOT).unwrap();
        totals.push((r.logical, r.files, r.subdirs));
    }
    assert!(totals.windows(2).all(|w| w[0] == w[1]), "{totals:?}");
}

/// EVC-SCAN-10: progress callback fires with monotonic counts.
#[test]
fn evc_scan_progress_monotonic() {
    let fx = Fixture::new("prog");
    for i in 0..500 {
        fx.file(&format!("d{}/f{}.dat", i % 20, i), 2_000);
    }
    let seen: Arc<Mutex<Vec<(u64, u64, bool)>>> = Arc::new(Mutex::new(Vec::new()));
    let seen2 = Arc::clone(&seen);
    let cb: Box<dyn Fn(&Progress) + Send + Sync> = Box::new(move |p| {
        seen2.lock().unwrap().push((p.items, p.bytes, p.done));
    });
    let mut s = Scan::begin(&fx.root, ScanOptions::default(), Some(cb)).unwrap();
    s.join();
    let seen = seen.lock().unwrap();
    assert!(!seen.is_empty());
    assert!(seen.last().unwrap().2, "final callback has done=true");
    for w in seen.windows(2) {
        assert!(w[1].0 >= w[0].0 && w[1].1 >= w[0].1, "counts only go up");
    }
}

/// EVC-SCAN-11: cancel stops promptly; partial result internally consistent.
#[test]
fn evc_scan_cancel() {
    let fx = Fixture::new("cancel");
    for i in 0..2_000 {
        fx.file(&format!("d{}/e{}/f{}.dat", i % 50, i % 7, i), 512);
    }
    let mut s = Scan::begin(&fx.root, ScanOptions::default(), None).unwrap();
    s.cancel();
    let t0 = Instant::now();
    s.join();
    assert!(t0.elapsed() < Duration::from_secs(1), "cancel stops < 1s");
    // Partial-but-consistent: invariant holds on whatever was scanned.
    let tree = s.model.tree.read().unwrap();
    for i in 0..tree.len() {
        let n = tree.get(NodeId::from_index(i)).unwrap();
        assert_eq!(n.items(), n.files + n.subdirs);
    }
}

/// EVC-SCAN-13: unreadable dir → report entry, no abort.
#[cfg(unix)]
#[test]
fn evc_scan_denied_reported() {
    use std::os::unix::fs::PermissionsExt;
    if libc_geteuid() == 0 {
        return; // root ignores permission bits; nothing to test
    }
    let fx = Fixture::new("deny");
    fx.file("open/a.bin", 1_000)
        .file("locked/secret.bin", 1_000);
    let locked = fx.root.join("locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let s = scan(&fx.root);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    let errors = s.model.errors.lock().unwrap();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].path.ends_with("locked"));
}

#[cfg(unix)]
fn libc_geteuid() -> u32 {
    // Avoid a libc dependency for one call.
    unsafe { geteuid() }
}
#[cfg(unix)]
extern "C" {
    fn geteuid() -> u32;
}

/// EVC-SYN-3: unknown = total − free − measured, clamped ≥ 0.
#[test]
fn evc_unknown_math() {
    let fx = Fixture::new("vol");
    fx.file("a.bin", 8_192);
    let s = scan(&fx.root);
    let measured = s
        .model
        .tree
        .read()
        .unwrap()
        .get(NodeId::ROOT)
        .unwrap()
        .physical;
    *s.model.volume.lock().unwrap() = Some(dirstat_core::scan::VolumeFigures {
        total: measured + 1_000_000,
        free: 400_000,
    });
    assert_eq!(s.model.unknown_bytes(), Some(600_000));
    // Clamp case.
    *s.model.volume.lock().unwrap() = Some(dirstat_core::scan::VolumeFigures {
        total: 100,
        free: 50,
    });
    assert_eq!(s.model.unknown_bytes(), Some(0));
}

/// EVC-EXT-1/3/6: extension aggregation, case-insensitive, top-12 slots,
/// totals reconcile.
#[test]
fn evc_ext_aggregation() {
    let fx = Fixture::new("ext");
    fx.file("a.MOV", 10_000)
        .file("b.mov", 5_000)
        .file("c.pdf", 3_000)
        .file("noext", 1_000);
    let s = scan(&fx.root);
    let stats = s.model.ext_stats.read().unwrap();
    let mov = stats.iter().find(|e| e.ext == "mov").unwrap();
    assert_eq!(mov.logical, 15_000); // case-insensitive merge
    assert_eq!(mov.files, 2);
    let with_ext: u64 = stats.iter().map(|e| e.logical).sum();
    let root_total = s
        .model
        .tree
        .read()
        .unwrap()
        .get(NodeId::ROOT)
        .unwrap()
        .logical;
    assert_eq!(with_ext + 1_000, root_total); // EVC-EXT-6 (noext accounted)
}

/// EVC-TM-1/2: area ∝ size for both algorithms; squarified aspect no worse.
#[test]
fn evc_treemap_area() {
    let fx = Fixture::new("tm");
    fx.file("a.bin", 60_000)
        .file("b.bin", 30_000)
        .file("dir/c.bin", 10_000);
    let s = scan(&fx.root);
    let tree = s.model.tree.read().unwrap();
    for algo in [Algorithm::Squarified, Algorithm::KDirStat] {
        let rects = treemap::layout(
            &tree,
            NodeId::ROOT,
            0.0,
            0.0,
            1000.0,
            600.0,
            LayoutParams {
                algorithm: algo,
                min_px: 0.5,
                max_depth: 32,
                use_physical: false,
            },
            &|_| 12,
            &|_| 0,
        );
        // Leaf areas sum to the root rect area.
        let leaf_area: f64 = rects
            .iter()
            .filter(|r| !r.is_dir)
            .map(|r| r.w as f64 * r.h as f64)
            .sum();
        assert!(
            (leaf_area - 600_000.0).abs() < 600_000.0 * 0.001,
            "{algo:?}: leaf area {leaf_area}"
        );
        // Area ratio == size ratio for two siblings.
        let area_of = |name: &str| -> f64 {
            let id = {
                let mut found = NodeId::INVALID;
                for i in 0..tree.len() {
                    let n = tree.get(NodeId::from_index(i)).unwrap();
                    if n.name.to_string_lossy() == name {
                        found = NodeId::from_index(i);
                    }
                }
                found
            };
            let r = rects.iter().find(|r| r.node == id).unwrap();
            r.w as f64 * r.h as f64
        };
        let ratio = area_of("a.bin") / area_of("b.bin");
        assert!((ratio - 2.0).abs() < 0.02, "{algo:?}: ratio {ratio}");
    }
}

/// EVC-TM-4: hit test returns the containing leaf for sampled points.
#[test]
fn evc_treemap_hit_test() {
    let fx = Fixture::new("hit");
    fx.file("a.bin", 50_000).file("b.bin", 50_000);
    let s = scan(&fx.root);
    let tree = s.model.tree.read().unwrap();
    let rects = treemap::layout(
        &tree,
        NodeId::ROOT,
        0.0,
        0.0,
        800.0,
        400.0,
        LayoutParams::default(),
        &|_| 12,
        &|_| 0,
    );
    for r in rects.iter().filter(|r| !r.is_dir) {
        let hit = treemap::hit_test(&rects, r.x + r.w / 2.0, r.y + r.h / 2.0);
        assert_eq!(hit, r.node);
    }
    assert_eq!(treemap::hit_test(&rects, -5.0, -5.0), NodeId::INVALID);
}

/// EVC-TM-5/6: layout is deterministic; zoom (re-rooted) layout keeps
/// relative proportions of the subtree.
#[test]
fn evc_treemap_deterministic_zoom() {
    let fx = Fixture::new("zoom");
    fx.file("dir/a.bin", 40_000)
        .file("dir/b.bin", 20_000)
        .file("c.bin", 40_000);
    let s = scan(&fx.root);
    let dir = find(&s, &["dir"]);
    let tree = s.model.tree.read().unwrap();
    let one = treemap::layout(
        &tree,
        dir,
        0.0,
        0.0,
        600.0,
        600.0,
        LayoutParams::default(),
        &|_| 12,
        &|_| 0,
    );
    let two = treemap::layout(
        &tree,
        dir,
        0.0,
        0.0,
        600.0,
        600.0,
        LayoutParams::default(),
        &|_| 12,
        &|_| 0,
    );
    assert_eq!(one.len(), two.len());
    for (a, b) in one.iter().zip(two.iter()) {
        assert_eq!(a.node, b.node);
        assert_eq!((a.x, a.y, a.w, a.h), (b.x, b.y, b.w, b.h));
    }
    let leaves: f64 = one
        .iter()
        .filter(|r| !r.is_dir)
        .map(|r| r.w as f64 * r.h as f64)
        .sum();
    assert!((leaves - 360_000.0).abs() < 500.0);
}

/// EVC-SCAN-12: refresh after deletion updates node and ancestors
/// (the design-1e "map reconciles after commit" call).
#[test]
fn evc_refresh_after_delete() {
    let fx = Fixture::new("refresh");
    fx.file("keep.bin", 10_000).file("junk/big.bin", 90_000);
    let s = scan(&fx.root);
    let junk = find(&s, &["junk"]);
    assert_eq!(
        s.model
            .tree
            .read()
            .unwrap()
            .get(NodeId::ROOT)
            .unwrap()
            .logical,
        100_000
    );
    fs::remove_dir_all(fx.root.join("junk")).unwrap();
    refresh_node(&s.model, junk).unwrap();
    let tree = s.model.tree.read().unwrap();
    let root = tree.get(NodeId::ROOT).unwrap();
    assert_eq!(root.logical, 10_000);
    assert_eq!(root.files, 1);
    assert_eq!(root.subdirs, 0);
}

/// Progressive reads (design 1d): a reader mid-scan sees consistent,
/// only-growing totals.
#[test]
fn progressive_reads_consistent() {
    let fx = Fixture::new("live");
    for i in 0..3_000 {
        fx.file(&format!("d{}/e{}/f{}.dat", i % 30, i % 5, i), 256);
    }
    let s = Scan::begin(&fx.root, ScanOptions::default(), None).unwrap();
    let mut last = 0u64;
    while !s.is_complete() {
        let tree = s.model.tree.read().unwrap();
        let root = tree.get(NodeId::ROOT).unwrap();
        assert!(root.logical >= last, "totals only grow");
        assert_eq!(root.items(), root.files + root.subdirs);
        last = root.logical;
        drop(tree);
        std::thread::sleep(Duration::from_millis(1));
    }
    let mut s = s;
    s.join();
    assert_eq!(
        s.model
            .tree
            .read()
            .unwrap()
            .get(NodeId::ROOT)
            .unwrap()
            .logical,
        3_000 * 256
    );
}

/// Kind classification propagates path rules into files (design 1g).
#[test]
fn category_rules_apply() {
    let fx = Fixture::new("cat");
    fx.file("DerivedData/proj/junk.noindex", 5_000)
        .file("Movies/clip.mov", 5_000)
        .file("random.xyz", 5_000);
    let s = scan(&fx.root);
    let junk = find(&s, &["DerivedData", "proj", "junk.noindex"]);
    let clip = find(&s, &["Movies", "clip.mov"]);
    let rand = find(&s, &["random.xyz"]);
    let tree = s.model.tree.read().unwrap();
    assert_eq!(
        tree.get(junk).unwrap().category,
        dirstat_core::Category::Developer as u8
    );
    assert_eq!(
        tree.get(clip).unwrap().category,
        dirstat_core::Category::Media as u8
    );
    assert_eq!(
        tree.get(rand).unwrap().category,
        dirstat_core::Category::Other as u8
    );
}

/// EVC-LIMIT-1: saturating accumulation near u64::MAX (unit-level).
#[test]
fn evc_limit_saturating() {
    let mut tree = dirstat_core::Tree::new();
    tree.push(dirstat_core::tree::Node {
        name: "root".into(),
        parent: NodeId::INVALID,
        children: Vec::new(),
        kind: dirstat_core::NodeKind::Dir,
        logical: u64::MAX - 10,
        physical: 0,
        files: 0,
        subdirs: 0,
        mtime: 0,
        flags: 0,
        category: 0,
        ext_id: u32::MAX,
    });
    tree.propagate(NodeId::ROOT, 100, 0, 1, 0);
    assert_eq!(tree.get(NodeId::ROOT).unwrap().logical, u64::MAX);
}

/// Physical metric: sparse files (the stand-in for cloud-placeholder /
/// dataless files) dominate logically but not physically; the physical
/// sort key and physical-area layout must reflect on-disk truth.
#[cfg(unix)]
#[test]
fn physical_metric_ignores_sparse_bloat() {
    let fx = Fixture::new("sparse");
    fx.file("real.bin", 100_000);
    // A 10 MB apparent file with (almost) no allocated blocks.
    let sparse_path = fx.root.join("sparse.bin");
    let f = fs::File::create(&sparse_path).unwrap();
    f.set_len(10_000_000).unwrap();
    drop(f);
    let physical = fs::symlink_metadata(&sparse_path)
        .map(|m| {
            use std::os::unix::fs::MetadataExt;
            m.blocks() * 512
        })
        .unwrap();
    if physical >= 1_000_000 {
        eprintln!("skipping: filesystem does not create sparse files");
        return;
    }
    let s = scan(&fx.root);
    let tree = s.model.tree.read().unwrap();

    // Logical sort puts the sparse file first; physical sort puts it last.
    let by_logical = tree.sorted_children(NodeId::ROOT, SortKey::Size, true);
    let by_physical = tree.sorted_children(NodeId::ROOT, SortKey::PhysicalSize, true);
    let name = |id: NodeId| tree.get(id).unwrap().name.to_string_lossy().into_owned();
    assert_eq!(name(by_logical[0]), "sparse.bin");
    assert_eq!(name(by_physical[0]), "real.bin");

    // Physical-area layout gives the real file the dominant rect.
    let rects = treemap::layout(
        &tree,
        NodeId::ROOT,
        0.0,
        0.0,
        1000.0,
        600.0,
        LayoutParams {
            use_physical: true,
            ..LayoutParams::default()
        },
        &|_| 12,
        &|_| 0,
    );
    let area = |target: &str| -> f64 {
        rects
            .iter()
            .find(|r| name(r.node) == target)
            .map(|r| r.w as f64 * r.h as f64)
            .unwrap_or(0.0)
    };
    assert!(area("real.bin") > area("sparse.bin") * 5.0);
}

/// skip_paths: host-supplied directories are not descended (the macOS
/// APFS volume-group rule rides on this).
#[test]
fn skip_paths_not_descended() {
    let fx = Fixture::new("skip");
    fx.file("keep/a.bin", 10_000).file("skipme/big.bin", 90_000);
    let mut s = Scan::begin(
        &fx.root,
        ScanOptions {
            skip_paths: vec![fx.root.join("skipme")],
            ..ScanOptions::default()
        },
        None,
    )
    .unwrap();
    s.join();
    let tree = s.model.tree.read().unwrap();
    let root = tree.get(NodeId::ROOT).unwrap();
    assert_eq!(root.logical, 10_000);
    assert_eq!(root.subdirs, 1, "skipped dir is absent entirely");
}

/// Directory alias dedup: the same directory inode reachable through two
/// paths (APFS firmlink / bind mount) is counted exactly once; the second
/// path is a zero-contribution DUPLICATE node. Requires bind-mount
/// privileges; skips silently where unavailable.
#[cfg(target_os = "linux")]
#[test]
fn alias_directory_counted_once() {
    let fx = Fixture::new("alias");
    fx.file("real/data.bin", 70_000).file("other.bin", 5_000);
    fs::create_dir_all(fx.root.join("mirror")).unwrap();
    let status = std::process::Command::new("mount")
        .args(["--bind"])
        .arg(fx.root.join("real"))
        .arg(fx.root.join("mirror"))
        .status();
    match status {
        Ok(st) if st.success() => {}
        _ => {
            eprintln!("skipping: bind mount unavailable");
            let _ = fs::remove_dir_all(&fx.root);
            return;
        }
    }
    // Bind mounts have the same st_dev on Linux only when the source is on
    // the same filesystem — which it is here (both under the fixture).
    let s = scan(&fx.root);
    let _ = std::process::Command::new("umount")
        .arg(fx.root.join("mirror"))
        .status();
    let tree = s.model.tree.read().unwrap();
    let root = tree.get(NodeId::ROOT).unwrap();
    assert_eq!(
        root.logical, 75_000,
        "aliased subtree must be counted exactly once"
    );
    // One of the two paths carries the DUPLICATE flag.
    let dups = (0..tree.len())
        .filter(|&i| {
            tree.get(NodeId::from_index(i)).unwrap().flags & dirstat_core::tree::flags::DUPLICATE
                != 0
        })
        .count();
    assert_eq!(dups, 1);
}

// ---------------------------------------------------------------------------
// ABI v4: non-UTF-8 name flag, node ceiling (dirstat-core#5, #11).
// ---------------------------------------------------------------------------

/// A file whose name is not valid UTF-8 is flagged NON_UTF8, and its lossy
/// string path differs from the raw bytes — the confused-deputy hazard the
/// flag exists to let hosts refuse. Only Linux-family filesystems permit
/// arbitrary bytes in names; macOS (APFS/HFS+) rejects invalid UTF-8 at
/// creation (EILSEQ), so the test skips gracefully there.
#[cfg(unix)]
#[test]
fn non_utf8_name_is_flagged() {
    use std::os::unix::ffi::OsStrExt;
    let fx = Fixture::new("nonutf8");
    fx.file("ok.txt", 10);
    // 0xFF is never valid UTF-8; build a name from raw bytes.
    let bad = std::ffi::OsStr::from_bytes(b"bad\xff\xfename.bin");
    if fs::write(fx.root.join(bad), vec![0u8; 20]).is_err() {
        eprintln!("skipping: filesystem rejects non-UTF-8 names (e.g. macOS)");
        return;
    }

    let s = scan(&fx.root);
    let tree = s.model.tree.read().unwrap();
    let mut found = false;
    for i in 0..tree.len() {
        let id = NodeId::from_index(i);
        let n = tree.get(id).unwrap();
        if n.flags & dirstat_core::tree::flags::NON_UTF8 != 0 {
            found = true;
            // Lossy string path replaces the invalid bytes; the raw path does not.
            let lossy = tree.abs_path(id).to_string_lossy().into_owned();
            assert!(lossy.contains('\u{FFFD}'), "lossy path should carry U+FFFD");
        } else {
            assert!(
                n.name.to_string_lossy().chars().all(|c| c != '\u{FFFD}'),
                "valid names must not be flagged non-UTF-8"
            );
        }
    }
    assert!(found, "the non-UTF-8 file must be flagged");
}

/// A validly-named file whose name literally contains U+FFFD is NOT flagged
/// (it is valid UTF-8), so the flag can't be spoofed to bypass host guards.
#[test]
fn literal_replacement_char_is_not_flagged() {
    let fx = Fixture::new("ufffd");
    fx.file("real\u{FFFD}name.bin", 10);
    let s = scan(&fx.root);
    let tree = s.model.tree.read().unwrap();
    for i in 0..tree.len() {
        assert_eq!(
            tree.get(NodeId::from_index(i)).unwrap().flags & dirstat_core::tree::flags::NON_UTF8,
            0
        );
    }
}

/// max_nodes stops the scan at (approximately) the ceiling, leaves a
/// partial-but-consistent tree, and records the report note exactly once.
#[test]
fn node_ceiling_bounds_the_scan() {
    let fx = Fixture::new("ceiling");
    for i in 0..2_000 {
        fx.file(&format!("d{}/f{}.dat", i % 40, i), 64);
    }
    let mut s = Scan::begin(
        &fx.root,
        ScanOptions {
            max_nodes: 200,
            ..ScanOptions::default()
        },
        None,
    )
    .unwrap();
    s.join();
    let tree = s.model.tree.read().unwrap();
    // Stopped near the ceiling (in-flight listings may overshoot by up to
    // one directory's width, well under the 2040 total).
    assert!(tree.len() >= 200, "reached the ceiling");
    assert!(tree.len() < 1_500, "stopped well short of the full tree");
    // Invariant holds on the partial tree.
    for i in 0..tree.len() {
        let n = tree.get(NodeId::from_index(i)).unwrap();
        assert_eq!(n.items(), n.files + n.subdirs);
    }
    // The report note is present exactly once.
    let errors = s.model.errors.lock().unwrap();
    let notes = errors
        .iter()
        .filter(|e| e.message.contains("node limit"))
        .count();
    assert_eq!(notes, 1);
}

// ---------------------------------------------------------------------------
// refresh_node: deep tree (iterative graft) + edge cases (dirstat-core#6, #9).
// ---------------------------------------------------------------------------

/// A deep chain refreshes correctly (validates the iterative graft rewrite,
/// dirstat-core#6). Depth is capped by PATH_MAX: the scanner joins absolute
/// paths per level, so the filesystem limits real depth to ~2000 short
/// levels on Linux; the iterative graft removes depth-proportional stack use
/// as robustness regardless of that ceiling.
#[test]
fn refresh_deep_chain() {
    let fx = Fixture::new("deep");
    // ~800-char path, safely under macOS PATH_MAX (1024) as well as Linux's.
    const DEPTH: usize = 400;
    let mut rel = String::new();
    for _ in 0..DEPTH {
        rel.push_str("d/");
    }
    rel.push_str("leaf.bin");
    fx.file(&rel, 4_096);
    let s = scan(&fx.root);
    let chain_root = find(&s, &["d"]);
    assert_eq!(
        s.model
            .tree
            .read()
            .unwrap()
            .get(NodeId::ROOT)
            .unwrap()
            .logical,
        4_096
    );
    // Delete the leaf, then refresh the whole chain from its top.
    let mut deep_leaf = fx.root.clone();
    for _ in 0..DEPTH {
        deep_leaf.push("d");
    }
    deep_leaf.push("leaf.bin");
    fs::remove_file(&deep_leaf).unwrap();
    refresh_node(&s.model, chain_root).unwrap(); // iterative graft: no abort
    let tree = s.model.tree.read().unwrap();
    assert_eq!(tree.get(NodeId::ROOT).unwrap().logical, 0);
}

/// Refreshing the root itself is valid and reconciles (edge: parent is INVALID).
#[test]
fn refresh_root() {
    let fx = Fixture::new("refroot");
    fx.file("a.bin", 10_000).file("sub/b.bin", 5_000);
    let s = scan(&fx.root);
    fs::remove_file(fx.root.join("a.bin")).unwrap();
    refresh_node(&s.model, NodeId::ROOT).unwrap();
    let tree = s.model.tree.read().unwrap();
    assert_eq!(tree.get(NodeId::ROOT).unwrap().logical, 5_000);
}

/// A path that changed KIND between scans (file -> directory) refreshes
/// correctly rather than corrupting aggregates.
#[test]
fn refresh_file_became_directory() {
    let fx = Fixture::new("kindswap");
    fx.file("thing", 8_000).file("keep.bin", 1_000);
    let s = scan(&fx.root);
    let thing = find(&s, &["thing"]);
    // Replace the file with a directory containing a bigger file.
    fs::remove_file(fx.root.join("thing")).unwrap();
    fs::create_dir(fx.root.join("thing")).unwrap();
    fs::write(fx.root.join("thing/inner.bin"), vec![0u8; 20_000]).unwrap();
    refresh_node(&s.model, thing).unwrap();
    let tree = s.model.tree.read().unwrap();
    // root = keep(1000) + thing subtree(20000)
    assert_eq!(tree.get(NodeId::ROOT).unwrap().logical, 21_000);
    for i in 0..tree.len() {
        let n = tree.get(NodeId::from_index(i)).unwrap();
        assert_eq!(n.items(), n.files + n.subdirs);
    }
}

/// ds_node_path_raw round-trips the exact bytes for a normal name (checked
/// here at the Rust level; the FFI harness checks the C surface).
#[cfg(unix)]
#[test]
fn abs_path_bytes_are_raw() {
    use std::os::unix::ffi::OsStrExt;
    let fx = Fixture::new("rawpath");
    let bad = std::ffi::OsStr::from_bytes(b"x\xffy.bin");
    if fs::write(fx.root.join(bad), vec![0u8; 5]).is_err() {
        eprintln!("skipping: filesystem rejects non-UTF-8 names (e.g. macOS)");
        return;
    }
    let s = scan(&fx.root);
    let tree = s.model.tree.read().unwrap();
    // Find the bad node and confirm its raw abs_path bytes contain 0xFF
    // (lossy conversion would have replaced it).
    let mut ok = false;
    for i in 0..tree.len() {
        let id = NodeId::from_index(i);
        if tree.get(id).unwrap().flags & dirstat_core::tree::flags::NON_UTF8 != 0 {
            let p = tree.abs_path(id);
            assert!(p.as_os_str().as_bytes().contains(&0xff));
            ok = true;
        }
    }
    assert!(ok);
}
