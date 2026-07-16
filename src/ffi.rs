//! C ABI (CORE-FFI-*). The only module that touches raw pointers.
//!
//! Contract summary (full details in the generated `dirstat_core.h`):
//! * Every function is panic-safe: Rust panics are caught and converted to
//!   error returns (CORE-FFI-SAFE-1); details via [`ds_last_error`].
//! * `DsScan` / `DsModel` are opaque. `ds_scan_model` may be called while
//!   the scan is running: the returned model is safe for concurrent reads
//!   and its numbers only grow (the progressive-UI contract, design 1d).
//! * Strings cross as UTF-8, NUL-terminated, via a two-call size-then-fill
//!   pattern: pass `cap == 0` to learn the required buffer size.
//! * `ds_treemap_layout` returns one engine-owned contiguous buffer; free it
//!   with `ds_treemap_free`. Never per-rect calls (CORE-FFI-6).
//! * Thread safety (CORE-FFI-SAFE-4): all `ds_model_*` / `ds_treemap_*`
//!   reads are safe from any thread, concurrently with a running scan.
//!   `ds_refresh_node` requires no concurrent scan on the same model.
//!   Progress callbacks arrive on engine threads — hop to your UI thread.

// C-ABI entry points take raw pointers by contract; every function
// null-checks its inputs and the remaining validity requirements (buffer
// capacities, ownership) are documented in the generated header. Marking
// them `unsafe` would change the exported signature for no caller benefit.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::cell::RefCell;
use std::ffi::{c_char, c_void, CStr};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::classify::{Category, CATEGORY_COUNT};
use crate::scan::{Model, Progress, Scan, ScanOptions, VolumeFigures};
use crate::tree::{age_bucket, NodeId, SortKey};
use crate::treemap::{self, Algorithm, LayoutParams, LayoutRect};

/// Bump on every ABI change; the Swift wrapper refuses to run against a
/// mismatched header/library pair (CORE-FFI-SAFE-3 / APP-FFI-6).
/// v2: `DsScanOptions` gained `skip_paths`/`skip_paths_len`;
/// `DS_NODE_FLAG_DUPLICATE` added.
pub const DS_ABI_VERSION: u32 = 2;

thread_local! {
    static LAST_ERROR: RefCell<String> = const { RefCell::new(String::new()) };
}

fn set_error(msg: impl Into<String>) {
    LAST_ERROR.with(|e| *e.borrow_mut() = msg.into());
}

fn guard<T>(default: T, f: impl FnOnce() -> T) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(p) => {
            let msg = p
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panic in dirstat-core".to_string());
            set_error(format!("internal panic: {msg}"));
            default
        }
    }
}

/// Copy `s` into `(buf, cap)` NUL-terminated; returns bytes needed
/// including the NUL. Two-call pattern: `cap == 0` just measures.
fn fill_str(s: &str, buf: *mut c_char, cap: usize) -> i32 {
    let bytes = s.as_bytes();
    let needed = bytes.len() + 1;
    if !buf.is_null() && cap > 0 {
        let n = bytes.len().min(cap - 1);
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf as *mut u8, n);
            *buf.add(n) = 0;
        }
    }
    needed as i32
}

// ---------------------------------------------------------------------------
// Opaque handles
// ---------------------------------------------------------------------------

pub struct DsScan {
    scan: Scan,
    // Keep the callback alive as long as the scan.
    _cb: Option<Box<CallbackState>>,
}

pub struct DsModel {
    model: Arc<Model>,
}

struct CallbackState {
    cb: DsProgressCallback,
    user: *mut c_void,
}
unsafe impl Send for CallbackState {}
unsafe impl Sync for CallbackState {}

/// Progress callback: `(items, bytes, current_path_utf8, done, user_data)`.
/// Called on an engine thread at a throttled (~10 Hz) rate plus a final
/// `done = 1` call. The path pointer is only valid during the call.
pub type DsProgressCallback =
    extern "C" fn(items: u64, bytes: u64, path: *const c_char, done: u8, user: *mut c_void);

