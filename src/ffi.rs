//! ============================================================================
//! FILE: src/ffi.rs
//!
//! ============================================================================
//!
//! # Purpose
//! The C ABI (CORE-FFI-*): the only module that touches raw pointers, and
//! the seam the Swift host (MacDirStat) links against. cbindgen reads this
//! file to generate `include/dirstat_core.h`, so every `///` doc comment on
//! an exported item below flows verbatim into the C header — the comments
//! ARE the host-facing API documentation. The design goal is a boundary
//! that cannot crash the host: every entry point is wrapped in a
//! panic-catching guard, every pointer is null-checked, and all buffer
//! ownership rules are explicit and symmetric (what the engine allocates,
//! the engine frees).
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
//!
//! # Upstream dependencies (what this file consumes)
//! - crate::scan — Scan/Model lifecycle, Progress, refresh_node; the Arc
//!   inside Model is what lets DsModel outlive DsScan
//! - crate::tree — NodeId/SortKey/age_bucket and the read-only accessors
//!   behind every ds_node_* call; flag constants mirrored as DS_NODE_FLAG_*
//! - crate::treemap — layout/hit_test plus the Algorithm/LayoutParams/
//!   LayoutRect types converted to/from DsTmRect
//! - crate::classify — Category names and CATEGORY_COUNT for the
//!   category aggregation
//! - std::panic::{catch_unwind, AssertUnwindSafe} — the panic wall
//! - std::ffi::{c_char, c_void, CStr} — raw C string/pointer traffic
//! - std::cell::RefCell (thread_local) — per-thread last-error storage
//! - std::sync::{Arc, atomic} — model sharing and lock-free stat reads
//!
//! # Downstream consumers (who depends on this file)
//! - include/dirstat_core.h — generated from this file by cbindgen
//!   (`cbindgen --crate dirstat-core -o include/dirstat_core.h`)
//! - MacDirStat (Swift) — calls every ds_* function and consumes the
//!   DsNodeInfo/DsTmRect/DsTypeStat/DsCategoryStat/DsScanOptions structs
//! - tests/ffi.rs — end-to-end host-side drive of the same surface
//!
//! # Structure
//! - DS_ABI_VERSION / ds_abi_version — handshake version gate
//! - LAST_ERROR / set_error / ds_last_error — thread-local error detail
//! - guard — the catch_unwind wrapper every entry point runs inside
//! - fill_str — the two-call string-return helper
//! - DsScan / DsModel / CallbackState / DsProgressCallback — handles
//! - DS_NODE_FLAG_* (+ flag_sync test) — mirrored flag bits
//! - DsScanOptions / DsNodeInfo / DsTmRect / DsTypeStat / DsCategoryStat /
//!   DsVolumeInfo / DsScanStats — #[repr(C)] structs (the ABI data model)
//! - Lifecycle: ds_scan_begin/cancel/is_complete/join/model/free
//! - Model/tree access: ds_model_root/stats, ext_slot_map, ds_node_*
//! - Aggregations: ds_type_list, ds_category_list, ds_category_name
//! - Volume figures: ds_model_set_volume / ds_model_volume
//! - Scan report: ds_model_error
//! - Treemap: ds_treemap_layout / ds_treemap_free / ds_treemap_hit_test
//! - Refresh: ds_refresh_node
//! - Test hook: ds_internal_panic_test (EVC-FFI-SAFE-1)
//!
//! # Algorithm & invariants
//! - Panic wall: no Rust panic may unwind into C (that is UB). `guard`
//!   catches the payload, stashes its message in LAST_ERROR, and returns
//!   the function's designated error value. The exported signatures stay
//!   plain (never `unsafe extern`), and `#[no_mangle] extern "C"` in Rust
//!   2021 does not abort-on-unwind by itself, hence the explicit guard.
//! - Error protocol: functions return -1/NULL/0 on failure and leave a
//!   message in thread-local storage; ds_last_error retrieves it. Being
//!   thread-local, an error on one thread never clobbers another's.
//! - String protocol: fill_str always NUL-terminates within `cap` and
//!   returns the full needed size, so callers can size-then-fill in two
//!   calls or accept truncation in one.
//! - Ownership: DsScan owns the Scan and (via `_cb`) the callback state;
//!   DsModel owns an Arc clone, so models survive scan disposal. The rect
//!   buffer from ds_treemap_layout is engine-allocated and MUST return to
//!   the engine via ds_treemap_free — the mem::forget at hand-out is
//!   exactly undone by Vec::from_raw_parts at free (same ptr/len/capacity,
//!   which is why the buffer is shrunk to fit before the pointer is taken).
//!
//! ============================================================================

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
/// v3: `ds_treemap_layout` gained a `metric` parameter (0 logical,
/// 1 physical); `ds_node_children` accepts sort key 4 (physical);
/// `DsCategoryStat` gained `physical`.
/// v4: `DsScanOptions` gained `max_nodes`; `DS_NODE_FLAG_NON_UTF8` added;
/// `ds_node_path_raw` added (raw-bytes path for names the lossy string
/// path can't safely represent).
pub const DS_ABI_VERSION: u32 = 4;

