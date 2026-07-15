# dirstat-core

UI-free disk-usage **engine**: parallel filesystem scan, in-memory directory
tree, kind/age classification, extension aggregation, treemap geometry, and a
panic-safe C ABI for host UIs. It is the engine behind
[MacDirStat](https://github.com/jsnay/macdirstat), and knows nothing about
AppKit, SwiftUI, windows, or menus.

The boundary rule: the engine owns **facts about the data** (sizes, counts,
percentages, sort order, aggregation, layout rectangles); hosts own
**presentation** (pixels, RGB palettes, OS integration). See
`dirstat-core-Spec.md` for the full requirements and eval suite.

## The model

- A **scan** (`Scan::begin`) walks a root on a bounded worker pool
  (work-stealing queue over `std::thread`; zero runtime dependencies) and
  builds an arena **tree** guarded by a `RwLock`. Aggregates propagate to
  ancestors once per directory listing, so concurrent readers always see an
  internally consistent tree whose numbers only grow — this is the contract
  that makes progressive "the scan is the show" UIs safe.
- Every node carries logical + physical size, file/subdir counts, mtime, raw
  attribute flags, a **kind category** (8 stable buckets: Developer, Audio &
  Video, Photos, Documents, Archives & Images, System & Caches, Applications,
  Other — derived from a curated extension + path-rule table, so
  `DerivedData` is Developer and a `.photoslibrary` bundle is one Photos
  block), and an interned extension id.
- **Age buckets** (this week / month / year / 1–2 years / older) are computed
  against the scan timestamp — deterministic for a given model.
- **Treemap layout** is a pure function `(subtree, rect, algorithm,
  min-rect) → flat buffer of rectangles`. Two algorithms: squarified
  (Bruls, Huizing & van Wijk, *Squarified Treemaps*, 1999) and a KDirStat-style
  strip layout. Hit-testing runs over the returned buffer.
- **Volume reconciliation**: the host supplies capacity/free figures; the
  engine owns `unknown = capacity − free − measured` (clamped ≥ 0) — the
  number a UI surfaces as "N GB unreadable".

## C ABI

The Swift app (or any host) links `libdirstat_core.a` against the generated,
checked-in header [`include/dirstat_core.h`](include/dirstat_core.h):

```c
#include "dirstat_core.h"

assert(ds_abi_version() == DS_ABI_VERSION);          // version pin
DsScan *scan = ds_scan_begin("/Users/me", NULL, on_progress, ctx);
DsModel *model = ds_scan_model(scan);                 // readable mid-scan
// ... poll ds_model_stats / re-layout on a cadence while scanning ...
ds_scan_join(scan);

uint64_t root = ds_model_root(model);
DsNodeInfo info;  ds_node_info(model, root, &info);

DsTmRect *rects; size_t n;
ds_treemap_layout(model, root, 1200, 800, /*squarified*/1, 2.0f, &rects, &n);
uint64_t hit = ds_treemap_hit_test(rects, n, 640, 400);
ds_treemap_free(rects, n);

ds_model_free(model);
ds_scan_free(scan);
```

Rules (documented per-function in the header):

- Panics never unwind across the boundary; failures return error codes and
  `ds_last_error` has the detail.
- The engine owns the tree. Hosts hold opaque `uint64_t` node ids and fetch
  visible rows lazily; treemap layout crosses as **one bulk buffer** per call.
- Strings are UTF-8, NUL-terminated, two-call size-then-fill.
- All `ds_model_*`/`ds_treemap_*` reads are safe from any thread, including
  concurrently with a running scan. Progress callbacks arrive on engine
  threads — hop to your UI thread yourself.

Regenerate the header after any `src/ffi.rs` change (CI fails on drift):

```sh
cargo install cbindgen && cbindgen --crate dirstat-core -o include/dirstat_core.h
```

## Building

```sh
cargo test                         # full suite, macOS/Linux
cargo build --release              # produces target/release/libdirstat_core.a
```

CI builds a universal (arm64 + x86_64) macOS static library artifact.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at
your option. Clean-room provenance: written from the public spec documents
and published treemap papers — no WinDirStat or other GPL source was read or
used, and there are no third-party runtime dependencies.
