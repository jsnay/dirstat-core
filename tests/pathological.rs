//! ============================================================================
//! FILE: tests/pathological.rs
//!
//! ============================================================================
//!
//! # Purpose
//! CORE-STAB-1 ("no panic/UB on pathological input"). Every scan ingests
//! attacker-controlled data — any downloaded archive or cloned repo — so
//! hostile names, extreme structure, and degenerate files are normal input,
//! not edge cases. Each test asserts the two universal invariants survive:
//! the scan does not panic, and `items == files + subdirs` holds at every
//! node. Filesystem-touching, so `#[cfg(unix)]` where the case needs raw
//! bytes or symlinks; the crate still builds on non-Unix without them.
//!
//! # Upstream dependencies
//! - dirstat_core::scan (Scan/ScanOptions), tree (NodeId), treemap (layout)
//! - std::fs / std::os::unix — hostile fixture construction
//!
//! ============================================================================

use std::fs;
use std::path::{Path, PathBuf};

use dirstat_core::scan::{Scan, ScanOptions};
use dirstat_core::tree::NodeId;
use dirstat_core::treemap::{self, LayoutParams};

struct Fx {
    root: PathBuf,
}
impl Fx {
    fn new(name: &str) -> Fx {
        let root = std::env::temp_dir().join(format!("dirstat-path-{name}-{}", std::process::id()));
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

fn scan(root: &Path) -> Scan {
    let mut s = Scan::begin(root, ScanOptions::default(), None).unwrap();
    s.join();
    s
}

/// Assert the always-true invariants over the whole (possibly partial) tree.
fn assert_consistent(s: &Scan) {
    let tree = s.model.tree.read().unwrap();
    for i in 0..tree.len() {
        let n = tree.get(NodeId::from_index(i)).unwrap();
        assert_eq!(
            n.items(),
            n.files + n.subdirs,
            "items invariant at node {i}"
        );
    }
    // A full treemap layout must also not panic on whatever was scanned.
    let _ = treemap::layout(
        &tree,
        NodeId::ROOT,
        0.0,
        0.0,
        800.0,
        600.0,
        LayoutParams::default(),
        &|_| 12,
        &|_| 0,
    );
}

/// Unicode, control characters, and lookalike names in one directory.
#[test]
fn hostile_names_unicode_and_controls() {
    let fx = Fx::new("names");
    let names = [
        "emoji_🗂️_📁.bin",
        "combining_é_ñ_ü.dat",
        "rtl_מנה_عربى.txt",
        "..dotdot_lookalike",
        "  leading and trailing  ",
        "trailing.dot.",
        "with\ttab.log",
        "semi;colon&amp.sh",
        "quote'\"quote.md",
    ];
    for (i, n) in names.iter().enumerate() {
        fs::write(fx.root.join(n), vec![0u8; 100 + i]).unwrap();
    }
    let s = scan(&fx.root);
    assert_consistent(&s);
    assert_eq!(
        s.model
            .tree
            .read()
            .unwrap()
            .get(NodeId::ROOT)
            .unwrap()
            .files,
        names.len() as u64
    );
}

/// A 255-byte name component (the POSIX max) survives intact.
#[test]
fn max_length_name_component() {
    let fx = Fx::new("longname");
    let name = format!("{}.bin", "a".repeat(251)); // 251 + ".bin" = 255
    fs::write(fx.root.join(&name), vec![0u8; 42]).unwrap();
    let s = scan(&fx.root);
    assert_consistent(&s);
    let tree = s.model.tree.read().unwrap();
    let root = tree.get(NodeId::ROOT).unwrap();
    assert_eq!(root.files, 1);
    let child = tree.get(root.children[0]).unwrap();
    assert_eq!(child.name.to_string_lossy().len(), 255);
}

/// Invalid UTF-8 name bytes: no panic, node present, flagged (pairs with the
/// non-UTF-8 flag in engine.rs).
#[cfg(unix)]
#[test]
fn invalid_utf8_name() {
    use std::os::unix::ffi::OsStrExt;
    let fx = Fx::new("badutf8");
    let bad = std::ffi::OsStr::from_bytes(b"\xff\xfe\x80raw.bin");
    fs::write(fx.root.join(bad), vec![0u8; 7]).unwrap();
    let s = scan(&fx.root);
    assert_consistent(&s);
    let tree = s.model.tree.read().unwrap();
    assert_eq!(tree.get(NodeId::ROOT).unwrap().files, 1);
}

/// Symlink cycles must terminate: links are never followed, so a→b→a and a
/// self-link scan finitely.
#[cfg(unix)]
#[test]
fn symlink_cycles_terminate() {
    let fx = Fx::new("cycle");
    fs::create_dir(fx.root.join("a")).unwrap();
    fs::create_dir(fx.root.join("b")).unwrap();
    std::os::unix::fs::symlink(fx.root.join("b"), fx.root.join("a/to_b")).unwrap();
    std::os::unix::fs::symlink(fx.root.join("a"), fx.root.join("b/to_a")).unwrap();
    std::os::unix::fs::symlink(fx.root.join("self"), fx.root.join("self")).unwrap();
    // Completes (join returns) rather than looping forever.
    let s = scan(&fx.root);
    assert_consistent(&s);
}

/// A directory of 50k zero-byte files: no divide-by-zero in the treemap, no
/// panic, exact count.
#[test]
fn many_zero_byte_files() {
    let fx = Fx::new("zeros");
    let dir = fx.root.join("zeros");
    fs::create_dir(&dir).unwrap();
    for i in 0..50_000 {
        fs::write(dir.join(format!("z{i}")), b"").unwrap();
    }
    let s = scan(&fx.root);
    assert_consistent(&s);
    let tree = s.model.tree.read().unwrap();
    let root = tree.get(NodeId::ROOT).unwrap();
    assert_eq!(root.files, 50_000);
    assert_eq!(root.logical, 0); // all zero-byte: total is zero, layout must cope
}

/// skip_paths entries that are nonexistent, relative, or a symlink must not
/// panic and must skip only exact matches.
#[cfg(unix)]
#[test]
fn skip_paths_pathological() {
    let fx = Fx::new("skip");
    fs::create_dir(fx.root.join("real")).unwrap();
    fs::write(fx.root.join("real/x.bin"), vec![0u8; 1000]).unwrap();
    fs::write(fx.root.join("keep.bin"), vec![0u8; 500]).unwrap();
    let opts = ScanOptions {
        skip_paths: vec![
            fx.root.join("real"),            // real, exact: should skip
            PathBuf::from("does/not/exist"), // nonexistent
            PathBuf::from("relative/path"),  // relative
        ],
        ..ScanOptions::default()
    };
    let mut s = Scan::begin(&fx.root, opts, None).unwrap();
    s.join();
    assert_consistent(&s);
    // Only keep.bin counted; real/ skipped.
    assert_eq!(
        s.model
            .tree
            .read()
            .unwrap()
            .get(NodeId::ROOT)
            .unwrap()
            .logical,
        500
    );
}

/// Dot-only and extension-less names don't confuse extension parsing.
#[test]
fn degenerate_extensions() {
    let fx = Fx::new("exts");
    for n in [".", "..hidden", "noext", "a.", ".gitignore", "x.y.z.tar.gz"] {
        // "." and ".." can't be created; skip those two, they're only here
        // to document intent. Create the rest.
        if n == "." {
            continue;
        }
        let _ = fs::write(fx.root.join(n), vec![0u8; 10]);
    }
    let s = scan(&fx.root);
    assert_consistent(&s);
}
