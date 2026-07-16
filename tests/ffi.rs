//! EVC-FFI-1 / EVC-FFI-SAFE-1: drive the C ABI end-to-end from the "host"
//! side — init → scan → navigate → layout → hit-test → refresh → free —
//! and prove panics never unwind across the boundary.

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
