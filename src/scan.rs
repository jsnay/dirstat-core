//! ============================================================================
//! FILE: src/scan.rs
//!
//! ============================================================================
//!
//! # Purpose
//! The parallel filesystem scanner (CORE-SCAN-*) and the shared [`Model`] it
//! produces. A bounded worker pool pulls directories off a shared queue,
//! stats their entries with no lock held, then inserts the whole listing
//! into the tree under one short write-lock critical section. Aggregates
//! propagate to ancestors once per directory listing, so a reader taking
//! the read lock at any moment sees an internally consistent tree whose
//! numbers only ever grow — the invariant behind the progressive UI
//! (design 1d: "counters appear instantly and only go up"). This file also
//! hosts [`refresh_node`], the post-mutation subtree re-read (CORE-SCAN-12).
//!
//! # Upstream dependencies (what this file consumes)
//! - crate::tree — Node/NodeId/Tree arena and flag bits; this file is the
//!   tree's only mutator (push/propagate/get_mut, always under write lock)
//! - crate::classify — Category, dir-name path rules, file classification;
//!   applied at insert time so category is a stored fact, not recomputed
//! - std::sync::{Mutex, RwLock, Condvar} — Model interior locks, the work
//!   queue, and worker sleep/wake for the termination protocol
//! - std::sync::atomic::{AtomicBool, AtomicU64} — cancel flag, live
//!   items/bytes counters, completion latch, progress throttle timestamp
//! - std::collections::{HashMap, HashSet} — extension interning table,
//!   hard-link (dev,ino) dedup map, directory-alias (dev,ino) dedup set
//! - std::fs / std::os::unix::fs::MetadataExt — read_dir + symlink_metadata
//!   for listing; dev/ino/nlink/blocks behind small `#[cfg(unix)]` shims so
//!   the crate builds on non-Unix CI
//! - std::thread — one OS thread per worker; std::time for the progress
//!   throttle and the age-bucket reference instant
//!
//! # Downstream consumers (who depends on this file)
//! - src/ffi.rs — wraps Scan/Model into DsScan/DsModel handles; forwards
//!   Progress to the C callback; calls refresh_node for ds_refresh_node;
//!   reads items/bytes/complete/errors/volume for ds_model_stats and co.
//! - src/lib.rs — re-exports Model, Progress, Scan, ScanOptions
//! - tests/engine.rs — drives Scan::begin/cancel/join and refresh_node
//!   directly and asserts every EVC-SCAN-* eval against Model internals
//!
//! # Structure
//! - ScanOptions / ScanError / ExtStat / VolumeFigures / Model / Progress —
//!   data types (options in, report + aggregations + live counters out)
//! - WorkQueue / QueueState — the Condvar-guarded directory queue
//! - Scan — public handle: begin (spawns workers), cancel, is_complete, join
//! - ScanShared — everything workers share (model, queue, dedup tables,
//!   options, cancel flag, progress throttle)
//! - worker_loop / finish_if_first — the pool loop and completion latch
//! - process_dir — list one directory: stat, dedup, insert, propagate
//! - refresh_node / graft — re-read one subtree in place (CORE-SCAN-12)
//! - extension_of / mtime_of / device_of / inode_of / nlink_of /
//!   physical_of — small metadata helpers (per-platform at the bottom)
//!
//! # Algorithm & invariants
//! - Work pool: the queue holds (NodeId, path, inherited-category) jobs.
//!   `in_flight` counts claimed-but-unfinished jobs. Termination: a worker
//!   exits when the queue is empty AND `in_flight == 0` — an empty queue
//!   alone is not enough because a running job may still enqueue subdirs.
//! - Cancel (CORE-SCAN-11): setting the flag makes workers drain the queue
//!   and bail out of in-progress listings before any insertion, so the
//!   partial tree keeps all invariants ("Stop keeps everything found so
//!   far").
//! - Only-grow invariant (design 1d): all stats happen before the write
//!   lock is taken; insertion + one `propagate` walk happen inside it.
//!   Readers therefore see totals that are always consistent and only
//!   increase (the sole subtraction path, refresh_node, also runs entirely
//!   under the write lock).
//! - Determinism (CORE-SCAN-9): per-directory listings are sorted by name
//!   before insertion, and dedup keys are (device, inode) facts, so
//!   aggregate results are identical across runs regardless of thread
//!   interleaving. (First-seen path for a hard link IS interleaving
//!   dependent; totals are not.)
//! - Dedup: hard links (CORE-SCAN-5) count bytes once via the `inodes` map;
//!   aliased directories (APFS firmlinks, bind mounts) are listed once via
//!   the `dir_inodes` set, with later paths becoming zero-size DUPLICATE
//!   leaves that are never descended.
//! - Lock ordering: the queue lock and the tree lock are never held at the
//!   same time; ext_index is acquired before ext_stats inside intern_ext
//!   only. No lock is held while touching the filesystem.
//!
//! ============================================================================

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::SystemTime;

use crate::classify::{self, Category};
use crate::tree::{flags, Node, NodeId, NodeKind, Tree};

