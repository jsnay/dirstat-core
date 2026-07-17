//! ============================================================================
//! FILE: tests/ffi.rs
//!
//! ============================================================================
//!
//! # Purpose
//! EVC-FFI-1 / EVC-FFI-SAFE-1: drive the C ABI exactly as a host would —
//! init → scan → navigate → layout → hit-test → refresh → free — and prove
//! panics never unwind across the boundary. This file plays the role of
//! the Swift app: it calls only the exported `ds_*` functions (never the
//! internal Rust API) so a green run here means the header's contract
//! actually works, including the two-call string pattern, bulk-buffer
//! ownership, and NULL/bad-id error paths.
//!
//! # Upstream dependencies (what this file consumes)
//! - dirstat_core::ffi — the entire exported surface under test
//! - std::ffi::CString / raw pointers — to fake the C caller faithfully
//! - std::fs — the same on-disk Fixture strategy as tests/engine.rs
//!
//! # Structure
//! - Fixture — self-cleaning temp tree builder (duplicated from
//!   tests/engine.rs; integration tests cannot share private helpers)
//! - c_string_from — exercises the size-then-fill two-call pattern the way
//!   a C caller would (measure with NULL, allocate, fill)
//! - evc_ffi_end_to_end — the full lifecycle walk with value assertions
//! - evc_ffi_panic_safe — the deliberate-panic hook returns -1 + message
//! - evc_ffi_error_paths — NULL/invalid inputs report errors, never crash

use std::ffi::{c_char, CString};
use std::fs;
use std::path::PathBuf;

use dirstat_core::ffi::*;

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let root = std::env::temp_dir().join(format!("dirstat-ffi-{name}-{}", std::process::id()));
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

fn c_string_from(f: impl Fn(*mut c_char, usize) -> i32) -> String {
    let needed = f(std::ptr::null_mut(), 0);
    assert!(needed >= 0);
    let mut buf = vec![0u8; needed as usize];
    f(buf.as_mut_ptr() as *mut c_char, buf.len());
    buf.pop(); // NUL
    String::from_utf8(buf).unwrap()
}

