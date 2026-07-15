//! # dirstat-core
//!
//! UI-free disk-usage engine: parallel filesystem scan, in-memory directory
//! tree, kind/age classification, extension aggregation, treemap geometry,
//! and a panic-safe C ABI for host UIs (MacDirStat is the primary consumer).
//!
//! The engine owns *facts about the data* — sizes, counts, percentages,
//! sort order, aggregation, layout rectangles. Hosts own *presentation* —
//! pixels, RGB palettes, OS integration. See `dirstat-core-Spec.md`.
//!
//! Clean-room provenance: written from the public spec documents and the
//! published treemap papers (Bruls/Huizing/van Wijk, "Squarified Treemaps"),
//! not from WinDirStat or any GPL source.

pub mod classify;
pub mod ffi;
pub mod format;
pub mod scan;
pub mod tree;
pub mod treemap;

pub use classify::Category;
pub use scan::{Model, Progress, Scan, ScanOptions};
pub use tree::{NodeId, NodeKind, SortKey, Tree};
pub use treemap::{hit_test, layout, Algorithm, LayoutParams, LayoutRect};