/// Node flag bits mirrored into the C header (values asserted equal to the
/// internal `tree::flags` constants by a unit test).
pub const DS_NODE_FLAG_HIDDEN: u32 = 1;
pub const DS_NODE_FLAG_HARDLINK_DUP: u32 = 2;
pub const DS_NODE_FLAG_UNREADABLE: u32 = 4;
pub const DS_NODE_FLAG_SCANNING: u32 = 8;
/// Directory alias (same device+inode already scanned via another path,
/// e.g. an APFS firmlink): listed but contributes nothing.
pub const DS_NODE_FLAG_DUPLICATE: u32 = 16;

#[cfg(test)]
mod flag_sync {
    use crate::tree::flags;

    #[test]
    fn ffi_flags_match_internal() {
        assert_eq!(super::DS_NODE_FLAG_HIDDEN, flags::HIDDEN);
        assert_eq!(super::DS_NODE_FLAG_HARDLINK_DUP, flags::HARDLINK_DUP);
        assert_eq!(super::DS_NODE_FLAG_UNREADABLE, flags::UNREADABLE);
        assert_eq!(super::DS_NODE_FLAG_SCANNING, flags::SCANNING);
        assert_eq!(super::DS_NODE_FLAG_DUPLICATE, flags::DUPLICATE);
    }
}

/// Scan options; zero-initialize for defaults.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DsScanOptions {
    pub cross_filesystems: u8,
    pub follow_symlinks: u8,
    pub exclude_hidden: u8,
    pub max_concurrency: u32,
    /// Optional array of `skip_paths_len` NUL-terminated UTF-8 absolute
    /// directory paths the scan must not descend into (platform knowledge
    /// the host supplies — e.g. `/System/Volumes/Data` when scanning `/` on
    /// macOS so the APFS volume group is not traversed twice). May be NULL
    /// when `skip_paths_len` is 0. Only read during `ds_scan_begin`.
    pub skip_paths: *const *const c_char,
    pub skip_paths_len: usize,
}

/// Flat per-node facts (CORE-FFI-4). Strings via `ds_node_name`/`ds_node_path`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DsNodeInfo {
    pub id: u64,
    pub parent: u64,
    pub logical: u64,
    pub physical: u64,
    pub files: u64,
    pub subdirs: u64,
    pub items: u64,
    pub mtime: i64,
    pub child_count: u32,
    pub flags: u32,
    /// 0 file, 1 dir, 2 symlink.
    pub kind: u8,
    /// Kind bucket (design 1g); see `ds_category_name`.
    pub category: u8,
    /// 0 this week … 4 older (vs scan start).
    pub age_bucket: u8,
    /// 0..11 top-12 extension slot, 12 = other/none (CORE-EXT-3).
    pub ext_slot: u8,
}

/// One treemap rectangle (bulk buffer element).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DsTmRect {
    pub node: u64,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub depth: u16,
    pub is_dir: u8,
    pub category: u8,
    pub age_bucket: u8,
    pub ext_slot: u8,
}

/// Per-extension aggregate (CORE-EXT-*).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DsTypeStat {
    /// Lowercased extension, NUL-terminated, truncated to fit.
    pub ext: [c_char; 16],
    pub logical: u64,
    pub files: u64,
    /// 0..11 distinct palette slot, 12 = aggregated "other".
    pub slot: u8,
}

/// Per-category aggregate for the legend chips / capacity footer (1b).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DsCategoryStat {
    pub category: u8,
    pub logical: u64,
    pub files: u64,
}

/// Volume reconciliation figures (CORE-SYN-2/3).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DsVolumeInfo {
    pub total: u64,
    pub free: u64,
    /// `max(0, total - free - measured)` — the "unreadable" number.
    pub unknown: u64,
}

/// Live scan totals for progress polling.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DsScanStats {
    pub items: u64,
    pub bytes: u64,
    pub complete: u8,
    pub error_count: u64,
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

/// ABI version of this library; compare with `DS_ABI_VERSION` in the header
/// you compiled against before calling anything else.
#[no_mangle]
pub extern "C" fn ds_abi_version() -> u32 {
    DS_ABI_VERSION
}

/// Retrieve (and keep) the calling thread's last error message.
/// Two-call pattern; returns bytes needed including NUL, 0 if no error.
#[no_mangle]
pub extern "C" fn ds_last_error(buf: *mut c_char, cap: usize) -> i32 {
    LAST_ERROR.with(|e| {
        let e = e.borrow();
        if e.is_empty() {
            0
        } else {
            fill_str(&e, buf, cap)
        }
    })
}

