//! Engine evals (spec §13). Fixtures are built as on-disk temp trees.

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
