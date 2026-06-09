# dirstat-core — Engine Requirements, FFI Contract & Evals

**Version:** 1.0
**Status:** Implementation-ready
**Language:** Rust (2021 or 2024 edition)
**License:** MIT OR Apache-2.0 (dual, the Rust-ecosystem norm) — standalone open-source project
**Role:** The UI-free, platform-light disk-usage **engine** behind MacDirStat. Owns scanning, the tree data model, all sizing/aggregation/treemap math, save/reload, and the C-ABI exposed to host UIs. Knows nothing about AppKit, SwiftUI, windows, or menus.
**Consumers:** the MacDirStat Swift app (primary); potentially a CLI, a Linux/Windows GUI, or a WASM demo (why it is its own repo).

This document is one of three:
- `dirstat-core-Spec.md` (this file) — the engine.
- `MacDirStat-App-Spec.md` — the Swift/SwiftUI native app that links this engine.
- The original `MacDirStat-Requirements.md` / `-Evals.md` remain as the product-level parity reference; this split supersedes their stack assumption.

---

## 0. Design principles

- **No UI, no platform UI frameworks.** The crate **MUST** build and pass its full test suite on macOS, Linux, and Windows CI (even if MacDirStat only ships macOS). This is what guarantees the engine is genuinely reusable and keeps the boundary honest.
- **Deterministic.** Given the same filesystem state and options, results (sizes, counts, color assignment, layout for a given rectangle) **MUST** be reproducible.
- **Pure core + thin FFI shell.** Business logic lives in safe Rust; `unsafe` is confined to a small `ffi` module that is the only place that touches raw pointers.
- **Errors are values.** The engine **MUST NOT** panic across the FFI boundary; every fallible operation returns a status code + retrievable error detail.

Requirement IDs: `CORE-<AREA>-<n>`. RFC-2119 keywords apply. Each maps to an eval in §13.

---

## 1. Scan engine (owns original FR-SCAN-*, FR-PERM data side)

- **CORE-SCAN-1** Recursively traverse a target path, recording per file: name, full path, logical size, physical/allocated size, mtime, ctime, and raw attribute flags. On macOS the implementation **SHOULD** use `getattrlistbulk` for batched directory reads; on other platforms it uses the platform-appropriate equivalent. The *requirement* is the data, not the syscall.
- **CORE-SCAN-2** Compute, per directory, aggregate subtree logical size, physical size, file count, subdir count, and total item count.
- **CORE-SCAN-3** A directory's size is defined solely as the sum of its descendants.
- **CORE-SCAN-4** Track logical and physical size independently; expose both. Selecting which is "primary" is a host concern, but both values are always available from the engine.
- **CORE-SCAN-5** Detect hard links by (device, inode) and count shared inodes once toward totals; record the set of paths sharing each inode.
- **CORE-SCAN-6** Do not follow symlinks by default; count the link's own size. Following is an option (§4).
- **CORE-SCAN-7** Do not cross filesystem/mount boundaries by default; crossing is an option (§4).
- **CORE-SCAN-8** Treat bundle directories as ordinary directories by default; "opaque bundle" is an option (the *list* of bundle extensions is supplied by the host, since "what is a bundle" is platform/UTI knowledge).
- **CORE-SCAN-9** Parallelize traversal across cores (e.g., a work-stealing pool such as rayon) while keeping results deterministic in aggregate. Concurrency **MUST** be bounded/configurable.
- **CORE-SCAN-10** Emit progress to the host: items discovered, bytes counted, current path, and a completion estimate when available. Progress is delivered via a host-registered callback (§9) at a throttled rate.
- **CORE-SCAN-11** Support cancellation: a cancel flag the host can set; the scan **MUST** stop promptly (< 1 s) and return partial-but-consistent results.
- **CORE-SCAN-12** Support refresh of a subtree (re-read one node and its descendants) and refresh-all (re-scan roots), updating aggregates and ancestors.
- **CORE-SCAN-13** Handle scan-time errors (permission denied, vanished file, race) without aborting; record each in a retrievable scan-report log. Permission-denied subtrees feed `<Unknown>` (§3).
- **CORE-SCAN-14** Support multiple roots in one model; each is a top-level node.