// ---------------------------------------------------------------------------
// Scan
// ---------------------------------------------------------------------------

/// Start scanning `root_utf8`. Returns NULL on error (see `ds_last_error`).
/// `options` may be NULL for defaults. The callback (nullable) fires on
/// engine threads (CORE-FFI-2).
#[no_mangle]
pub extern "C" fn ds_scan_begin(
    root_utf8: *const c_char,
    options: *const DsScanOptions,
    callback: Option<
        extern "C" fn(items: u64, bytes: u64, path: *const c_char, done: u8, user: *mut c_void),
    >,
    user: *mut c_void,
) -> *mut DsScan {
    guard(std::ptr::null_mut(), || {
        if root_utf8.is_null() {
            set_error("root path is NULL");
            return std::ptr::null_mut();
        }
        let root = match unsafe { CStr::from_ptr(root_utf8) }.to_str() {
            Ok(s) => s,
            Err(_) => {
                set_error("root path is not valid UTF-8");
                return std::ptr::null_mut();
            }
        };
        let opts = unsafe { options.as_ref() };
        let skip_paths = opts
            .filter(|o| !o.skip_paths.is_null() && o.skip_paths_len > 0)
            .map(|o| {
                unsafe { std::slice::from_raw_parts(o.skip_paths, o.skip_paths_len) }
                    .iter()
                    .filter(|p| !p.is_null())
                    .filter_map(|&p| unsafe { CStr::from_ptr(p) }.to_str().ok())
                    .map(std::path::PathBuf::from)
                    .collect()
            })
            .unwrap_or_default();
        let scan_opts = ScanOptions {
            cross_filesystems: opts.map(|o| o.cross_filesystems != 0).unwrap_or(false),
            follow_symlinks: opts.map(|o| o.follow_symlinks != 0).unwrap_or(false),
            include_hidden: opts.map(|o| o.exclude_hidden == 0).unwrap_or(true),
            max_concurrency: opts.map(|o| o.max_concurrency as usize).unwrap_or(0),
            skip_paths,
        };

        let cb_state = callback.map(|cb| Box::new(CallbackState { cb, user }));
        let progress = cb_state.as_ref().map(|state| {
            let raw: *const CallbackState = &**state;
            // SAFETY: the CallbackState outlives the Scan (owned by DsScan,
            // dropped after the scan joins in ds_scan_free).
            let raw = raw as usize;
            Box::new(move |p: &Progress| {
                let state = unsafe { &*(raw as *const CallbackState) };
                let path = p.current_path.to_string_lossy();
                let mut bytes = path.as_bytes().to_vec();
                bytes.push(0);
                (state.cb)(
                    p.items,
                    p.bytes,
                    bytes.as_ptr() as *const c_char,
                    p.done as u8,
                    state.user,
                );
            }) as Box<dyn Fn(&Progress) + Send + Sync>
        });

        match Scan::begin(Path::new(root), scan_opts, progress) {
            Ok(scan) => Box::into_raw(Box::new(DsScan {
                scan,
                _cb: cb_state,
            })),
            Err(e) => {
                set_error(format!("cannot scan {root}: {e}"));
                std::ptr::null_mut()
            }
        }
    })
}

/// Request cancellation (CORE-SCAN-11). Partial results remain valid.
#[no_mangle]
pub extern "C" fn ds_scan_cancel(scan: *mut DsScan) {
    guard((), || {
        if let Some(s) = unsafe { scan.as_ref() } {
            s.scan.cancel();
        }
    })
}

#[no_mangle]
pub extern "C" fn ds_scan_is_complete(scan: *const DsScan) -> u8 {
    guard(0, || {
        unsafe { scan.as_ref() }
            .map(|s| s.scan.is_complete() as u8)
            .unwrap_or(0)
    })
}

/// Block until the scan finishes (`scan_await`).
#[no_mangle]
pub extern "C" fn ds_scan_join(scan: *mut DsScan) {
    guard((), || {
        if let Some(s) = unsafe { scan.as_mut() } {
            s.scan.join();
        }
    })
}

