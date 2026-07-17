//! ============================================================================
//! FILE: src/lib.rs
//!
//! ============================================================================
//!
//! # Purpose
//! Crate root of `dirstat-core`, the UI-free disk-usage engine behind
//! MacDirStat: parallel filesystem scan, in-memory directory tree, kind/age
//! classification, extension aggregation, treemap geometry, and a panic-safe
//! C ABI for host UIs. The key design decision is the ownership split: the
//! engine owns *facts about the data* (sizes, counts, percentages, sort
//! order, aggregation, layout rectangles) while hosts own *presentation*
//! (pixels, RGB palettes, OS integration). See `dirstat-core-Spec.md` for the
//! full contract; requirement IDs referenced throughout the crate look like
//! `CORE-SCAN-1`, and each maps to an eval (`EVC-*`) in the test suite.
//!
//! Clean-room provenance: written from the public spec documents and the
//! published treemap papers (Bruls/Huizing/van Wijk, "Squarified Treemaps"),
//! not from WinDirStat or any GPL source.
//!
//! # Upstream dependencies (what this file consumes)
//! - crate::classify — re-exports `Category` (kind buckets)
//! - crate::scan — re-exports `Model`, `Progress`, `Scan`, `ScanOptions`
//! - crate::tree — re-exports `NodeId`, `NodeKind`, `SortKey`, `Tree`
//! - crate::treemap — re-exports `hit_test`, `layout`, `Algorithm`,
//!   `LayoutParams`, `LayoutRect`
//! - (no std facilities used directly; this file is declarations only)
//!
//! # Downstream consumers (who depends on this file)
//! - src/ffi.rs and every other module — via the module declarations here
//! - tests/engine.rs — uses the re-exports (`dirstat_core::Tree`,
//!   `dirstat_core::Category`, ...) as the public Rust API surface
//! - External Rust users (a CLI, a WASM demo) — the re-export list below is
//!   the intended "nice" Rust API; the Swift app instead consumes the C ABI
//!   in src/ffi.rs through the cbindgen-generated `include/dirstat_core.h`
//!
//! # Structure
//! - module declarations — classify, ffi, format, log, scan, tree, treemap
//! - `pub use` re-exports — flatten the most-used types to the crate root
//!
//! # Algorithm & invariants
//! None here; this file only wires the module tree together. Cross-module
//! invariants (only-growing aggregates, determinism, panic containment at
//! the FFI boundary) are documented in the module that enforces them.
//!
//! ============================================================================

pub mod classify;
pub mod ffi;
pub mod format;
pub mod log;
pub mod scan;
pub mod tree;
pub mod treemap;

pub use classify::Category;
pub use scan::{Model, Progress, Scan, ScanOptions};
pub use tree::{NodeId, NodeKind, SortKey, Tree};
pub use treemap::{hit_test, layout, Algorithm, LayoutParams, LayoutRect};