/// Scan options (CORE-OPT-*), all defaults documented. Fixed for the life
/// of a scan; the FFI layer builds one from `DsScanOptions` in
/// `ds_scan_begin`.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Cross filesystem/mount boundaries (default false, CORE-SCAN-7).
    pub cross_filesystems: bool,
    /// Follow symlinks (default false, CORE-SCAN-6).
    pub follow_symlinks: bool,
    /// Include dotfiles (default true, CORE-OPT-5).
    pub include_hidden: bool,
    /// Worker threads; 0 = available parallelism (CORE-OPT-7).
    pub max_concurrency: usize,
    /// Absolute directory paths the scan must not descend into (default
    /// empty). "Which paths" is platform knowledge the host supplies — e.g.
    /// a macOS host passes `/System/Volumes/Data` when scanning `/` so the
    /// APFS volume group isn't traversed twice through firmlinks.
    pub skip_paths: Vec<PathBuf>,
    /// Maximum number of nodes to create (0 = unlimited, the default).
    /// A safety ceiling against directory-bombs / degenerate trees that
    /// would otherwise exhaust memory: on reaching it the scan stops
    /// enqueueing new directories, records a scan-report note, and finishes
    /// with partial-but-consistent results. In-flight listings may push the
    /// final count slightly past the ceiling (bounded by one directory's
    /// width) before they wind down.
    pub max_nodes: u64,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            cross_filesystems: false,
            follow_symlinks: false,
            include_hidden: true,
            max_concurrency: 0,
            skip_paths: Vec::new(),
            max_nodes: 0,
        }
    }
}

/// One scan-report entry (CORE-SCAN-13): a path the scan could not read
/// (permission denied, vanished mid-scan) plus the OS error text. Errors
/// never abort the scan; they accumulate in `Model.errors` for the host's
/// scan report.
#[derive(Debug, Clone)]
pub struct ScanError {
    pub path: PathBuf,
    pub message: String,
}

/// Extension aggregation entry, maintained incrementally during the scan
/// (CORE-EXT-1). Index in `Model.ext_stats` == a node's `ext_id`, i.e. the
/// interning table (`ScanShared.ext_index`) assigns ids in first-seen order
/// and they double as array indices.
#[derive(Debug, Clone, Default)]
pub struct ExtStat {
    /// Lowercased extension without the dot (case-insensitive merge).
    pub ext: String,
    /// Total logical bytes across all files with this extension.
    pub logical: u64,
    /// File count (hard-link duplicates still count as files here but
    /// contribute 0 bytes, matching the tree's accounting).
    pub files: u64,
}

/// Volume figures supplied by the host (CORE-SYN-2/3): the engine cannot
/// portably know capacity/free for a path, but it owns the reconciliation
/// math (`unknown = capacity - free - measured`, clamped at 0).
#[derive(Debug, Clone, Copy, Default)]
pub struct VolumeFigures {
    pub total: u64,
    pub free: u64,
}

/// The scanned model: tree + aggregations + report. Lives behind an
/// `Arc<Model>`; interior locks make progressive reads safe while workers
/// are still inserting (CORE-FFI-SAFE-4: readers never need the scan to be
/// quiescent).
pub struct Model {
    /// The node arena. Writers: scan workers and `refresh_node`, each
    /// taking short write-guard critical sections. Readers: everything else.
    pub tree: RwLock<Tree>,
    /// Per-extension aggregates; indexed by `Node.ext_id` (CORE-EXT-1).
    pub ext_stats: RwLock<Vec<ExtStat>>,
    /// The scan report (CORE-SCAN-13); append-only during a scan.
    pub errors: Mutex<Vec<ScanError>>,
    /// Host-supplied capacity/free figures; `None` until the host calls
    /// `ds_model_set_volume`.
    pub volume: Mutex<Option<VolumeFigures>>,
    /// Reference instant for age buckets; set once at scan start so age
    /// coloring is deterministic for a loaded model.
    pub scan_started: SystemTime,
    /// Live count of nodes discovered (for cheap lock-free progress polls).
    pub items: AtomicU64,
    /// Live logical-byte total (ditto).
    pub bytes: AtomicU64,
    /// Completion latch. Set exactly once (by the first worker to observe
    /// termination) with Release ordering; readers use Acquire so a `true`
    /// guarantees they see the fully-built tree.
    pub complete: AtomicBool,
}

impl Model {
    fn new() -> Model {
        Model {
            tree: RwLock::new(Tree::new()),
            ext_stats: RwLock::new(Vec::new()),
            errors: Mutex::new(Vec::new()),
            volume: Mutex::new(None),
            scan_started: SystemTime::now(),
            items: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            complete: AtomicBool::new(false),
        }
    }

    /// `<Unknown>` math (CORE-SYN-3): capacity − free − measured, ≥ 0.
    /// "Measured" is the root's *physical* bytes (allocated blocks), the
    /// only figure comparable to volume capacity. Returns `None` until the
    /// host supplies volume figures. Saturating subtraction provides the
    /// clamp-at-zero behavior.
    pub fn unknown_bytes(&self) -> Option<u64> {
        let v = (*self.volume.lock().unwrap())?;
        let measured = self
            .tree
            .read()
            .unwrap()
            .get(NodeId::ROOT)
            .map(|r| r.physical)
            .unwrap_or(0);
        Some(v.total.saturating_sub(v.free).saturating_sub(measured))
    }
}

/// Progress snapshot delivered to the host (CORE-SCAN-10). Counts are
/// monotonic; `current_path` is best-effort.
#[derive(Debug, Clone)]
pub struct Progress {
    pub items: u64,
    pub bytes: u64,
    pub current_path: PathBuf,
    pub done: bool,
}

/// Host progress sink. `Send + Sync` because workers on any thread may
/// invoke it; the FFI layer wraps the C function pointer in one of these.
type ProgressCallback = Box<dyn Fn(&Progress) + Send + Sync>;