/// Get a model handle. Safe to call while the scan runs (progressive reads);
/// each returned handle must be freed with `ds_model_free`.
#[no_mangle]
pub extern "C" fn ds_scan_model(scan: *const DsScan) -> *mut DsModel {
    guard(std::ptr::null_mut(), || match unsafe { scan.as_ref() } {
        Some(s) => Box::into_raw(Box::new(DsModel {
            model: Arc::clone(&s.scan.model),
        })),
        None => {
            set_error("scan handle is NULL");
            std::ptr::null_mut()
        }
    })
}

/// Free the scan handle. Cancels and joins if still running (the model, if
/// retained via `ds_scan_model`, stays valid).
#[no_mangle]
pub extern "C" fn ds_scan_free(scan: *mut DsScan) {
    guard((), || {
        if !scan.is_null() {
            drop(unsafe { Box::from_raw(scan) });
        }
    })
}

// ---------------------------------------------------------------------------
// Model / tree access
// ---------------------------------------------------------------------------

/// Free a model handle (CORE-FFI-3). NodeIds from it are invalid after.
#[no_mangle]
pub extern "C" fn ds_model_free(model: *mut DsModel) {
    guard((), || {
        if !model.is_null() {
            drop(unsafe { Box::from_raw(model) });
        }
    })
}

/// Root node id (always 1 for a non-empty model; 0 = empty/invalid).
#[no_mangle]
pub extern "C" fn ds_model_root(model: *const DsModel) -> u64 {
    guard(0, || {
        let Some(m) = (unsafe { model.as_ref() }) else {
            return 0;
        };
        if m.model.tree.read().unwrap().is_empty() {
            0
        } else {
            NodeId::ROOT.0
        }
    })
}

#[no_mangle]
pub extern "C" fn ds_model_stats(model: *const DsModel, out: *mut DsScanStats) -> i32 {
    guard(-1, || {
        let (Some(m), Some(out)) = (unsafe { model.as_ref() }, unsafe { out.as_mut() }) else {
            set_error("NULL argument");
            return -1;
        };
        *out = DsScanStats {
            items: m.model.items.load(Ordering::Relaxed),
            bytes: m.model.bytes.load(Ordering::Relaxed),
            complete: m.model.complete.load(Ordering::Acquire) as u8,
            error_count: m.model.errors.lock().unwrap().len() as u64,
        };
        0
    })
}

fn ext_slot_map(model: &Model) -> Vec<u8> {
    let stats = model.ext_stats.read().unwrap();
    let mut order: Vec<(u32, u64)> = stats
        .iter()
        .enumerate()
        .map(|(i, s)| (i as u32, s.logical))
        .collect();
    // Deterministic: bytes desc, then extension name asc (CORE-EXT-3).
    order.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| stats[a.0 as usize].ext.cmp(&stats[b.0 as usize].ext))
    });
    let mut map = vec![12u8; stats.len()];
    for (slot, &(ext_id, _)) in order.iter().take(12).enumerate() {
        map[ext_id as usize] = slot as u8;
    }
    map
}

#[no_mangle]
pub extern "C" fn ds_node_info(model: *const DsModel, id: u64, out: *mut DsNodeInfo) -> i32 {
    guard(-1, || {
        let (Some(m), Some(out)) = (unsafe { model.as_ref() }, unsafe { out.as_mut() }) else {
            set_error("NULL argument");
            return -1;
        };
        let tree = m.model.tree.read().unwrap();
        let Some(n) = tree.get(NodeId(id)) else {
            set_error(format!("invalid node id {id}"));
            return -1;
        };
        let slots = ext_slot_map(&m.model);
        *out = DsNodeInfo {
            id,
            parent: n.parent.0,
            logical: n.logical,
            physical: n.physical,
            files: n.files,
            subdirs: n.subdirs,
            items: n.items(),
            mtime: n.mtime,
            child_count: n.children.len() as u32,
            flags: n.flags,
            kind: n.kind as u8,
            category: n.category,
            age_bucket: age_bucket(n.mtime, m.model.scan_started),
            ext_slot: if n.ext_id == u32::MAX {
                12
            } else {
                slots.get(n.ext_id as usize).copied().unwrap_or(12)
            },
        };
        0
    })
}