### Limits (parity)
- **CORE-LIMIT-1** Sizes accumulate to 2^63−1 without overflow (use 64-bit unsigned/`u64` accumulators with checked/saturating math at the cap).
- **CORE-LIMIT-2** Handle ≥2^31 direct children per directory and billions of total items (memory permitting) without logic error.
- **CORE-LIMIT-3** Handle long/deep paths and 255-byte name components without truncation; paths stored as `OsString`/bytes, not lossy UTF-8.

---

## 2. Tree data model (owns the data behind original Directory List / coupling)

- **CORE-TREE-1** Maintain an in-memory tree of nodes; each node exposes: name, kind (file / directory / synthetic), logical size, physical size, file/subdir/item counts, mtime, ctime, attribute flags, parent handle, and child handles.
- **CORE-TREE-2** Assign every node a stable opaque `NodeId` (e.g., a `u64` index / generational handle) valid for the life of the loaded model. The host references nodes only by `NodeId`.
- **CORE-TREE-3** Provide sorted child enumeration: given a `NodeId`, a sort key (size / name / count / mtime / type), and direction, return children in that order. Sorting is **within one parent only** (never flattens the tree) and uses a stable secondary key = previous sort column.
- **CORE-TREE-4** Provide the path from any node to its root (for "reveal/expand to this node").
- **CORE-TREE-5** Compute per-node "subtree %" (relative to expanded parent) and "percent" (relative to root) on request.
- **CORE-TREE-6** Counts invariant: `items == files + subdirs` at every node.
- **CORE-TREE-7** Attribute flags are stored raw and exposed as a bitset; *interpretation/labeling* (Hidden/Locked/Package/Compressed/etc.) is the host's job. Engine supplies the bits and OS-level facts (is-symlink, is-hidden-by-dotfile, has-compression-flag) it can determine portably.

---

## 3. Synthetic nodes (owns original FR-SYN-* math)

- **CORE-SYN-1** `<Files>`: each directory exposes a synthetic node aggregating only its *immediate* file children. Omitted when the directory has ≤1 file or no subdirectories. (Parity-exact.)
- **CORE-SYN-2** `<Free Space>`: when the host enables it and the root is a whole volume, expose a node equal to volume free space. The host supplies volume capacity/free figures (the engine cannot portably know "this path is a volume root with N free bytes"); the engine integrates them into the model and totals.
- **CORE-SYN-3** `<Unknown>`: when enabled and root is a volume, expose `total_capacity − free − measured_sum`. Clamp to ≥0; if a clamp occurs, note it in the scan report.
- **CORE-SYN-4** Synthetic nodes are flagged as such so the host can forbid destructive actions and render fixed colors. They carry no real filesystem path.

---

## 4. Scan options (owns original FR-CFG scanning toggles)

A `ScanOptions` struct passed at scan start, all with documented defaults:
- **CORE-OPT-1** cross_filesystems (default false)
- **CORE-OPT-2** follow_symlinks (default false)
- **CORE-OPT-3** follow_firmlinks/aliases where determinable (default false)
- **CORE-OPT-4** opaque_bundle_extensions: host-supplied list (default empty → traverse all)
- **CORE-OPT-5** include_hidden (default true)
- **CORE-OPT-6** show_free_space / show_unknown (default false; require host-supplied volume figures)
- **CORE-OPT-7** max_concurrency (default = available parallelism)
- **CORE-OPT-8** primary_metric hint (logical/physical) — informational; both are always computed.

---

## 5. Type / extension aggregation (owns original FR-EXT-* math + color assignment)