/// The shared directory queue: a LIFO stack of pending listings plus the
/// Condvar workers sleep on when it runs dry. LIFO (Vec::pop) keeps the
/// traversal roughly depth-first, bounding queue growth on wide trees.
struct WorkQueue {
    queue: Mutex<QueueState>,
    cond: Condvar,
}

/// State under the queue mutex. `in_flight` is the piece that makes
/// termination detectable: "queue empty" alone is not "done", because a
/// claimed job may still push subdirectories.
struct QueueState {
    /// Pending jobs: (node to fill, its absolute path, category inherited
    /// from path rules on ancestor directory names).
    items: Vec<(NodeId, PathBuf, Option<Category>)>,
    /// Directories claimed but not yet finished.
    in_flight: usize,
}

/// A running (or finished) scan. Owns worker threads; the model is shared
/// (`Arc`) so it outlives the scan handle — the FFI `ds_scan_model` clones
/// the Arc and the host may drop the scan while keeping the model.
pub struct Scan {
    pub model: Arc<Model>,
    /// Cooperative cancel flag polled by workers (CORE-SCAN-11).
    cancel: Arc<AtomicBool>,
    /// Join handles; drained by [`Scan::join`] (and by Drop).
    workers: Vec<std::thread::JoinHandle<()>>,
}

impl Scan {
    /// Begin scanning `root` (CORE-SCAN-1/9). Returns immediately with the
    /// pool already running; workers run until complete or cancelled.
    ///
    /// Errors only if `root` itself cannot be stat'ed (missing path, no
    /// permission on the root); errors below the root become scan-report
    /// entries instead. The optional `progress` callback fires on worker
    /// threads, throttled to ~10 Hz plus one final `done` call.
    pub fn begin(
        root: &Path,
        options: ScanOptions,
        progress: Option<ProgressCallback>,
    ) -> std::io::Result<Scan> {
        // symlink_metadata (not metadata): if the root itself is a symlink
        // we record the link, we do not follow it (CORE-SCAN-6).
        let meta = std::fs::symlink_metadata(root)?;
        let model = Arc::new(Model::new());
        let cancel = Arc::new(AtomicBool::new(false));

        // The root's device id anchors the CORE-SCAN-7 mount-boundary
        // check: child dirs on a different device are skipped by default.
        let root_dev = device_of(&meta);
        let root_name = root
            .file_name()
            .map(OsString::from)
            // e.g. scanning "/" has no file_name; fall back to the whole path.
            .unwrap_or_else(|| OsString::from(root.as_os_str()));

        // Seed the tree with the root node (always NodeId::ROOT == 1).
        {
            let mut tree = model.tree.write().unwrap();
            tree.root_path = root.as_os_str().to_os_string();
            tree.push(Node {
                name: root_name,
                parent: NodeId::INVALID,
                children: Vec::new(),
                kind: NodeKind::Dir,
                logical: 0,
                physical: 0,
                files: 0,
                subdirs: 0,
                mtime: mtime_of(&meta),
                flags: flags::SCANNING,
                category: Category::Other as u8,
                ext_id: u32::MAX,
            });
        }
        model.items.store(1, Ordering::Relaxed); // the root itself

        // CORE-OPT-7: 0 = auto. Capped at 16 — directory listing is
        // syscall-bound and more threads mostly add lock contention.
        let n_workers = if options.max_concurrency == 0 {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .min(16)
        } else {
            options.max_concurrency
        };

        let shared = Arc::new(ScanShared {
            model: Arc::clone(&model),
            queue: WorkQueue {
                // The queue starts with exactly one job: list the root.
                queue: Mutex::new(QueueState {
                    items: vec![(NodeId::ROOT, root.to_path_buf(), None)],
                    in_flight: 0,
                }),
                cond: Condvar::new(),
            },
            ext_index: Mutex::new(HashMap::new()),
            inodes: Mutex::new(HashMap::new()),
            // Pre-seed with the root's own (dev, ino) so an alias of the
            // root deeper in the tree is caught as a DUPLICATE too.
            dir_inodes: Mutex::new({
                let mut seen = std::collections::HashSet::new();
                seen.insert((root_dev, inode_of(&meta)));
                seen
            }),
            options,
            root_dev,
            cancel: Arc::clone(&cancel),
            progress,
            last_progress_ms: AtomicU64::new(0),
            started: std::time::Instant::now(),
            node_limit_hit: AtomicBool::new(false),
        });

        let workers = (0..n_workers)
            .map(|_| {
                let sh = Arc::clone(&shared);
                std::thread::spawn(move || worker_loop(&sh))
            })
            .collect();

        Ok(Scan {
            model,
            cancel,
            workers,
        })
    }

    /// Request cancellation (CORE-SCAN-11); workers stop promptly and the
    /// partial model stays consistent ("Stop keeps everything found so
    /// far"). Non-blocking and idempotent — pair with [`Scan::join`] to
    /// wait for the workers to actually exit.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// True once every worker has finished (whether by exhausting the
    /// queue or by cancellation). Acquire pairs with the Release store in
    /// `finish_if_first`, so `true` implies the final tree is visible.
    pub fn is_complete(&self) -> bool {
        self.model.complete.load(Ordering::Acquire)
    }