/// Node display name. Two-call pattern; returns needed bytes incl. NUL,
/// negative on error.
#[no_mangle]
pub extern "C" fn ds_node_name(
    model: *const DsModel,
    id: u64,
    buf: *mut c_char,
    cap: usize,
) -> i32 {
    guard(-1, || {
        let Some(m) = (unsafe { model.as_ref() }) else {
            set_error("NULL model");
            return -1;
        };
        let tree = m.model.tree.read().unwrap();
        let Some(n) = tree.get(NodeId(id)) else {
            set_error(format!("invalid node id {id}"));
            return -1;
        };
        fill_str(&n.name.to_string_lossy(), buf, cap)
    })
}

/// Absolute filesystem path of a node. Two-call pattern.
#[no_mangle]
pub extern "C" fn ds_node_path(
    model: *const DsModel,
    id: u64,
    buf: *mut c_char,
    cap: usize,
) -> i32 {
    guard(-1, || {
        let Some(m) = (unsafe { model.as_ref() }) else {
            set_error("NULL model");
            return -1;
        };
        let tree = m.model.tree.read().unwrap();
        if tree.get(NodeId(id)).is_none() {
            set_error(format!("invalid node id {id}"));
            return -1;
        }
        fill_str(&tree.abs_path(NodeId(id)).to_string_lossy(), buf, cap)
    })
}

/// Sorted children (CORE-TREE-3). `sort`: 0 size, 1 name, 2 items, 3 mtime.
/// Fills up to `cap` ids into `buf`; returns the total child count
/// (call again with a bigger buffer if total > cap), negative on error.
#[no_mangle]
pub extern "C" fn ds_node_children(
    model: *const DsModel,
    id: u64,
    sort: u8,
    descending: u8,
    buf: *mut u64,
    cap: usize,
) -> i64 {
    guard(-1, || {
        let Some(m) = (unsafe { model.as_ref() }) else {
            set_error("NULL model");
            return -1;
        };
        let tree = m.model.tree.read().unwrap();
        if tree.get(NodeId(id)).is_none() {
            set_error(format!("invalid node id {id}"));
            return -1;
        }
        let ids = tree.sorted_children(NodeId(id), SortKey::from_u8(sort), descending != 0);
        if !buf.is_null() {
            for (i, nid) in ids.iter().take(cap).enumerate() {
                unsafe { *buf.add(i) = nid.0 };
            }
        }
        ids.len() as i64
    })
}

#[no_mangle]
pub extern "C" fn ds_node_percent_of_root(model: *const DsModel, id: u64) -> f64 {
    guard(0.0, || {
        unsafe { model.as_ref() }
            .map(|m| m.model.tree.read().unwrap().percent_of_root(NodeId(id)))
            .unwrap_or(0.0)
    })
}

#[no_mangle]
pub extern "C" fn ds_node_percent_of_parent(model: *const DsModel, id: u64) -> f64 {
    guard(0.0, || {
        unsafe { model.as_ref() }
            .map(|m| m.model.tree.read().unwrap().percent_of_parent(NodeId(id)))
            .unwrap_or(0.0)
    })
}

// ---------------------------------------------------------------------------
// Aggregations: extensions, categories
// ---------------------------------------------------------------------------

/// Extension aggregation sorted bytes-desc (CORE-EXT-2). Fills up to `cap`
/// entries; returns total distinct extensions, negative on error.
#[no_mangle]
pub extern "C" fn ds_type_list(model: *const DsModel, buf: *mut DsTypeStat, cap: usize) -> i64 {
    guard(-1, || {
        let Some(m) = (unsafe { model.as_ref() }) else {
            set_error("NULL model");
            return -1;
        };
        let stats = m.model.ext_stats.read().unwrap();
        let slots = ext_slot_map(&m.model);
        let mut order: Vec<usize> = (0..stats.len()).collect();
        order.sort_by(|&a, &b| {
            stats[b]
                .logical
                .cmp(&stats[a].logical)
                .then_with(|| stats[a].ext.cmp(&stats[b].ext))
        });
        if !buf.is_null() {
            for (i, &si) in order.iter().take(cap).enumerate() {
                let s = &stats[si];
                let mut ext = [0 as c_char; 16];
                for (j, b) in s.ext.as_bytes().iter().take(15).enumerate() {
                    ext[j] = *b as c_char;
                }
                unsafe {
                    *buf.add(i) = DsTypeStat {
                        ext,
                        logical: s.logical,
                        files: s.files,
                        slot: slots[si],
                    };
                }
            }
        }
        order.len() as i64
    })
}