- **CORE-EXT-1** Aggregate, across the whole tree, per file extension: total bytes (logical and physical), file count. Comparison case-insensitive; files with no extension grouped under a sentinel "(none)" key.
- **CORE-EXT-2** Provide the type list sorted by total bytes descending by default.
- **CORE-EXT-3** Assign distinct palette indices to the **top 12** types by bytes; all others map to a single "other" index. Assignment is deterministic for a given scan.
- **CORE-EXT-4** Expose the type→palette-index mapping so host legend and host treemap rendering agree. **Actual RGB values are host-supplied** (the host owns theming, dark mode, color-blind palettes); the engine only owns *which type gets slot N*.
- **CORE-EXT-5** Per-type % of total tree bytes is computed by the engine.
- **CORE-EXT-6** Type totals (excluding synthetic nodes) sum to total real-file bytes (data-correctness invariant).

---

## 6. Treemap layout geometry (owns original FR-TM geometry + FR-DATA-3)

The engine computes **rectangles**, not pixels. Rendering (cushion shading, gradients, grid, selection frame, colors) is the host's job; geometry and the cushion *surface coefficients* are the engine's.

- **CORE-TM-1** Given a root `NodeId` and a target rectangle (x, y, w, h in logical units), compute a treemap layout: a list of `(NodeId, rect)` for files, nested so each directory's rect contains its children. Area ∝ size; within a directory the ratio of two files' areas equals the ratio of their sizes (subject to a documented minimum-rect threshold applied consistently).
- **CORE-TM-2** Support both **KDirStat-style** (row layout) and **Squarified/SequoiaView-style** (aspect-ratio-minimizing) algorithms, selectable per call. KDirStat is default.
- **CORE-TM-3** For cushion treemaps, compute per-rectangle surface/relief coefficients (the Bruls/Huizing/van Wijk/van de Wetering cushion parameters) and expose them so the host shader/renderer can produce the 3D-relief shading. Shading *parameters* (height, ambient, scale, brightness) are inputs; the engine returns the per-rect coefficients needed to render them.
- **CORE-TM-4** Provide hit-testing: given a point in the laid-out rectangle and a computed layout, return the `NodeId` of the file whose rect contains it.
- **CORE-TM-5** Layout **MUST** be incremental/region-aware enough that the host can lay out only a visible sub-rectangle or a zoomed subtree (supports zoom-in/out and large trees within perf budgets).
- **CORE-TM-6** Layout is a pure function of (subtree, target rect, algorithm, min-rect threshold) → deterministic output.

---

## 7. Units & numeric helpers (owns original FR-DATA-6, FR-CFG-17)

- **CORE-UNIT-1** Provide exact binary (1 KiB = 1024) and decimal (1 KB = 1000) formatting helpers. The engine returns raw byte counts always; formatting helpers are convenience and **MUST** be exact at the conversion boundaries.

---

## 8. Save / reload (owns original FR-SCAN-17/18)

- **CORE-IO-1** Serialize a completed model (full tree, sizes both metrics, counts, dates, attribute bits, synthetic values, type aggregation, scan timestamp, options used) to a versioned file format, and deserialize it back to an equivalent model without re-scanning.
- **CORE-IO-2** The format **MUST** be versioned and forward-tolerant (reject unknown future versions cleanly with an error, never a panic).
- **CORE-IO-3** A reloaded model is flagged `is_snapshot = true` with its timestamp, so the host can disable on-disk actions.

---

## 9. C-ABI / FFI contract (the seam — NEW)

This is the public boundary the Swift app links against. It **MUST** be a C ABI (`#[no_mangle] extern "C"`), with a generated C header (e.g., via `cbindgen`).

### 9.1 Boundary design (recommended approach)

**Rust owns the tree; Swift holds opaque handles and pulls data on demand; the treemap layout is returned as one flattened contiguous buffer.** Rationale: a multi-million-node tree must never be deep-copied across FFI (kills NFR-MEM/PERF). The host only ever materializes data for *visible* rows (the outline shows a few hundred at a time) and for the *currently laid-out* treemap rectangles. This satisfies the perf/memory budgets while keeping FFI call counts low for the one genuinely bulk operation (treemap rects).