    /// Block until the scan finishes (CORE-FFI-2 `scan_await`). Safe to
    /// call more than once; subsequent calls are no-ops (handles drained).
    pub fn join(&mut self) {
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

impl Drop for Scan {
    /// Dropping a scan cancels and joins it, so worker threads never
    /// outlive the handle. The model survives independently through any
    /// `Arc<Model>` clones the host still holds.
    fn drop(&mut self) {
        self.cancel();
        self.join();
    }
}

/// Everything the worker threads share, bundled in one `Arc`.
struct ScanShared {
    model: Arc<Model>,
    queue: WorkQueue,
    /// Extension interning table: lowercased extension -> ext_id (which is
    /// also the index into `Model.ext_stats`).
    ext_index: Mutex<HashMap<String, u32>>,
    /// Hard-link dedup (CORE-SCAN-5): (device, inode) of every multi-link
    /// file seen so far. First path wins the bytes; later paths become
    /// zero-byte `HARDLINK_DUP` entries.
    inodes: Mutex<HashMap<(u64, u64), ()>>,
    /// Directories already scanned, by (device, inode): the second path to
    /// an aliased directory (APFS firmlink, bind mount) becomes a
    /// zero-contribution `DUPLICATE` node instead of a double count.
    dir_inodes: Mutex<std::collections::HashSet<(u64, u64)>>,
    options: ScanOptions,
    /// Device id of the scan root, for the CORE-SCAN-7 mount check.
    root_dev: u64,
    cancel: Arc<AtomicBool>,
    progress: Option<ProgressCallback>,
    /// Milliseconds-since-start of the last progress emission (the ~10 Hz
    /// throttle state, CAS-updated so only one worker wins each tick).
    last_progress_ms: AtomicU64,
    started: std::time::Instant,
    /// Set true (once, via CAS) when the arena reached `options.max_nodes`.
    /// Latches the ceiling so the report note is pushed exactly once and no
    /// worker enqueues further directories.
    node_limit_hit: AtomicBool,
}

impl ScanShared {
    /// Throttled progress emission (~every 100 ms), plus a final `done`
    /// call that bypasses the throttle (CORE-SCAN-10). Called from worker
    /// threads; the callback must expect that.
    fn maybe_report(&self, current: &Path, done: bool) {
        let Some(cb) = &self.progress else { return };
        let now_ms = self.started.elapsed().as_millis() as u64;
        if !done {
            // Two-step throttle: cheap load first, then CAS so that when
            // several workers cross the 100 ms line together exactly one
            // wins and reports; the losers skip instead of stacking calls.
            let last = self.last_progress_ms.load(Ordering::Relaxed);
            if now_ms.saturating_sub(last) < 100 {
                return;
            }
            if self
                .last_progress_ms
                .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
            {
                return;
            }
        }
        cb(&Progress {
            items: self.model.items.load(Ordering::Relaxed),
            bytes: self.model.bytes.load(Ordering::Relaxed),
            current_path: current.to_path_buf(),
            done,
        });
    }

    /// Intern a (lowercased) extension, returning its stable `ext_id`.
    /// Holds `ext_index` across the `ext_stats` push so id assignment and
    /// slot creation are atomic (ids are indices; a torn insert would
    /// leave a dangling id). This is the only place both locks are held.
    fn intern_ext(&self, ext: &str) -> u32 {
        let mut idx = self.ext_index.lock().unwrap();
        if let Some(&id) = idx.get(ext) {
            return id;
        }
        let mut stats = self.model.ext_stats.write().unwrap();
        let id = stats.len() as u32;
        stats.push(ExtStat {
            ext: ext.to_string(),
            logical: 0,
            files: 0,
        });
        idx.insert(ext.to_string(), id);
        id
    }
}

/// The per-worker loop. Every worker runs this until the pool terminates.
///
/// Termination protocol: a worker may exit only when the queue is empty AND
/// `in_flight == 0`. An empty queue alone is NOT termination — some other
/// worker may still be inside `process_dir` and about to enqueue that
/// directory's subdirectories. `in_flight` (incremented at claim time,
/// decremented after processing, both under the queue lock) is exactly the
/// count of such workers, so `empty && in_flight == 0` proves no job exists
/// anywhere and none can be created: the scan is over.
fn worker_loop(sh: &ScanShared) {
    loop {
        // --- Claim phase: get one job or learn that the scan is over. ---
        let job = {
            let mut q = sh.queue.queue.lock().unwrap();
            loop {
                // Cancel drains the queue (under the lock) so no new job
                // can be claimed; once in-flight jobs bail out (they poll
                // the flag too), the normal termination condition fires.
                // This is how "stop promptly" and "partial-but-consistent"
                // coexist (CORE-SCAN-11).
                if sh.cancel.load(Ordering::Relaxed) {
                    q.items.clear();
                }
                if let Some(job) = q.items.pop() {
                    q.in_flight += 1; // claimed: visible to the exit check
                    break Some(job);
                }
                if q.in_flight == 0 {
                    break None; // no work anywhere: scan is over
                }
                // Queue empty but peers are mid-directory: sleep until a
                // peer enqueues subdirs or announces termination. Condvar
                // wait releases the lock while sleeping and re-takes it on
                // wake; the re-check loop handles spurious wakeups.
                q = sh.queue.cond.wait(q).unwrap();
            }
        };
        let Some((dir_id, dir_path, inherited)) = job else {
            // This worker observed termination. Peers may still be parked
            // in the wait above (they saw empty + in_flight > 0 before the
            // last job finished); notify_all so every one of them wakes,
            // re-checks, and exits too — without this the pool would hang.
            sh.queue.cond.notify_all();
            finish_if_first(sh);
            return;
        };

        // --- Work phase: no queue lock held while touching the disk. ---
        process_dir(sh, dir_id, &dir_path, inherited);

        // --- Release phase: retire the claim and wake the right peers. ---
        let mut q = sh.queue.queue.lock().unwrap();
        q.in_flight -= 1;
        if q.in_flight == 0 && q.items.is_empty() {
            // That was the last job in the whole scan: wake EVERY sleeper
            // so all of them observe termination and exit.
            drop(q);
            sh.queue.cond.notify_all();
        } else {
            // Work may remain (process_dir may have enqueued subdirs):
            // one wake is enough — an awakened worker claims a job and its
            // own release will keep the wake chain going.
            drop(q);
            sh.queue.cond.notify_one();
        }
    }
}

/// Completion latch: every exiting worker calls this, but the CAS ensures
/// exactly one performs the finalization (root flag clear + final progress
/// report). The Release half of AcqRel pairs with the Acquire load in
/// `Scan::is_complete`/`ds_model_stats`, publishing the finished tree.
fn finish_if_first(sh: &ScanShared) {
    if sh
        .model
        .complete
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        // Clear the SCANNING flag on the root; subtree flags were cleared
        // as each directory finished.
        if let Some(root) = sh.model.tree.write().unwrap().get_mut(NodeId::ROOT) {
            root.flags &= !flags::SCANNING;
        }
        sh.maybe_report(Path::new(""), true);
    }
}

/// List one directory: create child nodes, aggregate, enqueue subdirs.
///
/// Two-phase design: phase 1 stats every entry with NO tree lock held
/// (syscalls are slow and unbounded — a lock held across them would stall
/// every reader and worker); phase 2 takes the write lock once and inserts
/// the whole listing plus a single `propagate` ancestor walk. Readers
/// therefore see this directory's contribution appear atomically: totals
/// are always internally consistent and only ever grow (design 1d).
///
/// `inherited` is the category imposed by path rules on some ancestor
/// directory name (e.g. everything under `node_modules` is Developer);
/// `None` means classify files by their own extension.
fn process_dir(sh: &ScanShared, dir_id: NodeId, dir_path: &Path, inherited: Option<Category>) {
    if sh.cancel.load(Ordering::Relaxed) {
        return; // claimed but cancelled: contribute nothing, stay consistent
    }
    // Node ceiling latched: skip this already-queued directory entirely so
    // the scan winds down instead of draining the whole backlog. This bounds
    // the overshoot past `max_nodes` to the directories already in flight
    // when the limit hit (at most one per worker), not the queue depth.
    if sh.node_limit_hit.load(Ordering::Acquire) {
        return;
    }
    let entries = match std::fs::read_dir(dir_path) {
        Ok(rd) => rd,
        Err(e) => {
            // Unreadable directory (permission denied / vanished): record
            // a report entry (CORE-SCAN-13), mark the node, and move on —
            // errors never abort the scan.
            sh.model.errors.lock().unwrap().push(ScanError {
                path: dir_path.to_path_buf(),
                message: e.to_string(),
            });
            let mut tree = sh.model.tree.write().unwrap();
            if let Some(n) = tree.get_mut(dir_id) {
                n.flags |= flags::UNREADABLE;
                n.flags &= !flags::SCANNING;
            }
            return;
        }
    };

    // ---- Phase 1: stat everything first without holding the tree lock. ----
    // `Entry` is the lock-free staging record: everything a Node needs,
    // computed up front so the write-lock section below is pure insertion.
    struct Entry {
        name: OsString,
        kind: NodeKind,
        logical: u64,
        physical: u64,
        mtime: i64,
        flags: u32,
        category: Category,
        ext_id: u32,
        subdir_inherit: Option<Category>,
        is_subdir: bool,
        /// False for alias duplicates: counted as a subdir entry but never
        /// enqueued for listing.
        descend: bool,
    }
    let mut found: Vec<Entry> = Vec::new();

    for entry in entries {
        if sh.cancel.load(Ordering::Relaxed) {
            return;
        }
        let Ok(entry) = entry else {
            continue; // vanished mid-listing (CORE-SCAN-13): skip, don't abort
        };
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let hidden = name_str.starts_with('.');
        if hidden && !sh.options.include_hidden {
            continue; // CORE-OPT-5: dotfiles excluded on request
        }
        let path = entry.path();
        // symlink_metadata: never follow links here (CORE-SCAN-6); a
        // symlink is recorded as a leaf at its own (tiny) size.
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                // Stat failed (vanished / EACCES): report and skip the
                // entry, keep the rest of the listing (CORE-SCAN-13).
                sh.model.errors.lock().unwrap().push(ScanError {
                    path,
                    message: e.to_string(),
                });
                continue;
            }
        };