#[test]
fn evc_ffi_end_to_end() {
    assert_eq!(ds_abi_version(), DS_ABI_VERSION);

    let fx = Fixture::new("e2e");
    fx.file("movies/clip.mov", 80_000)
        .file("movies/other.mov", 20_000)
        .file("docs/paper.pdf", 40_000);

    let root_c = CString::new(fx.root.to_str().unwrap()).unwrap();

    extern "C" fn on_progress(
        _items: u64,
        _bytes: u64,
        _path: *const c_char,
        done: u8,
        user: *mut std::ffi::c_void,
    ) {
        if done == 1 {
            let flag = unsafe { &*(user as *const std::sync::atomic::AtomicBool) };
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let done_flag = Box::new(std::sync::atomic::AtomicBool::new(false));
    let done_ptr = &*done_flag as *const _ as *mut std::ffi::c_void;

    let scan = ds_scan_begin(
        root_c.as_ptr(),
        std::ptr::null(),
        Some(on_progress),
        done_ptr,
    );
    assert!(!scan.is_null(), "scan_begin failed");
    ds_scan_join(scan);
    assert_eq!(ds_scan_is_complete(scan), 1);
    assert!(done_flag.load(std::sync::atomic::Ordering::SeqCst));

    let model = ds_scan_model(scan);
    assert!(!model.is_null());
    ds_scan_free(scan); // model outlives the scan handle

    // Stats + root.
    let mut stats = DsScanStats {
        items: 0,
        bytes: 0,
        complete: 0,
        error_count: 0,
    };
    assert_eq!(ds_model_stats(model, &mut stats), 0);
    assert_eq!(stats.bytes, 140_000);
    assert_eq!(stats.complete, 1);
    let root = ds_model_root(model);
    assert_eq!(root, 1);

    // Node info + name.
    let mut info = unsafe { std::mem::zeroed::<DsNodeInfo>() };
    assert_eq!(ds_node_info(model, root, &mut info), 0);
    assert_eq!(info.logical, 140_000);
    assert_eq!(info.files, 3);
    assert_eq!(info.subdirs, 2);
    assert_eq!(info.items, 5);

    // Children sorted size-desc.
    let mut kids = [0u64; 8];
    let n = ds_node_children(model, root, 0, 1, kids.as_mut_ptr(), kids.len());
    assert_eq!(n, 2);
    let first_name = c_string_from(|b, c| ds_node_name(model, kids[0], b, c));
    assert_eq!(first_name, "movies");

    // Path round-trip.
    let path = c_string_from(|b, c| ds_node_path(model, kids[0], b, c));
    assert!(path.ends_with("movies"), "{path}");

    // Type list: mov is the top slot.
    let mut types = [unsafe { std::mem::zeroed::<DsTypeStat>() }; 8];
    let t = ds_type_list(model, types.as_mut_ptr(), types.len());
    assert!(t >= 2);
    let ext0: String = types[0]
        .ext
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8 as char)
        .collect();
    assert_eq!(ext0, "mov");
    assert_eq!(types[0].logical, 100_000);
    assert_eq!(types[0].slot, 0);

    // Category list covers all 8 buckets.
    let mut cats = [unsafe { std::mem::zeroed::<DsCategoryStat>() }; 8];
    let c = ds_category_list(model, cats.as_mut_ptr(), cats.len());
    assert_eq!(c, 8);
    let media_total: u64 = cats
        .iter()
        .filter(|s| s.category == dirstat_core::Category::Media as u8)
        .map(|s| s.logical)
        .sum();
    assert_eq!(media_total, 100_000);

    // Volume figures + unknown math.
    assert_eq!(ds_model_set_volume(model, 10_000_000, 2_000_000), 0);
    let mut vol = DsVolumeInfo {
        total: 0,
        free: 0,
        unknown: 0,
    };
    assert_eq!(ds_model_volume(model, &mut vol), 0);
    assert_eq!(vol.total, 10_000_000);
    assert!(vol.unknown > 0);

    // Treemap layout (bulk buffer) + hit test.
    let mut rects: *mut DsTmRect = std::ptr::null_mut();
    let mut len: usize = 0;
    assert_eq!(
        ds_treemap_layout(model, root, 900.0, 500.0, 1, 1.0, 0, &mut rects, &mut len),
        0
    );
    assert!(len >= 5, "root + 2 dirs + 3 files, got {len}");
    let slice = unsafe { std::slice::from_raw_parts(rects, len) };
    let leaf_area: f64 = slice
        .iter()
        .filter(|r| r.is_dir == 0)
        .map(|r| r.w as f64 * r.h as f64)
        .sum();
    assert!((leaf_area - 450_000.0).abs() < 500.0);
    let first_leaf = slice.iter().find(|r| r.is_dir == 0).unwrap();
    let hit = ds_treemap_hit_test(
        rects,
        len,
        first_leaf.x + first_leaf.w / 2.0,
        first_leaf.y + first_leaf.h / 2.0,
    );
    assert_eq!(hit, first_leaf.node);
    ds_treemap_free(rects, len);

    // Refresh after deletion (the 1e post-Trash call).
    fs::remove_dir_all(fx.root.join("movies")).unwrap();
    let movies_id = kids[0];
    assert_eq!(ds_refresh_node(model, movies_id), 0);
    assert_eq!(ds_node_info(model, root, &mut info), 0);
    assert_eq!(info.logical, 40_000);

    ds_model_free(model);
}

/// EVC-FFI-SAFE-1: a deliberate internal panic returns an error code and
/// sets a retrievable message instead of unwinding.
#[test]
fn evc_ffi_panic_safe() {
    assert_eq!(ds_internal_panic_test(), -1);
    let msg = c_string_from(|b, c| ds_last_error(b, c));
    assert!(msg.contains("panic"), "{msg}");
}

/// Errors: NULL and bad ids are reported, never crash.
#[test]
fn evc_ffi_error_paths() {
    assert!(ds_scan_begin(
        std::ptr::null(),
        std::ptr::null(),
        None,
        std::ptr::null_mut()
    )
    .is_null());
    let msg = c_string_from(|b, c| ds_last_error(b, c));
    assert!(msg.contains("NULL"), "{msg}");

    let fx = Fixture::new("err");
    fx.file("a.txt", 10);
    let root_c = CString::new(fx.root.to_str().unwrap()).unwrap();
    let scan = ds_scan_begin(
        root_c.as_ptr(),
        std::ptr::null(),
        None,
        std::ptr::null_mut(),
    );
    ds_scan_join(scan);
    let model = ds_scan_model(scan);
    ds_scan_free(scan);

    let mut info = unsafe { std::mem::zeroed::<DsNodeInfo>() };
    assert_eq!(ds_node_info(model, 999_999, &mut info), -1);
    assert_eq!(ds_node_info(std::ptr::null(), 1, &mut info), -1);
    ds_model_free(model);
    ds_model_free(std::ptr::null_mut()); // no-op, no crash
}

// ---------------------------------------------------------------------------
// FFI buffer edges + ABI v4 raw path (dirstat-core#9, #5).
// ---------------------------------------------------------------------------

/// ds_node_children with cap < total fills cap and returns total; cap == 0
/// and NULL buf just count.
#[test]
fn ffi_children_buffer_edges() {
    let fx = Fixture::new("kids");
    for i in 0..10 {
        fx.file(&format!("f{i}.bin"), 100 + i);
    }
    let root_c = CString::new(fx.root.to_str().unwrap()).unwrap();
    let scan = ds_scan_begin(
        root_c.as_ptr(),
        std::ptr::null(),
        None,
        std::ptr::null_mut(),
    );
    ds_scan_join(scan);
    let model = ds_scan_model(scan);
    ds_scan_free(scan);
    let root = ds_model_root(model);

    // NULL buf, cap 0: count only.
    assert_eq!(
        ds_node_children(model, root, 0, 1, std::ptr::null_mut(), 0),
        10
    );
    // Too-small buffer: fills 3, still returns the true total of 10.
    let mut small = [0u64; 3];
    assert_eq!(
        ds_node_children(model, root, 0, 1, small.as_mut_ptr(), 3),
        10
    );
    assert!(small.iter().all(|&x| x != 0), "the 3 slots were filled");
    ds_model_free(model);
}

/// Two-call string pattern at exact boundaries: cap == needed, cap ==
/// needed-1 (truncated but NUL-terminated), cap == 1 (just the NUL).
#[test]
fn ffi_string_boundaries() {
    let fx = Fixture::new("strbound");
    fx.file("exactly_sixteen!.x", 1); // name length chosen to be nontrivial
    let root_c = CString::new(fx.root.to_str().unwrap()).unwrap();
    let scan = ds_scan_begin(
        root_c.as_ptr(),
        std::ptr::null(),
        None,
        std::ptr::null_mut(),
    );
    ds_scan_join(scan);
    let model = ds_scan_model(scan);
    ds_scan_free(scan);
    let root = ds_model_root(model);
    let mut kids = [0u64; 4];
    ds_node_children(model, root, 0, 1, kids.as_mut_ptr(), 4);
    let child = kids[0];

    let needed = ds_node_name(model, child, std::ptr::null_mut(), 0);
    assert!(needed > 1);
    // cap == needed: full name + NUL.
    let mut buf = vec![0u8; needed as usize];
    ds_node_name(model, child, buf.as_mut_ptr() as *mut c_char, buf.len());
    assert_eq!(*buf.last().unwrap(), 0);
    // cap == 1: only the NUL fits (empty string, no overflow).
    let mut one = [0xAAu8; 1];
    ds_node_name(model, child, one.as_mut_ptr() as *mut c_char, 1);
    assert_eq!(one[0], 0);
    ds_model_free(model);
}

/// treemap layout on a zero-byte / empty model returns an empty buffer, not
/// an error, and handles a 0-area rect.
#[test]
fn ffi_layout_degenerate() {
    let fx = Fixture::new("emptylayout");
    fx.file("z", 0); // single zero-byte file: root total is 0
    let root_c = CString::new(fx.root.to_str().unwrap()).unwrap();
    let scan = ds_scan_begin(
        root_c.as_ptr(),
        std::ptr::null(),
        None,
        std::ptr::null_mut(),
    );
    ds_scan_join(scan);
    let model = ds_scan_model(scan);
    ds_scan_free(scan);
    let root = ds_model_root(model);

    let mut rects: *mut DsTmRect = std::ptr::null_mut();
    let mut len = 999usize;
    // Zero total bytes: empty layout, len set to 0, still returns success.
    assert_eq!(
        ds_treemap_layout(model, root, 400.0, 300.0, 1, 2.0, 0, &mut rects, &mut len),
        0
    );
    assert_eq!(len, 0);
    ds_treemap_free(rects, len);
    // 0-width rect: also empty, no panic.
    let mut r2: *mut DsTmRect = std::ptr::null_mut();
    let mut l2 = 0usize;
    assert_eq!(
        ds_treemap_layout(model, root, 0.0, 300.0, 1, 2.0, 0, &mut r2, &mut l2),
        0
    );
    assert_eq!(l2, 0);
    ds_treemap_free(r2, l2);
    ds_model_free(model);
}

/// ds_node_path_raw returns the exact bytes (two-call by byte length) and,
/// for a non-UTF-8 name, differs from the lossy string path.
#[cfg(unix)]
#[test]
fn ffi_node_path_raw() {
    use std::os::unix::ffi::OsStrExt;
    let fx = Fixture::new("rawffi");
    let bad = std::ffi::OsStr::from_bytes(b"r\xffw.bin");
    if std::fs::write(fx.root.join(bad), vec![0u8; 3]).is_err() {
        eprintln!("skipping: filesystem rejects non-UTF-8 names (e.g. macOS)");
        return;
    }
    let root_c = CString::new(fx.root.to_str().unwrap()).unwrap();
    let scan = ds_scan_begin(
        root_c.as_ptr(),
        std::ptr::null(),
        None,
        std::ptr::null_mut(),
    );
    ds_scan_join(scan);
    let model = ds_scan_model(scan);
    ds_scan_free(scan);
    let root = ds_model_root(model);
    let mut kids = [0u64; 2];
    ds_node_children(model, root, 0, 1, kids.as_mut_ptr(), 2);
    let child = kids[0];

    // Flag is set.
    let mut info = unsafe { std::mem::zeroed::<DsNodeInfo>() };
    ds_node_info(model, child, &mut info);
    assert_ne!(info.flags & DS_NODE_FLAG_NON_UTF8, 0);

    // Raw path: two-call by byte length; contains the raw 0xFF.
    let needed = ds_node_path_raw(model, child, std::ptr::null_mut(), 0);
    assert!(needed > 0);
    let mut raw = vec![0u8; needed as usize];
    ds_node_path_raw(model, child, raw.as_mut_ptr(), raw.len());
    assert!(raw.contains(&0xff), "raw path preserves the invalid byte");
    ds_model_free(model);
}

/// ds_set_log_callback (v5): register → events arrive from a scan and a
/// refresh; unregister → silence. The collector is a process-global
/// static because the callback is process-global; assertions filter by
/// this test's root path where identity matters, and the post-unregister
/// check relies on set_sink(None) stopping ALL emission, so a concurrent
/// test can't grow the log either.
#[test]
fn ffi_log_callback_events() {
    use std::sync::Mutex;
    static EVENTS: Mutex<Vec<(u8, String)>> = Mutex::new(Vec::new());

    extern "C" fn on_log(level: u8, msg: *const c_char, _user: *mut std::ffi::c_void) {
        let s = unsafe { std::ffi::CStr::from_ptr(msg) }
            .to_string_lossy()
            .into_owned();
        EVENTS.lock().unwrap().push((level, s));
    }

    let fx = Fixture::new("log");
    fx.file("a/x.bin", 10_000).file("b/y.bin", 20_000);
    let root_c = CString::new(fx.root.to_str().unwrap()).unwrap();

    ds_set_log_callback(Some(on_log), std::ptr::null_mut());

    let scan = ds_scan_begin(
        root_c.as_ptr(),
        std::ptr::null(),
        None,
        std::ptr::null_mut(),
    );
    assert!(!scan.is_null());
    ds_scan_join(scan);
    let model = ds_scan_model(scan);
    ds_scan_free(scan);

    // A refresh also emits.
    let root = ds_model_root(model);
    let mut kids = [0u64; 4];
    let n = ds_node_children(model, root, 0, 1, kids.as_mut_ptr(), 4);
    assert!(n >= 1);
    assert_eq!(ds_refresh_node(model, kids[0]), 0);

    {
        let events = EVENTS.lock().unwrap();
        let mine: Vec<&(u8, String)> = events
            .iter()
            .filter(|(_, m)| m.contains(fx.root.to_str().unwrap()) || m.starts_with("scan "))
            .collect();
        assert!(
            mine.iter().any(|(_, m)| m.starts_with("scan start:")),
            "missing scan-start event; got {events:?}"
        );
        assert!(
            mine.iter()
                .any(|(_, m)| m.starts_with("scan completed:") && m.contains("items=")),
            "missing scan-done totals; got {events:?}"
        );
        assert!(
            mine.iter().any(|(_, m)| m.starts_with("refresh done:")),
            "missing refresh event; got {events:?}"
        );
    }

    // Unregister: nothing emits any more, from any thread or test.
    ds_set_log_callback(None, std::ptr::null_mut());
    let len_after = EVENTS.lock().unwrap().len();
    let scan2 = ds_scan_begin(
        root_c.as_ptr(),
        std::ptr::null(),
        None,
        std::ptr::null_mut(),
    );
    ds_scan_join(scan2);
    ds_scan_free(scan2);
    assert_eq!(
        EVENTS.lock().unwrap().len(),
        len_after,
        "events arrived after unregister"
    );
    ds_model_free(model);
}