thread_local! {
    // Per-thread last-error text (empty = no error). Thread-local so
    // concurrent callers on different threads can't clobber each other's
    // diagnostics; the cost is that ds_last_error must be called on the
    // same thread that got the failure.
    static LAST_ERROR: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Record an error message for the calling thread (retrievable via
/// `ds_last_error`). Overwrites any previous message.
fn set_error(msg: impl Into<String>) {
    LAST_ERROR.with(|e| *e.borrow_mut() = msg.into());
}

/// The panic wall (CORE-FFI-SAFE-1): run `f`, and if it panics, convert
/// the unwind into `default` plus a LAST_ERROR message. Every exported
/// function's body is wrapped in this, because a Rust panic unwinding into
/// C is undefined behavior.
///
/// `AssertUnwindSafe` is sound here because nothing observed after a catch
/// can see broken invariants: the guarded closures either work on owned
/// locals or take std locks, and a lock poisoned by the panic makes any
/// later `.unwrap()` panic again — which this same guard converts into an
/// error return rather than UB.
fn guard<T>(default: T, f: impl FnOnce() -> T) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(p) => {
            // Panic payloads are `Any`; the two overwhelmingly common
            // types are &str (panic!("literal")) and String
            // (panic!("{x}")). Anything else gets a generic message.
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
///
/// The contract every string-returning ds_* function shares:
/// call 1: `fill_str(s, NULL, 0)` -> returns needed size, writes nothing;
/// call 2: caller allocates `needed` bytes and calls again to fill.
/// A too-small `cap` is not an error: the copy truncates to `cap - 1`
/// bytes + NUL and the return value still reports the full size, so the
/// caller can detect truncation (`ret > cap`) and retry.
fn fill_str(s: &str, buf: *mut c_char, cap: usize) -> i32 {
    let bytes = s.as_bytes();
    let needed = bytes.len() + 1; // + NUL
    if !buf.is_null() && cap > 0 {
        // Reserve 1 byte of cap for the NUL; truncate the payload to fit.
        let n = bytes.len().min(cap - 1);
        // SAFETY: caller guarantees `buf` points to at least `cap` writable
        // bytes (the documented contract); n + 1 <= cap by construction.
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

/// Opaque handle to a running (or finished) scan. Created by
/// `ds_scan_begin`; destroyed by `ds_scan_free` (which cancels and joins
/// if the scan is still running).
pub struct DsScan {
    scan: Scan,
    // Keep the callback alive as long as the scan: the progress closure
    // handed to Scan::begin dereferences a raw pointer into this box (see
    // the trampoline in ds_scan_begin), so it must not move or drop until
    // the workers have joined. Boxed so the address is stable; field order
    // (scan first) makes Drop join the workers BEFORE the state is freed.
    _cb: Option<Box<CallbackState>>,
}

/// Opaque handle to a scanned model. Holds an `Arc<Model>`, so any number
/// of handles may exist and the model lives until the last one is freed —
/// independent of the scan handle's lifetime.
pub struct DsModel {
    model: Arc<Model>,
}

/// Raw OS bytes of a path, without lossy UTF-8 conversion. On Unix this is
/// the exact byte sequence the kernel uses; elsewhere no non-UTF-8 paths
/// occur in practice, so the lossy UTF-8 bytes are an adequate fallback.
#[cfg(unix)]
fn os_path_bytes(path: &std::path::Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}
#[cfg(not(unix))]
fn os_path_bytes(path: &std::path::Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

/// The host's progress callback plus its user_data pointer, boxed at a
/// stable address so worker threads can reach it via raw pointer.
struct CallbackState {
    cb: DsProgressCallback,
    user: *mut c_void,
}
// SAFETY: `user` is an opaque token owned by the host; the engine never
// dereferences it, only passes it back on callback invocations. The host
// accepted cross-thread delivery by registering a callback (documented:
// progress arrives on engine threads), so sharing the pair across threads
// is the host's explicit contract, not an engine assumption.
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
/// The node's name is not valid UTF-8, so the lossy string path
/// (`ds_node_path`) can collide with a different real file. Hosts MUST
/// refuse to trash/delete via the string path when this bit is set; use
/// `ds_node_path_raw` or skip the node.
pub const DS_NODE_FLAG_NON_UTF8: u32 = 32;

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
        assert_eq!(super::DS_NODE_FLAG_NON_UTF8, flags::NON_UTF8);
    }
}

/// Scan options; zero-initialize for defaults.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DsScanOptions {
    /// Nonzero: cross filesystem/mount boundaries (default 0, stay on one).
    pub cross_filesystems: u8,
    /// Nonzero: follow symlinks (default 0, count the link itself).
    pub follow_symlinks: u8,
    /// Nonzero: skip dotfiles (default 0, include them). Note the polarity:
    /// the C-side field is exclude_*, the engine default is include.
    pub exclude_hidden: u8,
    /// Worker threads; 0 = automatic (available parallelism, capped).
    pub max_concurrency: u32,
    /// Optional array of `skip_paths_len` NUL-terminated UTF-8 absolute
    /// directory paths the scan must not descend into (platform knowledge
    /// the host supplies — e.g. `/System/Volumes/Data` when scanning `/` on
    /// macOS so the APFS volume group is not traversed twice). May be NULL
    /// when `skip_paths_len` is 0. Only read during `ds_scan_begin`.
    pub skip_paths: *const *const c_char,
    pub skip_paths_len: usize,
    /// Maximum nodes to create (0 = unlimited). A safety ceiling against
    /// directory-bombs: on reaching it the scan stops enqueueing, records a
    /// scan-report note, and finishes partial-but-consistent. Interactive
    /// hosts should pass a generous value (e.g. 50_000_000).
    pub max_nodes: u64,
}

/// Flat per-node facts (CORE-FFI-4). Strings via `ds_node_name`/`ds_node_path`.
/// For directories, sizes/counts are subtree aggregates (descendants only);
/// for files they are the file's own figures.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DsNodeInfo {
    /// The node's own id (echoed back for convenience).
    pub id: u64,
    /// Parent node id; 0 for the root.
    pub parent: u64,
    /// Logical (apparent) bytes.
    pub logical: u64,
    /// Physical (allocated on-disk) bytes.
    pub physical: u64,
    /// Files in subtree (a file counts itself as 1).
    pub files: u64,
    /// Directories in subtree (excluding self).
    pub subdirs: u64,
    /// files + subdirs (the engine maintains this invariant).
    pub items: u64,
    /// mtime, seconds since Unix epoch; 0 when unknown.
    pub mtime: i64,
    /// Number of direct children (size the ds_node_children buffer).
    pub child_count: u32,
    /// Bitset of DS_NODE_FLAG_* values.
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

/// One treemap rectangle (bulk buffer element). Directory rects precede
/// their children in the buffer, so painting in order yields correct
/// nesting. Leaves carry the color-channel keys (category / age_bucket /
/// ext_slot) the host maps to its palette.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DsTmRect {
    /// Node id this rect represents (for selection / hit-test).
    pub node: u64,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// Nesting level relative to the layout root (root = 0).
    pub depth: u16,
    /// Nonzero: a directory (group) rect rather than a leaf.
    pub is_dir: u8,
    /// Kind bucket; see `ds_category_name`.
    pub category: u8,
    /// 0 this week .. 4 older (vs scan start).
    pub age_bucket: u8,
    /// 0..11 top-12 extension slot, 12 = other/none.
    pub ext_slot: u8,
}

/// Per-extension aggregate (CORE-EXT-*).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DsTypeStat {
    /// Lowercased extension, NUL-terminated, truncated to fit.
    pub ext: [c_char; 16],
    /// Total logical bytes across files with this extension.
    pub logical: u64,
    /// Number of files with this extension.
    pub files: u64,
    /// 0..11 distinct palette slot, 12 = aggregated "other".
    pub slot: u8,
}

/// Per-category aggregate for the legend chips / capacity footer (1b).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DsCategoryStat {
    /// Category id; label via `ds_category_name`.
    pub category: u8,
    /// Apparent bytes.
    pub logical: u64,
    /// Allocated on-disk bytes (what the capacity footer should show).
    pub physical: u64,
    /// Number of files in this category (hard-link duplicates excluded).
    pub files: u64,
}

