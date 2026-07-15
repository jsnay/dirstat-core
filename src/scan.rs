//! Parallel filesystem scanner (CORE-SCAN-*).
//!
//! Design: a bounded worker pool pulls directories off a shared queue and
//! lists them; results are inserted into the shared [`Model`] under a write
//! lock. Aggregates propagate to ancestors once per directory listing, so a
//! reader taking the read lock at any moment sees an internally consistent
//! tree whose numbers only ever grow — the invariant behind the progressive
//! UI (design 1d: "counters appear instantly and only go up").

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::SystemTime;

use crate::classify::{self, Category};
use crate::tree::{flags, Node, NodeId, NodeKind, Tree};

/// Scan options (CORE-OPT-*), all defaults documented.
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
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            cross_filesystems: false,
            follow_symlinks: false,
            include_hidden: true,
            max_concurrency: 0,
        }
    }
}

/// One scan-report entry (CORE-SCAN-13).
#[derive(Debug, Clone)]
pub struct ScanError {
    pub path: PathBuf,
    pub message: String,
}

/// Extension aggregation entry, maintained incrementally during the scan
/// (CORE-EXT-1). Index in `Model.ext_stats` == a node's `ext_id`.
#[derive(Debug, Clone, Default)]
pub struct ExtStat {
    pub ext: String,
    pub logical: u64,
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
    pub tree: RwLock<Tree>,
    pub ext_stats: RwLock<Vec<ExtStat>>,
    pub errors: Mutex<Vec<ScanError>>,
    pub volume: Mutex<Option<VolumeFigures>>,
    /// Reference instant for age buckets; set once at scan start.
    pub scan_started: SystemTime,
    pub items: AtomicU64,
    pub bytes: AtomicU64,
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

type ProgressCallback = Box<dyn Fn(&Progress) + Send + Sync>;

struct WorkQueue {
    queue: Mutex<QueueState>,
    cond: Condvar,
}

struct QueueState {
    items: Vec<(NodeId, PathBuf, Option<Category>)>,
    /// Directories claimed but not yet finished.
    in_flight: usize,
}

/// A running (or finished) scan. Owns worker threads; the model is shared.
pub struct Scan {
    pub model: Arc<Model>,
    cancel: Arc<AtomicBool>,
    workers: Vec<std::thread::JoinHandle<()>>,
}

impl Scan {
    /// Begin scanning `root` (CORE-SCAN-1/9). Returns immediately; workers
    /// run until complete or cancelled.
    pub fn begin(
        root: &Path,
        options: ScanOptions,
        progress: Option<ProgressCallback>,
    ) -> std::io::Result<Scan> {
        let meta = std::fs::symlink_metadata(root)?;
        let model = Arc::new(Model::new());
        let cancel = Arc::new(AtomicBool::new(false));

        let root_dev = device_of(&meta);
        let root_name = root
            .file_name()
            .map(OsString::from)
            .unwrap_or_else(|| OsString::from(root.as_os_str()));

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
        model.items.store(1, Ordering::Relaxed);

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
                queue: Mutex::new(QueueState {
                    items: vec![(NodeId::ROOT, root.to_path_buf(), None)],
                    in_flight: 0,
                }),
                cond: Condvar::new(),
            },
            ext_index: Mutex::new(HashMap::new()),
            inodes: Mutex::new(HashMap::new()),
            options,
            root_dev,
            cancel: Arc::clone(&cancel),
            progress,
            last_progress_ms: AtomicU64::new(0),
            started: std::time::Instant::now(),
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
    /// partial model stays consistent ("Stop keeps everything found so far").
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn is_complete(&self) -> bool {
        self.model.complete.load(Ordering::Acquire)
    }