/// Per-category totals over the whole tree (design 1b legend chips / 1g).
/// `buf` should hold 8 entries; returns the count written (8), sorted
/// bytes-desc.
#[no_mangle]
pub extern "C" fn ds_category_list(
    model: *const DsModel,
    buf: *mut DsCategoryStat,
    cap: usize,
) -> i64 {
    guard(-1, || {
        let Some(m) = (unsafe { model.as_ref() }) else {
            set_error("NULL model");
            return -1;
        };
        let tree = m.model.tree.read().unwrap();
        let mut agg = [(0u64, 0u64); CATEGORY_COUNT];
        for i in 0..tree.len() {
            let n = tree.get(NodeId::from_index(i)).unwrap();
            if !n.is_dir() && n.flags & crate::tree::flags::HARDLINK_DUP == 0 {
                let c = (n.category as usize).min(CATEGORY_COUNT - 1);
                agg[c].0 += n.logical;
                agg[c].1 += 1;
            }
        }
        let mut order: Vec<usize> = (0..CATEGORY_COUNT).collect();
        order.sort_by(|&a, &b| agg[b].0.cmp(&agg[a].0).then(a.cmp(&b)));
        if !buf.is_null() {
            for (i, &c) in order.iter().take(cap).enumerate() {
                unsafe {
                    *buf.add(i) = DsCategoryStat {
                        category: c as u8,
                        logical: agg[c].0,
                        files: agg[c].1,
                    };
                }
            }
        }
        CATEGORY_COUNT as i64
    })
}

/// Stable English name for a category id (hosts localize their own).
#[no_mangle]
pub extern "C" fn ds_category_name(category: u8, buf: *mut c_char, cap: usize) -> i32 {
    guard(-1, || {
        fill_str(Category::from_u8(category).name(), buf, cap)
    })
}

// ---------------------------------------------------------------------------
// Volume figures (CORE-SYN-2/3)
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn ds_model_set_volume(model: *const DsModel, total: u64, free: u64) -> i32 {
    guard(-1, || {
        let Some(m) = (unsafe { model.as_ref() }) else {
            set_error("NULL model");
            return -1;
        };
        *m.model.volume.lock().unwrap() = Some(VolumeFigures { total, free });
        0
    })
}

#[no_mangle]
pub extern "C" fn ds_model_volume(model: *const DsModel, out: *mut DsVolumeInfo) -> i32 {
    guard(-1, || {
        let (Some(m), Some(out)) = (unsafe { model.as_ref() }, unsafe { out.as_mut() }) else {
            set_error("NULL argument");
            return -1;
        };
        let Some(v) = *m.model.volume.lock().unwrap() else {
            set_error("no volume figures set");
            return -1;
        };
        *out = DsVolumeInfo {
            total: v.total,
            free: v.free,
            unknown: m.model.unknown_bytes().unwrap_or(0),
        };
        0
    })
}

// ---------------------------------------------------------------------------
// Scan report (CORE-SCAN-13)
// ---------------------------------------------------------------------------

/// Error message + path for report entry `index`. Two-call pattern.
#[no_mangle]
pub extern "C" fn ds_model_error(
    model: *const DsModel,
    index: u64,
    buf: *mut c_char,
    cap: usize,
) -> i32 {
    guard(-1, || {
        let Some(m) = (unsafe { model.as_ref() }) else {
            set_error("NULL model");
            return -1;
        };
        let errors = m.model.errors.lock().unwrap();
        let Some(e) = errors.get(index as usize) else {
            set_error("error index out of range");
            return -1;
        };
        fill_str(
            &format!("{}: {}", e.path.to_string_lossy(), e.message),
            buf,
            cap,
        )
    })
}

// ---------------------------------------------------------------------------
// Treemap (CORE-TM-*, CORE-FFI-6)
// ---------------------------------------------------------------------------