Concretely:
- A scan produces an opaque `EngineModel*` handle.
- Tree navigation is **lazy accessors**: the host calls `node_children(model, node_id, sort, dir) -> ChildIdList`, `node_info(model, node_id) -> NodeInfo` (a flat C struct of scalars; strings via a borrow+copy call), etc. Hosts cache what they display.
- Treemap layout is **bulk**: `treemap_layout(model, root_id, rect, algo, min_rect, &out_buffer, &out_len)` fills a host-owned or engine-owned contiguous array of `{ node_id, x, y, w, h, palette_index, cushion_coeffs[] }`. One call per (re)layout, not per rectangle.
- Strings (paths, names) cross as UTF-8 byte buffers with explicit length; the host copies and the engine frees, or the engine writes into a host buffer with a two-call (size-then-fill) pattern. Never assume null-terminated-only.

### 9.2 Required exported functions (illustrative, not exhaustive — the eval suite is the contract)

- **CORE-FFI-1** Lifecycle: `engine_init()`, `engine_shutdown()`, `engine_last_error(msg_buf, len) -> needed_len`.
- **CORE-FFI-2** Scan: `scan_begin(roots[], n, options, progress_cb, cancel_flag) -> ScanHandle`, `scan_await(ScanHandle) -> EngineModel* | error`. Scanning runs on engine threads; progress is delivered via `progress_cb` (a C function pointer + opaque `user_data`), called on an engine thread — the host is responsible for hopping to its UI thread.
- **CORE-FFI-3** Model lifetime: `model_free(EngineModel*)`. All `NodeId`s are invalid after free.
- **CORE-FFI-4** Tree access: `model_roots`, `node_info`, `node_children`, `node_path`, `node_subtree_percent`, `node_percent`.
- **CORE-FFI-5** Types: `type_list`, `type_palette_index`.
- **CORE-FFI-6** Treemap: `treemap_layout`, `treemap_hit_test`, `treemap_free_layout`.
- **CORE-FFI-7** Synthetic/volume: `set_volume_figures(model, node_id, total, free)`, `enable_synthetic(model, kind, bool)`.
- **CORE-FFI-8** Actions support: the engine does **not** delete files or call Trash (that's host/OS). After the host performs a filesystem mutation, it calls `refresh_node(model, node_id)` and the engine re-reads + re-aggregates ancestors. (Keeps all OS-integration on the Swift side.)
- **CORE-FFI-9** Save/reload: `model_save(model, path)`, `model_load(path) -> EngineModel*`.
- **CORE-FFI-10** Memory & threading rules: every pointer's ownership and free-responsibility is documented in the header; the ABI is panic-safe (`catch_unwind` at every boundary → error code).

### 9.3 FFI safety requirements
- **CORE-FFI-SAFE-1** No Rust panic may unwind across the boundary; all are caught and converted to error codes.
- **CORE-FFI-SAFE-2** All exported types are `#[repr(C)]`; no Rust enums/strings/`Vec` cross raw.
- **CORE-FFI-SAFE-3** The header is generated and checked into both repos / version-pinned, so Swift and Rust cannot silently disagree.
- **CORE-FFI-SAFE-4** A thread-safety contract is documented: which calls are safe concurrently, which require the model be quiescent.

---

## 10. Quality / non-functional (owns original NFR-PERF/MEM/STAB on the engine side)

- **CORE-PERF-1** Scan throughput: ≥100,000 items in ≤~10 s on a typical SSD (machine-relative budget recorded in `perf-baseline.json`).
- **CORE-PERF-2** Treemap layout for 1,000,000 items completes within the recorded interactive budget; region/zoom layout is sub-linear in visible area.
- **CORE-MEM-1** Per-node memory footprint documented; multi-million-node trees fit in 16 GB. Strings interned/deduplicated where it pays (common directory path prefixes).
- **CORE-STAB-1** No panic/UB on pathological input: symlink cycles, zero-byte files, no-extension files, Unicode/emoji names, depth in the thousands, permission-denied subtrees.
- **CORE-STAB-2** `cargo test`, `cargo clippy -D warnings`, and `cargo miri` (on the `ffi` module's safe-reachable logic) pass in CI on macOS + Linux.

---

## 11. Packaging as OSS

- **CORE-PKG-1** Published as a Cargo crate + a C header + a prebuilt static lib artifact in CI releases for at least macOS (arm64 + x86_64, ideally a universal lib).
- **CORE-PKG-2** README documents the model, the algorithms (with citations to the cushion/squarified papers — not WinDirStat source), and the FFI usage with a minimal C example.
- **CORE-PKG-3** Dual MIT/Apache-2.0 license files present. No GPL code or GPL-licensed dependencies are linked. (Keeps both this crate and any host app license-flexible.)
- **CORE-PKG-4** Clean-room provenance note: engine written from public docs + published papers, not from WinDirStat or any GPL source.

---

## 12. Mapping from the original product requirements

| Original IDs | Lands in | Notes |
|---|---|---|
| FR-SCAN-1..16, NFR-LIMIT-* | dirstat-core §1 | Engine. Save/reload §8. |
| FR-DIR sorting/percent/counts math | §2 | Rendering/columns are app-side. |
| FR-SYN-* | §3 | Volume figures supplied by app. |
| FR-EXT math + 12-color slot assignment | §5 | RGB values app-side. |
| FR-TM geometry, FR-DATA-3 | §6 | Pixels/shading render app-side. |
| FR-DATA-1,2,4,5,6 | §2/§5/§7 | Correctness invariants live in engine. |
| FR-CFG scanning toggles | §4 | UI for them is app-side. |
| FR-SCAN-17/18 | §8 | |
| Everything UI/OS (views, coupling UX, Finder, Trash, settings UI, a11y, notarization, menus, toolbar, shortcuts) | **MacDirStat-App-Spec.md** | Not in engine. |

---

## 13. Engine evals (Rust tests — fast, no UI)

Fixtures mirror the originals (FIX-TINY/TYPES/EDGE/LARGE/VOLUME) as on-disk temp trees with JSON ground-truth manifests. All evals are `cargo test` / `cargo bench` / fuzz targets.

| Eval ID | Requirement | Type | Pass criteria |
|---|---|---|---|
| EVC-SCAN-1 | CORE-SCAN-1 | integration | Per-file fields on FIX-TINY match manifest. |
| EVC-SCAN-2 | CORE-SCAN-2/3 | integration | root=20000, sub=16000, deep=10000; counts exact. |
| EVC-SCAN-4 | CORE-SCAN-4 | integration | Sparse/clone file: logical≠physical, both correct. |
| EVC-SCAN-5 | CORE-SCAN-5 | integration | Hard-link pair counted once; both paths recorded. |
| EVC-SCAN-6 | CORE-SCAN-6 | integration | Symlink not followed by default. |
| EVC-SCAN-7 | CORE-SCAN-7 | integration | Mount boundary not crossed by default. |
| EVC-SCAN-9 | CORE-SCAN-9 | perf | >1 core used; aggregate result deterministic across 5 runs. |
| EVC-SCAN-10 | CORE-SCAN-10 | integration | Progress callback fires with monotonic counts. |
| EVC-SCAN-11 | CORE-SCAN-11 | integration | Cancel stops <1s; partial result internally consistent. |
| EVC-SCAN-12 | CORE-SCAN-12 | integration | Refresh subtree updates node + ancestors. |
| EVC-SCAN-13 | CORE-SCAN-13 | integration | Denied dir → scan-report entry → feeds `<Unknown>`; no abort. |
| EVC-LIMIT-1 | CORE-LIMIT-1 | unit | Accumulate near 2^63−1; no overflow. |
| EVC-LIMIT-2 | CORE-LIMIT-2 | unit | 2^31-child node counts sum correctly. |
| EVC-LIMIT-3 | CORE-LIMIT-3 | integration | Long/deep/255-byte names intact. |
| EVC-TREE-3 | CORE-TREE-3 | unit | Sort within-parent only; stable secondary key; tree never flattened. |
| EVC-TREE-5 | CORE-TREE-5 | unit | Subtree% vs parent, percent vs root match manifest. |
| EVC-TREE-6 | CORE-TREE-6 | unit | items==files+subdirs everywhere. |
| EVC-SYN-1 | CORE-SYN-1 | unit | `<Files>` aggregates immediate files; omitted on ≤1 file / no-subdir. |
| EVC-SYN-2 | CORE-SYN-2 | integration | `<Free Space>` == supplied free figure. |
| EVC-SYN-3 | CORE-SYN-3 | unit | `<Unknown>` == total−free−sum; clamps ≥0 with report note. |
| EVC-EXT-1 | CORE-EXT-1 | integration | Per-type bytes/counts on FIX-TYPES match; (none) group correct; case-insensitive. |
| EVC-EXT-3 | CORE-EXT-3 | unit | Exactly top-12 get distinct slots; rest → "other"; deterministic. |
| EVC-EXT-6 | CORE-EXT-6 | unit | Type totals sum to total real-file bytes. |
| EVC-TM-1 | CORE-TM-1 | unit | Area∝size; within-dir area ratio==size ratio (±min-rect tolerance). |
| EVC-TM-2 | CORE-TM-2 | unit | KDirStat & squarified both area-correct; squarified max-aspect < slice-and-dice. |
| EVC-TM-3 | CORE-TM-3 | unit | Cushion coefficients present and vary monotonically with height/ambient inputs. |
| EVC-TM-4 | CORE-TM-4 | unit | Hit-test returns containing node for sampled points. |
| EVC-TM-5/6 | CORE-TM-5/6 | unit | Region/zoom layout deterministic; sub-rect layout matches full-layout for that region. |
| EVC-UNIT-1 | CORE-UNIT-1 | unit | 1 KiB=1024, 1 MiB=1024 KiB, 1 GiB=1024 MiB; decimal exact too. |
| EVC-IO-1 | CORE-IO-1 | integration | Save→load round-trips byte-identical model; snapshot flagged. |
| EVC-IO-2 | CORE-IO-2 | unit | Unknown future version → clean error, no panic. |
| EVC-FFI-1 | CORE-FFI-1..9 | integration | A C (or Swift) harness drives init→scan→navigate→layout→save→load→free; values match the equivalent Rust-native test. |
| EVC-FFI-SAFE-1 | CORE-FFI-SAFE-1 | unit | A deliberately-panicking internal path returns an error code, does not unwind across FFI. |
| EVC-FFI-SAFE-3 | CORE-FFI-SAFE-3 | CI | Generated header matches committed header (diff = empty) or build fails. |
| EVC-PERF-1 | CORE-PERF-1 | bench | FIX-LARGE scan within recorded budget; >20% regression fails. |
| EVC-PERF-2 | CORE-PERF-2 | bench | 1M-item layout within budget; region layout sub-linear. |
| EVC-MEM-1 | CORE-MEM-1 | bench | FIX-LARGE within 16 GB; footprint recorded. |
| EVC-STAB-1 | CORE-STAB-1 | fuzz | Pathological trees: no panic/UB. |
| EVC-STAB-2 | CORE-STAB-2 | CI | clippy -D warnings, miri, multi-OS test suite green. |

### Coverage gate
Build releasable only when every `CORE-*` ID appears in ≥1 eval, all MUST-level pass, and data-correctness + stability + FFI-safety evals (no deferral allowed) pass.