    /// Block until the scan finishes (CORE-FFI-2 `scan_await`).
    pub fn join(&mut self) {
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

impl Drop for Scan {
    fn drop(&mut self) {
        self.cancel();
        self.join();
    }
}

struct ScanShared {
    model: Arc<Model>,
    queue: WorkQueue,
    ext_index: Mutex<HashMap<String, u32>>,
    inodes: Mutex<HashMap<(u64, u64), ()>>,
    options: ScanOptions,
    root_dev: u64,
    cancel: Arc<AtomicBool>,
    progress: Option<ProgressCallback>,
    last_progress_ms: AtomicU64,
    started: std::time::Instant,
}

impl ScanShared {
    /// Throttled progress emission (~every 100 ms), plus a final `done`.
    fn maybe_report(&self, current: &Path, done: bool) {
        let Some(cb) = &self.progress else { return };
        let now_ms = self.started.elapsed().as_millis() as u64;
        if !done {
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

fn worker_loop(sh: &ScanShared) {
    loop {
        let job = {
            let mut q = sh.queue.queue.lock().unwrap();
            loop {
                if sh.cancel.load(Ordering::Relaxed) {
                    q.items.clear();
                }
                if let Some(job) = q.items.pop() {
                    q.in_flight += 1;
                    break Some(job);
                }
                if q.in_flight == 0 {
                    break None; // no work anywhere: scan is over
                }
                q = sh.queue.cond.wait(q).unwrap();
            }
        };
        let Some((dir_id, dir_path, inherited)) = job else {
            // Wake any peers still waiting so they can observe termination.
            sh.queue.cond.notify_all();
            finish_if_first(sh);
            return;
        };

        process_dir(sh, dir_id, &dir_path, inherited);

        let mut q = sh.queue.queue.lock().unwrap();
        q.in_flight -= 1;
        if q.in_flight == 0 && q.items.is_empty() {
            drop(q);
            sh.queue.cond.notify_all();
        } else {
            drop(q);
            sh.queue.cond.notify_one();
        }
    }
}

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
fn process_dir(sh: &ScanShared, dir_id: NodeId, dir_path: &Path, inherited: Option<Category>) {
    if sh.cancel.load(Ordering::Relaxed) {
        return;
    }
    let entries = match std::fs::read_dir(dir_path) {
        Ok(rd) => rd,
        Err(e) => {
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

    // Stat everything first without holding the tree lock.
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
            continue;
        }
        let path = entry.path();
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                sh.model.errors.lock().unwrap().push(ScanError {
                    path,
                    message: e.to_string(),
                });
                continue;
            }
        };

        let mut fl = if hidden { flags::HIDDEN } else { 0 };

        if meta.is_dir() {
            if !sh.options.cross_filesystems && device_of(&meta) != sh.root_dev {
                continue; // CORE-SCAN-7: don't cross mounts
            }
            let dir_cat = classify::category_for_dir_name(&name_str);
            let child_inherit = dir_cat.or(inherited);
            found.push(Entry {
                name,
                kind: NodeKind::Dir,
                logical: 0,
                physical: 0,
                mtime: mtime_of(&meta),
                flags: fl | flags::SCANNING,
                category: child_inherit.unwrap_or(Category::Other),
                ext_id: u32::MAX,
                subdir_inherit: child_inherit,
                is_subdir: true,
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
            // Hard links: count shared inodes once (CORE-SCAN-5).
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
            });
        }
    }

    // Deterministic aggregate independent of readdir order (CORE-SCAN-9).
    found.sort_by(|a, b| a.name.cmp(&b.name));

    let mut d_logical = 0u64;
    let mut d_physical = 0u64;
    let mut d_files = 0u64;
    let mut d_subdirs = 0u64;
    let mut subdir_jobs: Vec<(NodeId, PathBuf, Option<Category>)> = Vec::new();

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
                subdir_jobs.push((id, dir_path.join(&e.name), e.subdir_inherit));
            } else {
                d_files += 1;
                d_logical += e.logical;
                d_physical += e.physical;
            }
        }
        // One ancestor walk per directory listing: readers always see
        // consistent, only-growing numbers (design 1d).
        tree.propagate(dir_id, d_logical, d_physical, d_files, d_subdirs);
        if let Some(n) = tree.get_mut(dir_id) {
            n.flags &= !flags::SCANNING;
        }
    }

    // Extension aggregation, incremental (CORE-EXT-1).
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

    sh.model
        .items
        .fetch_add(found.len() as u64, Ordering::Relaxed);
    sh.model.bytes.fetch_add(d_logical, Ordering::Relaxed);
    sh.maybe_report(dir_path, false);

    if !subdir_jobs.is_empty() {
        let mut q = sh.queue.queue.lock().unwrap();
        q.items.extend(subdir_jobs);
        drop(q);
        sh.queue.cond.notify_all();
    }
}

/// Re-scan one subtree in place and fix ancestors (CORE-SCAN-12, the
/// post-mutation consistency call behind design 1e's "map reconciles after
/// commit"). Synchronous; intended for post-Trash refresh of one node.
pub fn refresh_node(model: &Arc<Model>, id: NodeId) -> std::io::Result<()> {
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

    // Detach children and zero the subtree's contribution.
    {
        let mut tree = model.tree.write().unwrap();
        if let Some(n) = tree.get_mut(id) {
            n.children.clear();
            n.logical = 0;
            n.physical = 0;
            n.files = if n.is_dir() { 0 } else { n.files };
            n.subdirs = 0;
        }
        if parent.is_valid() {
            // Subtract the old subtree from every ancestor (u64 wrapping-safe).
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

    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_dir() => {
            // Re-scan the subtree synchronously with a nested single-thread walk.
            let sub = Scan::begin(
                &path,
                ScanOptions {
                    max_concurrency: 2,
                    ..ScanOptions::default()
                },
                None,
            );
            if let Ok(mut sub) = sub {
                sub.join();
                let sub_tree = sub.model.tree.read().unwrap();
                let mut tree = model.tree.write().unwrap();
                graft(&mut tree, id, &sub_tree, NodeId::ROOT);
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
                        n.logical = l;
                        n.physical = p;
                        n.files = f;
                        n.subdirs = s;
                    }
                    cur = n.parent;
                }
            }
        }
        Ok(meta) => {
            // Still a file: restore its own size.
            let mut tree = model.tree.write().unwrap();
            let logical = meta.len();
            let physical = physical_of(&meta);
            if let Some(n) = tree.get_mut(id) {
                n.logical = logical;
                n.physical = physical;
                n.mtime = mtime_of(&meta);
            }
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
            // Gone (trashed/deleted): remove from the parent's child list.
            // The subtree's bytes/counts were subtracted above; the node
            // itself still counts as one subdir (dirs) in every ancestor —
            // files already carried themselves in `old.files`.
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
fn graft(dst: &mut Tree, dst_id: NodeId, src: &Tree, src_id: NodeId) {
    let src_node = src.get(src_id).unwrap();
    let children: Vec<NodeId> = src_node.children.clone();
    for cid in children {
        let c = src.get(cid).unwrap();
        let new_id = dst.push(Node {
            name: c.name.clone(),
            parent: dst_id,
            children: Vec::new(),
            kind: c.kind,
            logical: c.logical,
            physical: c.physical,
            files: c.files,
            subdirs: c.subdirs,
            mtime: c.mtime,
            flags: c.flags,
            category: c.category,
            ext_id: u32::MAX, // ext stats are not re-aggregated on refresh (documented)
        });
        graft(dst, new_id, src, cid);
    }
}

fn extension_of(name: &str) -> Option<String> {
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty() || ext.is_empty() || ext.len() > 15 {
        return None; // dotfiles and absurd "extensions" don't count
    }
    Some(ext.to_ascii_lowercase())
}

fn mtime_of(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

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