        let mut fl = if hidden { flags::HIDDEN } else { 0 };
        // Non-UTF-8 names are flagged so hosts refuse destructive actions on
        // them (their lossy string path can collide with a different file).
        // `to_str` is None exactly when the raw OS bytes are not valid UTF-8.
        if name.to_str().is_none() {
            fl |= flags::NON_UTF8;
        }

        if meta.is_dir() {
            if !sh.options.cross_filesystems && device_of(&meta) != sh.root_dev {
                continue; // CORE-SCAN-7: don't cross mounts
            }
            if !sh.options.skip_paths.is_empty() && sh.options.skip_paths.iter().any(|s| s == &path)
            {
                continue; // host-supplied skip list (platform knowledge)
            }
            // Alias dedup: a directory inode already scanned via another
            // path (APFS firmlink, bind mount) must not be counted twice.
            // HashSet::insert returns false if the key was already present;
            // insert-and-test in one call makes the check-and-claim atomic
            // under the mutex, so two workers racing on the same alias
            // cannot both descend.
            let dir_key = (device_of(&meta), inode_of(&meta));
            let is_alias = !sh.dir_inodes.lock().unwrap().insert(dir_key);
            // Path rules: this directory's own name may impose a category
            // on everything below (deeper rules override via `.or`).
            let dir_cat = classify::category_for_dir_name(&name_str);
            let child_inherit = dir_cat.or(inherited);
            found.push(Entry {
                name,
                kind: NodeKind::Dir,
                logical: 0,
                physical: 0,
                mtime: mtime_of(&meta),
                // An alias is final (DUPLICATE, never listed); a real dir
                // starts SCANNING until its own listing completes.
                flags: if is_alias {
                    fl | flags::DUPLICATE
                } else {
                    fl | flags::SCANNING
                },
                category: child_inherit.unwrap_or(Category::Other),
                ext_id: u32::MAX,
                subdir_inherit: child_inherit,
                is_subdir: true,
                descend: !is_alias,
            });
        } else {
            let is_symlink = meta.file_type().is_symlink();
            let kind = if is_symlink {
                NodeKind::Symlink // not followed (CORE-SCAN-6)
            } else {
                NodeKind::File
            };
            let mut logical = meta.len();
            let mut physical = physical_of(&meta);
            // Hard links: count shared inodes once (CORE-SCAN-5). Only
            // files with nlink > 1 pay for the map lookup. Like the dir
            // set above, HashMap::insert is the atomic check-and-claim:
            // Some(..) back means another path already owns the bytes, so
            // this one becomes a zero-size HARDLINK_DUP entry (it still
            // counts as an item, just not as bytes).
            if !is_symlink && nlink_of(&meta) > 1 {
                let key = (device_of(&meta), inode_of(&meta));
                let mut seen = sh.inodes.lock().unwrap();
                if seen.insert(key, ()).is_some() {
                    fl |= flags::HARDLINK_DUP;
                    logical = 0;
                    physical = 0;
                }
            }
            let ext = extension_of(&name_str);
            let category = classify::classify_file(ext.as_deref(), inherited);
            let ext_id = match &ext {
                Some(e) => sh.intern_ext(e),
                None => u32::MAX,
            };
            found.push(Entry {
                name,
                kind,
                logical,
                physical,
                mtime: mtime_of(&meta),
                flags: fl,
                category,
                ext_id,
                subdir_inherit: None,
                is_subdir: false,
                descend: false,
            });
        }
    }

    // Deterministic aggregate independent of readdir order (CORE-SCAN-9):
    // name-sorting the listing fixes both child order in the tree and the
    // arena insertion order for this directory.
    found.sort_by(|a, b| a.name.cmp(&b.name));

    // ---- Phase 2: insert under the write lock, batch the aggregates. ----
    let mut d_logical = 0u64;
    let mut d_physical = 0u64;
    let mut d_files = 0u64;
    let mut d_subdirs = 0u64;
    let mut subdir_jobs: Vec<(NodeId, PathBuf, Option<Category>)> = Vec::new();
    let node_count;

    {
        let mut tree = sh.model.tree.write().unwrap();
        for e in &found {
            let id = tree.push(Node {
                name: e.name.clone(),
                parent: dir_id,
                children: Vec::new(),
                kind: e.kind,
                logical: e.logical,
                physical: e.physical,
                files: if e.is_subdir { 0 } else { 1 },
                subdirs: 0,
                mtime: e.mtime,
                flags: e.flags,
                category: e.category as u8,
                ext_id: e.ext_id,
            });
            if e.is_subdir {
                d_subdirs += 1;
                // Aliases (descend == false) are entries but never jobs.
                if e.descend {
                    subdir_jobs.push((id, dir_path.join(&e.name), e.subdir_inherit));
                }
            } else {
                // Files contribute bytes here; subdirs contribute theirs
                // later when their own listings propagate.
                d_files += 1;
                d_logical += e.logical;
                d_physical += e.physical;
            }
        }
        // One ancestor walk per directory listing: readers always see
        // consistent, only-growing numbers (design 1d). Doing it per-entry
        // instead would let a reader observe a half-inserted directory.
        tree.propagate(dir_id, d_logical, d_physical, d_files, d_subdirs);
        // This directory's direct listing is done (children may still be
        // SCANNING themselves).
        if let Some(n) = tree.get_mut(dir_id) {
            n.flags &= !flags::SCANNING;
        }
        node_count = tree.len() as u64;
    }

    // Node ceiling (CORE-STAB: directory-bomb defense). Once the arena
    // reaches the limit, stop enqueueing new directories; in-flight workers
    // finish their current listings (so the final count may sit one
    // directory-width past the ceiling) and the queue drains normally. The
    // report note is pushed exactly once via the CAS latch.
    if sh.options.max_nodes != 0
        && node_count >= sh.options.max_nodes
        && sh
            .node_limit_hit
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    {
        sh.model.errors.lock().unwrap().push(ScanError {
            path: dir_path.to_path_buf(),
            message: format!(
                "node limit {} reached; results are partial",
                sh.options.max_nodes
            ),
        });
    }

    // Extension aggregation, incremental (CORE-EXT-1). Done outside the
    // tree lock: ext_stats has its own lock and readers of one don't need
    // the other to be in sync at every instant (totals reconcile at end).
    if d_files > 0 {
        let mut stats = sh.model.ext_stats.write().unwrap();
        for e in found.iter().filter(|e| !e.is_subdir) {
            if e.ext_id != u32::MAX {
                let s = &mut stats[e.ext_id as usize];
                s.logical += e.logical;
                s.files += 1;
            }
        }
    }

    // Lock-free live counters for cheap progress polling.
    sh.model
        .items
        .fetch_add(found.len() as u64, Ordering::Relaxed);
    sh.model.bytes.fetch_add(d_logical, Ordering::Relaxed);
    sh.maybe_report(dir_path, false);

    // Publish new work last, and only if there is any: notify_all (not
    // notify_one) because several parked workers can be put to use when a
    // wide directory yields many subdirectories at once. Suppressed once the
    // node ceiling latched, so the scan winds down instead of growing.
    if !subdir_jobs.is_empty() && !sh.node_limit_hit.load(Ordering::Acquire) {
        let mut q = sh.queue.queue.lock().unwrap();
        q.items.extend(subdir_jobs);
        drop(q);
        sh.queue.cond.notify_all();
    }
}