/// Lay out the subtree under `root` into a `w × h` rectangle at origin.
/// `algorithm`: 0 KDirStat rows, 1 squarified. On success writes an
/// engine-owned buffer to `(out, out_len)`; free with `ds_treemap_free`.
#[no_mangle]
pub extern "C" fn ds_treemap_layout(
    model: *const DsModel,
    root: u64,
    w: f32,
    h: f32,
    algorithm: u8,
    min_px: f32,
    out: *mut *mut DsTmRect,
    out_len: *mut usize,
) -> i32 {
    guard(-1, || {
        let Some(m) = (unsafe { model.as_ref() }) else {
            set_error("NULL model");
            return -1;
        };
        if out.is_null() || out_len.is_null() {
            set_error("NULL out pointer");
            return -1;
        }
        let tree = m.model.tree.read().unwrap();
        let slots = ext_slot_map(&m.model);
        let started = m.model.scan_started;
        let rects: Vec<LayoutRect> = treemap::layout(
            &tree,
            NodeId(root),
            0.0,
            0.0,
            w,
            h,
            LayoutParams {
                algorithm: Algorithm::from_u8(algorithm),
                min_px: if min_px > 0.0 { min_px } else { 2.0 },
                max_depth: 64,
            },
            &|ext_id| {
                if ext_id == u32::MAX {
                    12
                } else {
                    slots.get(ext_id as usize).copied().unwrap_or(12)
                }
            },
            &|mtime| age_bucket(mtime, started),
        );
        let mut buf: Vec<DsTmRect> = rects
            .iter()
            .map(|r| DsTmRect {
                node: r.node.0,
                x: r.x,
                y: r.y,
                w: r.w,
                h: r.h,
                depth: r.depth,
                is_dir: r.is_dir as u8,
                category: r.category,
                age_bucket: r.age_bucket,
                ext_slot: r.ext_slot,
            })
            .collect();
        buf.shrink_to_fit();
        unsafe {
            *out_len = buf.len();
            *out = buf.as_mut_ptr();
        }
        std::mem::forget(buf);
        0
    })
}

/// Free a layout buffer returned by `ds_treemap_layout`.
#[no_mangle]
pub extern "C" fn ds_treemap_free(rects: *mut DsTmRect, len: usize) {
    guard((), || {
        if !rects.is_null() {
            drop(unsafe { Vec::from_raw_parts(rects, len, len) });
        }
    })
}

/// Hit-test a laid-out buffer (CORE-TM-4). Returns the deepest containing
/// leaf's node id, 0 if none.
#[no_mangle]
pub extern "C" fn ds_treemap_hit_test(rects: *const DsTmRect, len: usize, x: f32, y: f32) -> u64 {
    guard(0, || {
        if rects.is_null() {
            return 0;
        }
        let slice = unsafe { std::slice::from_raw_parts(rects, len) };
        let converted: Vec<LayoutRect> = slice
            .iter()
            .map(|r| LayoutRect {
                node: NodeId(r.node),
                x: r.x,
                y: r.y,
                w: r.w,
                h: r.h,
                depth: r.depth,
                is_dir: r.is_dir != 0,
                category: r.category,
                age_bucket: r.age_bucket,
                ext_slot: r.ext_slot,
            })
            .collect();
        treemap::hit_test(&converted, x, y).0
    })
}

// ---------------------------------------------------------------------------
// Refresh (CORE-SCAN-12 / CORE-FFI-8)
// ---------------------------------------------------------------------------

/// Re-read one node and its subtree from disk (after the host mutated the
/// filesystem, e.g. Move to Trash), updating ancestors. Synchronous.
#[no_mangle]
pub extern "C" fn ds_refresh_node(model: *const DsModel, id: u64) -> i32 {
    guard(-1, || {
        let Some(m) = (unsafe { model.as_ref() }) else {
            set_error("NULL model");
            return -1;
        };
        match crate::scan::refresh_node(&m.model, NodeId(id)) {
            Ok(()) => 0,
            Err(e) => {
                set_error(format!("refresh failed: {e}"));
                -1
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Test hook (EVC-FFI-SAFE-1)
// ---------------------------------------------------------------------------

/// Deliberately panics internally; must return -1 and set the error rather
/// than unwind across the boundary. Exists for the FFI-safety eval.
#[no_mangle]
pub extern "C" fn ds_internal_panic_test() -> i32 {
    guard(-1, || panic!("deliberate test panic"))
}
