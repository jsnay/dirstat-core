# dirstat-core

UI-free disk-usage **engine**: parallel filesystem scan, in-memory directory
tree, kind/age classification, extension aggregation, treemap geometry, and a
panic-safe C ABI for host UIs. It is the engine behind
[MacDirStat](https://github.com/jsnay/macdirstat), and knows nothing about
AppKit, SwiftUI, windows, or menus.

**The boundary rule** (the one sentence that explains every API decision
here): the engine owns *facts about the data* — sizes, counts, percentages,
sort order, aggregation, layout rectangles; hosts own *presentation* —
pixels, RGB palettes, OS integration. See `dirstat-core-Spec.md` for the full
requirements contract (`CORE-*` ids) and its eval suite (`EVC-*` ids).

## Repository map

| Path | What it is |
|---|---|
| `src/tree.rs` | The arena tree: stable `NodeId` handles, aggregate invariants, sorting, percent math. Dependency root of the crate. |
| `src/scan.rs` | The parallel scanner: worker pool, progress, cancellation, hard-link + directory-alias dedup, `refresh_node`. The tree's only mutator. |
| `src/classify.rs` | Kind classification: 8 stable categories from a curated extension + path-rule table (path rules win). |
| `src/treemap.rs` | Layout geometry: squarified + KDirStat algorithms, logical/physical metric, hit-testing. Pure functions. |
| `src/format.rs` | Exact binary/decimal byte formatting helpers. |
| `src/ffi.rs` | The C ABI — the only module with raw pointers. cbindgen reads its doc comments into the header. |
| `include/dirstat_core.h` | The **generated, checked-in** header hosts compile against. Never hand-edit; CI fails on drift. |
| `tests/engine.rs` | The `EVC-*` eval suite against real on-disk fixtures. |
| `tests/ffi.rs` | The C-ABI driven end-to-end + panic-safety tests (plays the role of the Swift host). |
| `deferrals.md` | Spec items intentionally not built yet, each with a reason. |

Every source file opens with a structured header (purpose, upstream
dependencies, downstream consumers, structure, algorithm & invariants) —
start there when reviewing.

## Data flow

```
   ds_scan_begin(root, options, callback)
        │  worker pool: read_dir → stat (no lock) → insert + propagate (write lock)
        ▼
   Model ── tree: RwLock<Tree>          arena of Nodes, aggregates only grow
        ├── ext_stats                   incremental per-extension totals
        ├── errors                      scan report (unreadable paths)
        └── volume figures              host-supplied; unknown = cap − free − measured
        │
        ├─ ds_node_info / ds_node_children / ds_node_path   lazy per-node facts
        ├─ ds_category_list / ds_type_list                  aggregations
        └─ ds_treemap_layout ──► one bulk DsTmRect buffer ──► ds_treemap_hit_test
```

Key invariants (each enforced and documented at its source):

- **Only-grow**: readers may take the read lock at any moment mid-scan and
  see consistent, monotonically increasing totals — this is what makes a
  progressive "scan is the show" UI safe.
- **Determinism**: identical filesystem state + options ⇒ identical
  aggregates and identical layout geometry, regardless of thread
  interleaving.
- **Counted once**: hard links dedup by `(device, inode)`; aliased
  directories (APFS firmlinks, bind mounts) dedup the same way, the second
  path becoming a zero-byte `DUPLICATE` entry.
- **Two size channels always**: logical (apparent) and physical (allocated)
  are both tracked everywhere; physical is the truthful channel when sparse
  files, APFS clones, or cloud-placeholder (dataless) files exist.

## C ABI

Hosts link `libdirstat_core.a` against the generated
[`include/dirstat_core.h`](include/dirstat_core.h). The header's comments are
the API reference — they are generated from `src/ffi.rs` doc comments.

```c
#include "dirstat_core.h"

assert(ds_abi_version() == DS_ABI_VERSION);          /* version pin */

DsScanOptions opts = {0};                            /* zero = defaults */
DsScan  *scan  = ds_scan_begin("/Users/me", &opts, on_progress, ctx);
DsModel *model = ds_scan_model(scan);                /* readable mid-scan */
/* ... poll ds_model_stats / re-layout on a cadence while scanning ... */
ds_scan_join(scan);

uint64_t root = ds_model_root(model);
DsNodeInfo info;  ds_node_info(model, root, &info);

DsTmRect *rects; size_t n;                           /* metric: 0 logical, 1 physical */
ds_treemap_layout(model, root, 1200, 800, /*squarified*/1, 2.0f, /*physical*/1, &rects, &n);
uint64_t hit = ds_treemap_hit_test(rects, n, 640, 400);
ds_treemap_free(rects, n);

ds_model_free(model);
ds_scan_free(scan);
```

Rules (documented per-function in the header):

- Panics never unwind across the boundary; failures return error codes and
  `ds_last_error` has the detail.
- The engine owns the tree. Hosts hold opaque `uint64_t` node ids, fetch
  visible rows lazily, and receive treemap layouts as **one bulk buffer**
  per call (freed with `ds_treemap_free`, never the host allocator).
- Strings are UTF-8, NUL-terminated, two-call size-then-fill.
- **Thread safety**: all `ds_model_*`/`ds_treemap_*` reads are safe from any
  thread, including concurrently with a running scan. `ds_refresh_node`
  requires no concurrent scan on the same model. Progress callbacks arrive
  on engine threads — hop to your UI thread yourself.

### ABI versions

| Version | Change |
|---|---|
| 1 | Initial surface: scan lifecycle, tree access, extension/category aggregation, treemap layout + hit test, volume reconciliation, refresh. |
| 2 | `DsScanOptions.skip_paths`; `DS_NODE_FLAG_DUPLICATE` (directory-alias dedup). |
| 3 | `ds_treemap_layout` `metric` parameter; sort key 4 (physical); `DsCategoryStat.physical`. |

Hosts check `ds_abi_version()` at startup; MacDirStat additionally diffs the
pinned header at build time. Regenerate after any `src/ffi.rs` change
(CI fails on drift):

```sh
cargo install cbindgen && cbindgen --crate dirstat-core -o include/dirstat_core.h
```

## Building & testing

```sh
cargo test                         # full eval suite (30 tests, EVC-* ids)
cargo clippy --all-targets -- -D warnings
cargo build --release              # target/release/libdirstat_core.a
```

Tests run against **real on-disk temp fixtures** — permissions, symlinks,
hard links, sparse files, and bind mounts only behave honestly on a real
filesystem. Environment-dependent tests (bind-mount aliasing, sparse files)
detect unsupported runners and skip gracefully, so the suite is green on
plain CI and exercises the real paths where privileges allow.

CI (`.github/workflows/ci.yml`): tests + clippy + fmt on **Linux and macOS**
(the multi-OS gate is what keeps the "UI-free" claim honest), a header-drift
job, and a universal (arm64 + x86_64) macOS static-library artifact.

## Consumers

- **MacDirStat** (primary): links the staticlib, pins the header, and builds
  the SwiftUI treemap UI on top.
- The re-exports in `src/lib.rs` are the intended Rust-native API for future
  consumers (a CLI, a WASM demo).

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at
your option. Clean-room provenance: written from the public spec documents
and published treemap papers (Bruls, Huizing & van Wijk, *Squarified
Treemaps*) — no WinDirStat or other GPL source was read or used, and there
are no third-party runtime dependencies.