/// Re-scan one subtree in place and fix ancestors (CORE-SCAN-12, the
/// post-mutation consistency call behind design 1e's "map reconciles after
/// commit"). Synchronous; intended for post-Trash refresh of one node.
///
/// The sequence is subtract-old -> rescan -> graft -> re-add:
/// 1. Snapshot the node's old aggregates and detach its children, then
///    subtract the old contribution from every ancestor. The tree is now
///    consistent "as if the subtree were empty".
/// 2. Stat the path again and branch on what is there now:
///    - still a directory: run a nested mini-Scan of just that path, graft
///      the resulting tree under the node, and add the new totals back up
///      the ancestor chain;
///    - still a file: restore its (possibly changed) size and re-add it;
///    - vanished: unlink the node from its parent entirely.
///
/// Errors only if `id` is not a valid node. Must not run concurrently with
/// a scan on the same model (documented FFI contract, CORE-FFI-SAFE-4):
/// each step is write-locked so readers stay consistent, but a concurrent
/// scan could re-add bytes between the subtract and re-add steps.
pub fn refresh_node(model: &Arc<Model>, id: NodeId) -> std::io::Result<()> {
    // Snapshot under a read lock: path (for the re-stat), the old
    // aggregate tuple (what we must subtract), and the parent link.
    let (path, old, parent) = {
        let tree = model.tree.read().unwrap();
        let n = tree
            .get(id)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "bad node id"))?;
        (
            tree.abs_path(id),
            (n.logical, n.physical, n.files, n.subdirs),
            n.parent,
        )
    };

    // Step 1: detach children and zero the subtree's contribution.
    // The old child nodes stay in the arena (handles are never reused)
    // but become unreachable; hosts must re-query children after refresh.
    {
        let mut tree = model.tree.write().unwrap();
        if let Some(n) = tree.get_mut(id) {
            n.children.clear();
            n.logical = 0;
            n.physical = 0;
            // A file keeps counting itself (files == 1); a dir's file
            // count came entirely from descendants, so it resets to 0.
            n.files = if n.is_dir() { 0 } else { n.files };
            n.subdirs = 0;
        }
        if parent.is_valid() {
            // Subtract the old subtree from every ancestor (u64 wrapping-safe).
            // Note: `old.files`/`old.subdirs` are the node's own aggregate,
            // which for a file includes itself (files == 1) — the file
            // branch below re-adds it symmetrically.
            let mut cur = parent;
            while cur.is_valid() {
                let n = tree.get_mut(cur).unwrap();
                n.logical = n.logical.saturating_sub(old.0);
                n.physical = n.physical.saturating_sub(old.1);
                n.files = n.files.saturating_sub(old.2);
                n.subdirs = n.subdirs.saturating_sub(old.3);
                cur = n.parent;
            }
        }
    }

    // Step 2: look at what the path is NOW (not what it was).
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_dir() => {
            // Still a directory: re-scan the subtree with a nested
            // low-concurrency Scan (its own private Model), then graft.
            let sub = Scan::begin(
                &path,
                ScanOptions {
                    max_concurrency: 2,
                    ..ScanOptions::default()
                },
                None,
            );
            if let Ok(mut sub) = sub {
                sub.join(); // synchronous: wait for the mini-scan
                let sub_tree = sub.model.tree.read().unwrap();
                let mut tree = model.tree.write().unwrap();
                // Deep-copy the mini-scan's nodes (fresh ids in `tree`)
                // under the refreshed node.
                graft(&mut tree, id, &sub_tree, NodeId::ROOT);
                // The mini-scan's root aggregates ARE the new subtree
                // totals; set them on `id` and add them to true ancestors.
                let root = sub_tree.get(NodeId::ROOT).unwrap();
                let (l, p, f, s) = (root.logical, root.physical, root.files, root.subdirs);
                drop(sub_tree);
                let mut cur = id;
                while cur.is_valid() {
                    let n = tree.get_mut(cur).unwrap();
                    if cur != id {
                        n.logical = n.logical.saturating_add(l);
                        n.physical = n.physical.saturating_add(p);
                        n.files = n.files.saturating_add(f);
                        n.subdirs = n.subdirs.saturating_add(s);
                    } else {
                        // The node itself is assigned, not added: its
                        // aggregates were zeroed in step 1.
                        n.logical = l;
                        n.physical = p;
                        n.files = f;
                        n.subdirs = s;
                    }
                    cur = n.parent;
                }
            }
            // A failed sub-scan leaves the subtree empty-but-consistent.
        }
        Ok(meta) => {
            // Still a file: restore its own (possibly changed) size.
            let mut tree = model.tree.write().unwrap();
            let logical = meta.len();
            let physical = physical_of(&meta);
            if let Some(n) = tree.get_mut(id) {
                n.logical = logical;
                n.physical = physical;
                n.mtime = mtime_of(&meta);
            }
            // Re-add to ancestors, mirroring the subtraction in step 1
            // (which removed this file's bytes and its files == 1).
            let mut cur = parent;
            while cur.is_valid() {
                let n = tree.get_mut(cur).unwrap();
                n.logical = n.logical.saturating_add(logical);
                n.physical = n.physical.saturating_add(physical);
                n.files = n.files.saturating_add(1);
                cur = n.parent;
            }
            // The node itself counts as one file again.
            if let Some(n) = tree.get_mut(id) {
                n.files = 1;
            }
        }
        Err(_) => {
            // Vanished (trashed/deleted): remove from the parent's child
            // list so it disappears from listings. The subtree's
            // bytes/counts were subtracted above; what remains is the
            // node's own presence: a directory still counted as one subdir
            // in every ancestor (subtract it here), while a file already
            // carried itself in `old.files` and needs nothing more.
            let mut tree = model.tree.write().unwrap();
            let was_dir = tree.get(id).map(|n| n.is_dir()).unwrap_or(false);
            if parent.is_valid() {
                if let Some(p) = tree.get_mut(parent) {
                    p.children.retain(|&c| c != id);
                }
                if was_dir {
                    let mut cur = parent;
                    while cur.is_valid() {
                        let n = tree.get_mut(cur).unwrap();
                        n.subdirs = n.subdirs.saturating_sub(1);
                        cur = n.parent;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Copy a scanned subtree from `src` (rooted at `src_id`) under `dst_id`.
/// Aggregates are copied as-is (the sub-scan already computed them); only
/// `ext_id` is dropped because the interning tables of the two models don't
/// correspond.
///
/// ITERATIVE by design (security, dirstat-core#6): a recursive walk here
/// used one stack frame per directory level, so a maliciously deep tree
/// (trivially created with nested mkdir) could overflow the stack when a
/// host runs Re-scan From Here on it — and a stack overflow *aborts* the
/// process, bypassing the FFI `catch_unwind` guard. An explicit work stack
/// bounds memory to the heap instead. Order differs from the old
/// depth-first recursion (it now processes a directory's whole child list
/// before descending), but the resulting tree is identical. This is the
/// only place in the crate that walks unbounded tree depth without an
/// explicit stack or depth cap; keep it that way.
fn graft(dst: &mut Tree, dst_id: NodeId, src: &Tree, src_id: NodeId) {
    // Each work item pairs a source node with the destination parent its
    // children should be attached under.
    let mut stack: Vec<(NodeId, NodeId)> = vec![(src_id, dst_id)];
    while let Some((src_parent, dst_parent)) = stack.pop() {
        let children: Vec<NodeId> = src.get(src_parent).unwrap().children.clone();
        for cid in children {
            let c = src.get(cid).unwrap();
            let new_id = dst.push(Node {
                name: c.name.clone(),
                parent: dst_parent,
                children: Vec::new(),
                kind: c.kind,
                logical: c.logical,
                physical: c.physical,
                files: c.files,
                subdirs: c.subdirs,
                mtime: c.mtime,
                flags: c.flags,
                ext_id: u32::MAX, // ext stats not re-aggregated on refresh (documented)
                category: c.category,
            });
            // Descend only into directories; leaves have no children to copy.
            if c.is_dir() {
                stack.push((cid, new_id));
            }
        }
    }
}

/// Extension of a file name, lowercased, or `None` if it has none.
/// Dotfiles (".bashrc"), trailing dots, and "extensions" longer than 15
/// bytes (which are almost always not extensions) don't count. The 15-byte
/// cap also guarantees the string fits `DsTypeStat.ext`'s 16-byte buffer.
fn extension_of(name: &str) -> Option<String> {
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty() || ext.is_empty() || ext.len() > 15 {
        return None; // dotfiles and absurd "extensions" don't count
    }
    Some(ext.to_ascii_lowercase())
}

/// mtime as seconds since the Unix epoch; 0 when unavailable (matches the
/// `Node.mtime` "unknown" convention).
fn mtime_of(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// Per-platform metadata shims. On Unix these read the stat fields the
// dedup/mount logic needs; the non-Unix fallbacks return values that
// safely disable those features (dev/ino 0 = no dedup keys, nlink 1 = no
// hard-link handling, physical = logical). This is what keeps the crate
// building and testing on non-Unix CI (spec §0).

#[cfg(unix)]
fn device_of(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.dev()
}
#[cfg(unix)]
fn inode_of(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.ino()
}
#[cfg(unix)]
fn nlink_of(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.nlink()
}
#[cfg(unix)]
fn physical_of(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    // st_blocks is always in 512-byte units regardless of the volume's
    // block size (POSIX), so this IS the allocated on-disk figure.
    meta.blocks() * 512
}

#[cfg(not(unix))]
fn device_of(_meta: &std::fs::Metadata) -> u64 {
    0
}
#[cfg(not(unix))]
fn inode_of(_meta: &std::fs::Metadata) -> u64 {
    0
}
#[cfg(not(unix))]
fn nlink_of(_meta: &std::fs::Metadata) -> u64 {
    1
}
#[cfg(not(unix))]
fn physical_of(meta: &std::fs::Metadata) -> u64 {
    meta.len()
}
