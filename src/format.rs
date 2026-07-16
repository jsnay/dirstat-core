//! ============================================================================
//! FILE: src/format.rs
//!
//! ============================================================================
//!
//! # Purpose
//! Byte formatting helpers (CORE-UNIT-1). The engine always returns raw
//! byte counts across FFI (formatting/localization is a host concern);
//! these functions are exact-at-boundary conveniences for tests and
//! non-GUI hosts (a future CLI). "Exact at boundary" is the one behavior
//! the spec pins down: 1024 bytes formats as "1 KiB", never "1.0 KiB",
//! and 1023 stays "1023 B" — no premature rounding across a unit edge.
//!
//! # Upstream dependencies (what this file consumes)
//! - (none — pure string formatting over u64; no other crate modules,
//!   no std facilities beyond `format!`)
//!
//! # Downstream consumers (who depends on this file)
//! - src/lib.rs — exposes the module (`dirstat_core::format`)
//! - the module's own unit tests (EVC-UNIT-1) — currently the only
//!   in-repo caller; the Swift app formats bytes itself from raw counts
//!
//! # Structure
//! - `format_binary` — 1 KiB = 1024 units (B..EiB)
//! - `format_decimal` — 1 KB = 1000 units (B..EB)
//! - `format_with` — the shared engine both wrap
//! - tests — boundary exactness and fractional rendering
//!
//! # Algorithm & invariants
//! - Unit selection uses integer division (`bytes / divisor >= base`) so
//!   the choice is exact — no float comparison can misplace a boundary.
//! - Whole multiples of the chosen divisor print with no decimals
//!   (verified with the integer `bytes % divisor == 0` check, not just
//!   float closeness); everything else prints one decimal place.
//! - u64::MAX is ~16 EiB, so the EiB/EB entry is reachable and the unit
//!   arrays can never be overrun (`unit < units.len() - 1` guard).
//!
//! ============================================================================

/// Binary units, 1 KiB = 1024.
pub fn format_binary(bytes: u64) -> String {
    format_with(
        bytes,
        1024,
        &["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"],
    )
}

/// Decimal units, 1 KB = 1000.
pub fn format_decimal(bytes: u64) -> String {
    format_with(bytes, 1000, &["B", "KB", "MB", "GB", "TB", "PB", "EB"])
}

/// Shared formatter: pick the largest unit with a value >= 1, then print
/// either an exact integer ("3 MiB") or one decimal ("1.5 KiB").
fn format_with(bytes: u64, base: u64, units: &[&str]) -> String {
    if bytes < base {
        // Sub-unit values are always exact integers ("1023 B").
        return format!("{} {}", bytes, units[0]);
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    let mut divisor = 1u64;
    // Integer-division walk up the unit ladder: exact at boundaries where
    // a float loop could drift (e.g. exactly 1024 must reach KiB).
    while bytes / divisor >= base && unit < units.len() - 1 {
        divisor *= base;
        unit += 1;
        value = bytes as f64 / divisor as f64;
    }
    // Print without decimals only when the value is truly whole — the
    // integer modulo check is authoritative; the float check merely
    // filters fast.
    if (value - value.round()).abs() < f64::EPSILON && bytes % divisor == 0 {
        format!("{} {}", value.round() as u64, units[unit])
    } else {
        format!("{:.1} {}", value, units[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// EVC-UNIT-1: exact at the conversion boundaries.
    #[test]
    fn exact_boundaries() {
        assert_eq!(format_binary(1023), "1023 B");
        assert_eq!(format_binary(1024), "1 KiB");
        assert_eq!(format_binary(1024 * 1024), "1 MiB");
        assert_eq!(format_binary(1024 * 1024 * 1024), "1 GiB");
        assert_eq!(format_decimal(999), "999 B");
        assert_eq!(format_decimal(1000), "1 KB");
        assert_eq!(format_decimal(1_000_000), "1 MB");
        assert_eq!(format_decimal(1_000_000_000), "1 GB");
    }

    /// Non-whole multiples render with exactly one decimal place.
    #[test]
    fn fractional() {
        assert_eq!(format_binary(1536), "1.5 KiB");
        assert_eq!(format_decimal(1500), "1.5 KB");
    }
}
