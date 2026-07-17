//! ============================================================================
//! FILE: src/log.rs
//!
//! ============================================================================
//!
//! # Purpose
//! Host-routed event logging. The engine is a library: it must never write
//! files or pick a log destination — the host owns I/O policy (the same
//! ownership split as everything else in this crate). Instead, the engine
//! emits a small, bounded stream of lifecycle events (scan start/done,
//! refresh timings, ceiling hits, cancellation) through one globally
//! registered sink, and the host routes them wherever it logs.
//!
//! # Upstream dependencies (what this file consumes)
//! - std::sync only (RwLock, atomics). No other crate modules.
//!
//! # Downstream consumers (who depends on this file)
//! - src/scan.rs — emits scan/refresh lifecycle events
//! - src/ffi.rs — `ds_set_log_callback` installs a sink that forwards to
//!   the host's C callback
//! - tests/ffi.rs — asserts events arrive through the FFI registration
//!
//! # Algorithm & invariants
//! - **Zero cost when unregistered**: the emission fast path is a single
//!   Acquire load of an AtomicBool; callers gate their `format!` on
//!   [`enabled`] so no formatting allocation happens with no sink.
//! - **Bounded volume**: emitters log summaries and lifecycle edges only,
//!   never per-file lines — a scan of any size produces O(1) events (plus
//!   O(refreshes)). This is a contract, not an accident; new emit sites
//!   must preserve it.
//! - The sink may be called from any worker thread concurrently; the
//!   installer's sink closure must be thread-safe (enforced by the
//!   `Send + Sync` bound).
//! - Levels are a tiny fixed ladder shared with the C ABI: 0 debug,
//!   1 info, 2 warn.
//!
//! ============================================================================

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;

/// Verbose diagnostics (off by default in hosts).
pub const DEBUG: u8 = 0;
/// Lifecycle events: scan start/done, refresh done.
pub const INFO: u8 = 1;
/// Degraded-but-working conditions: node ceiling hit, refresh fallback.
pub const WARN: u8 = 2;

type Sink = Box<dyn Fn(u8, &str) + Send + Sync>;

/// Fast-path latch mirroring `SINK.is_some()`, so emission costs one
/// atomic load when no host is listening.
static ENABLED: AtomicBool = AtomicBool::new(false);
static SINK: RwLock<Option<Sink>> = RwLock::new(None);

/// Install (Some) or remove (None) the global sink. Replaces any previous
/// sink; events emitted concurrently with the swap go to whichever sink
/// the emitting thread observes — both are valid targets during the
/// transition, and after `set_sink(None)` returns no further calls to the
/// old sink begin.
pub fn set_sink(sink: Option<Sink>) {
    let mut guard = SINK.write().unwrap();
    ENABLED.store(sink.is_some(), Ordering::Release);
    *guard = sink;
}

/// True when a sink is installed. Emit sites check this before building
/// their message so the no-listener path allocates nothing.
#[inline]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

/// Deliver one event to the sink, if any. `msg` should be a single line.
pub fn emit(level: u8, msg: &str) {
    if !enabled() {
        return;
    }
    if let Some(sink) = SINK.read().unwrap().as_ref() {
        sink(level, msg);
    }
}