/// Volume reconciliation figures (CORE-SYN-2/3). `total`/`free` echo what
/// the host supplied via `ds_model_set_volume`; the engine owns only the
/// `unknown` math.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DsVolumeInfo {
    /// Volume capacity in bytes (host-supplied).
    pub total: u64,
    /// Free bytes (host-supplied).
    pub free: u64,
    /// `max(0, total - free - measured)` — the "unreadable" number.
    pub unknown: u64,
}

/// Live scan totals for progress polling. Cheap to read at any time
/// (lock-free counters); safe while the scan is running.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DsScanStats {
    /// Nodes discovered so far (monotonic).
    pub items: u64,
    /// Logical bytes counted so far (monotonic).
    pub bytes: u64,
    /// Nonzero once the scan has finished (complete or cancelled).
    pub complete: u8,
    /// Scan-report entries so far; retrieve via `ds_model_error`.
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
/// engine threads (CORE-FFI-2), throttled to roughly 10 Hz plus a final
/// done call; `user` is passed back verbatim on every invocation. The
/// returned handle must be released with `ds_scan_free`. Returns
/// immediately; the scan proceeds on background threads.
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
        // Deep-copy skip_paths NOW: the C arrays are only guaranteed valid
        // during this call, but the scan threads outlive it. NULL entries
        // and non-UTF-8 strings are skipped rather than failing the scan.
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
            max_nodes: opts.map(|o| o.max_nodes).unwrap_or(0),
        };

        // Progress-callback trampoline: adapt the engine's Rust closure
        // interface (Fn(&Progress)) to the host's C function pointer.
        //
        // Lifetime: CallbackState is boxed (stable heap address) and stored
        // in the returned DsScan as `_cb`, so it lives exactly as long as
        // the Scan. DsScan's field order drops `scan` first, and Scan's
        // Drop cancels + joins the workers — so by the time `_cb` is freed
        // no thread can still invoke the closure. That ordering is what
        // makes the raw-pointer dereference below sound.
        let cb_state = callback.map(|cb| Box::new(CallbackState { cb, user }));
        let progress = cb_state.as_ref().map(|state| {
            let raw: *const CallbackState = &**state;
            // SAFETY: the CallbackState outlives the Scan (owned by DsScan,
            // dropped after the scan joins in ds_scan_free).
            // The pointer is laundered through usize because a raw pointer
            // captured directly would make the closure !Send/!Sync (raw
            // pointers aren't Send), and Scan::begin requires a
            // Send + Sync callback. The round-trip is sound: usize <->
            // pointer casts are value-preserving, and the pointee's
            // validity is guaranteed by the ownership argument above,
            // not by the pointer's type.
            let raw = raw as usize;
            Box::new(move |p: &Progress| {
                let state = unsafe { &*(raw as *const CallbackState) };
                // Build a NUL-terminated copy of the path for C. The
                // pointer handed to the callback refers to this local
                // Vec, hence the documented rule that it is only valid
                // for the duration of the call.
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

/// Request cancellation (CORE-SCAN-11). Non-blocking; partial results
/// remain valid and internally consistent. NULL is a safe no-op.
#[no_mangle]
pub extern "C" fn ds_scan_cancel(scan: *mut DsScan) {
    guard((), || {
        if let Some(s) = unsafe { scan.as_ref() } {
            s.scan.cancel();
        }
    })
}

/// 1 once the scan has finished (complete or cancelled); 0 while running
/// or for a NULL handle.
#[no_mangle]
pub extern "C" fn ds_scan_is_complete(scan: *const DsScan) -> u8 {
    guard(0, || {
        unsafe { scan.as_ref() }
            .map(|s| s.scan.is_complete() as u8)
            .unwrap_or(0)
    })
}

/// Block until the scan finishes (`scan_await`). Combine with
/// `ds_scan_cancel` for a synchronous stop. NULL is a safe no-op.
#[no_mangle]
pub extern "C" fn ds_scan_join(scan: *mut DsScan) {
    guard((), || {
        if let Some(s) = unsafe { scan.as_mut() } {
            s.scan.join();
        }
    })
}

/// Get a model handle. Safe to call while the scan runs (progressive reads);
/// each returned handle must be freed with `ds_model_free`. The model is
/// reference-counted internally, so it remains valid after `ds_scan_free`.
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
/// retained via `ds_scan_model`, stays valid). No progress callback fires
/// after this returns. NULL is a safe no-op; do not double-free.
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

/// Free a model handle (CORE-FFI-3). NodeIds from it are invalid after
/// the LAST handle to the same model is freed. NULL is a safe no-op.
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

/// Fill `out` with live scan totals. 0 on success, -1 on NULL arguments.
/// Lock-free counter reads: cheap enough to poll from a UI timer while
/// the scan runs.
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

/// Build the ext_id -> palette-slot table (CORE-EXT-3): the 12 extensions
/// with the most logical bytes get slots 0..11; everything else maps to
/// slot 12 ("other"). Recomputed per call — the ranking legitimately
/// shifts while a scan is running, and the sort is cheap at realistic
/// extension counts.
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
    let mut map = vec![12u8; stats.len()]; // default: everything is "other"
    for (slot, &(ext_id, _)) in order.iter().take(12).enumerate() {
        map[ext_id as usize] = slot as u8;
    }
    map
}

/// Fill `out` with a node's facts. 0 on success; -1 on NULL arguments or
/// an invalid node id (details via `ds_last_error`).
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

/// Absolute path of a node as RAW OS bytes (no lossy conversion, no NUL).
/// This is the safe path to feed back to the OS for nodes whose name is not
/// valid UTF-8 (`DS_NODE_FLAG_NON_UTF8`), where `ds_node_path`'s lossy
/// string could denote a different file. Two-call pattern by BYTE length:
/// pass `cap == 0` to learn the byte count, then a buffer of that size.
/// Returns the total byte length (NOT NUL-terminated), negative on error.
#[no_mangle]
pub extern "C" fn ds_node_path_raw(
    model: *const DsModel,
    id: u64,
    buf: *mut u8,
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
        let path = tree.abs_path(NodeId(id));
        let bytes = os_path_bytes(&path);
        if !buf.is_null() && cap > 0 {
            let n = bytes.len().min(cap);
            // SAFETY: caller guarantees `buf` points to at least `cap` bytes.
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, n) };
        }
        bytes.len() as i64
    })
}

/// Sorted children (CORE-TREE-3). `sort`: 0 size, 1 name, 2 items,
/// 3 mtime, 4 physical size; nonzero `descending` reverses. Fills up to
/// `cap` ids into `buf` (`buf` may be NULL to just count); returns the
/// total child count (call again with a bigger buffer if total > cap),
/// negative on error.
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

/// Percent of the root's logical bytes (CORE-TREE-5). 0.0 on any error
/// (percentages are display sugar; there is no error channel here).
#[no_mangle]
pub extern "C" fn ds_node_percent_of_root(model: *const DsModel, id: u64) -> f64 {
    guard(0.0, || {
        unsafe { model.as_ref() }
            .map(|m| m.model.tree.read().unwrap().percent_of_root(NodeId(id)))
            .unwrap_or(0.0)
    })
}

/// Percent of the parent's logical bytes (CORE-TREE-5); the root reports
/// 100. 0.0 on any error.
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

/// Extension aggregation sorted bytes-desc (CORE-EXT-2), name-asc ties.
/// Fills up to `cap` entries (`buf` may be NULL to just count); returns
/// the total number of distinct extensions, negative on error.
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
/// `buf` should hold 8 entries; returns the category count (8), sorted
/// physical-bytes-desc. Every category is present, including zero rows.
/// Aggregates non-directory nodes only (dir totals would double-count)
/// and skips hard-link duplicates, so category totals reconcile with the
/// tree's byte totals.
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
        // (logical, physical, files) per category, accumulated over a flat
        // arena walk — leaves only, so nothing is counted twice.
        let mut agg = [(0u64, 0u64, 0u64); CATEGORY_COUNT];
        for i in 0..tree.len() {
            let n = tree.get(NodeId::from_index(i)).unwrap();
            if !n.is_dir() && n.flags & crate::tree::flags::HARDLINK_DUP == 0 {
                let c = (n.category as usize).min(CATEGORY_COUNT - 1);
                agg[c].0 += n.logical;
                agg[c].1 += n.physical;
                agg[c].2 += 1;
            }
        }
        let mut order: Vec<usize> = (0..CATEGORY_COUNT).collect();
        order.sort_by(|&a, &b| agg[b].1.cmp(&agg[a].1).then(a.cmp(&b)));
        if !buf.is_null() {
            for (i, &c) in order.iter().take(cap).enumerate() {
                unsafe {
                    *buf.add(i) = DsCategoryStat {
                        category: c as u8,
                        logical: agg[c].0,
                        physical: agg[c].1,
                        files: agg[c].2,
                    };
                }
            }
        }
        CATEGORY_COUNT as i64
    })
}

/// Stable English name for a category id (hosts localize their own).
/// Two-call pattern; unknown ids report as "Other".
#[no_mangle]
pub extern "C" fn ds_category_name(category: u8, buf: *mut c_char, cap: usize) -> i32 {
    guard(-1, || {
        fill_str(Category::from_u8(category).name(), buf, cap)
    })
}

// ---------------------------------------------------------------------------
// Volume figures (CORE-SYN-2/3)
// ---------------------------------------------------------------------------

/// Supply volume capacity/free figures (CORE-SYN-2); the engine cannot
/// portably learn them itself but owns the `<Unknown>` reconciliation
/// math. 0 on success, -1 on NULL model.
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

/// Read back volume figures plus the computed `unknown` bytes (CORE-SYN-3).
/// -1 (with error detail) if `ds_model_set_volume` was never called.
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

/// Error message + path for report entry `index` (0-based; the count comes
/// from `DsScanStats.error_count`), formatted as "path: message".
/// Two-call pattern; -1 for NULL model or out-of-range index.
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
/// `algorithm`: 0 KDirStat rows, 1 squarified. `min_px`: smallest rect
/// side that is still subdivided (values <= 0 default to 2.0). `metric`:
/// 0 = area ∝ logical (apparent) bytes, 1 = area ∝ physical (allocated)
/// bytes — the truthful channel when sparse files, APFS clones, or
/// cloud-placeholder (dataless) files are present. On success (returns 0)
/// writes an engine-owned buffer to `(out, out_len)`; free with
/// `ds_treemap_free` and never with the host allocator. One bulk buffer
/// per frame — never per-rect calls (CORE-FFI-6). Deterministic: equal
/// inputs produce equal geometry (CORE-TM-6).
#[no_mangle]
pub extern "C" fn ds_treemap_layout(
    model: *const DsModel,
    root: u64,
    w: f32,
    h: f32,
    algorithm: u8,
    min_px: f32,
    metric: u8,
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
                use_physical: metric == 1,
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
        // Convert the internal rects into the #[repr(C)] layout, then hand
        // the Vec's storage to the caller. Ownership dance:
        //  1. shrink_to_fit forces capacity == len — required because
        //     ds_treemap_free reconstructs the Vec with capacity = len,
        //     and Vec::from_raw_parts with the wrong capacity is UB.
        //     (collect() from an exact-size iterator already gives
        //     capacity == len in practice; the shrink makes it a
        //     guarantee rather than an implementation detail.)
        //  2. mem::forget relinquishes ownership WITHOUT freeing, so the
        //     pointer stays valid after this function returns.
        //  3. ds_treemap_free re-adopts the exact (ptr, len, len) triple
        //     and drops it, releasing the memory in the same allocator.
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

/// Free a layout buffer returned by `ds_treemap_layout`. Pass back the
/// exact pointer and length that call produced (the pair reconstructs the
/// original allocation). NULL is a safe no-op; do not free twice.
#[no_mangle]
pub extern "C" fn ds_treemap_free(rects: *mut DsTmRect, len: usize) {
    guard((), || {
        if !rects.is_null() {
            // SAFETY: (ptr, len, len) is exactly what ds_treemap_layout
            // forgot — same allocation, same length, capacity forced equal
            // to len by the shrink_to_fit there. Dropping the rebuilt Vec
            // frees the buffer in the allocator that created it.
            drop(unsafe { Vec::from_raw_parts(rects, len, len) });
        }
    })
}

/// Hit-test a laid-out buffer (CORE-TM-4). Returns the deepest containing
/// leaf's node id (a leaf beats any directory rect containing the same
/// point), 0 if the point is outside every rect. Pure function of the
/// buffer — the model is not consulted.
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
