//! Byte formatting helpers (CORE-UNIT-1). The engine always returns raw
//! byte counts across FFI; these are exact-at-boundary conveniences used by
//! tests and non-GUI hosts.

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

fn format_with(bytes: u64, base: u64, units: &[&str]) -> String {
    if bytes < base {
        return format!("{} {}", bytes, units[0]);
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    let mut divisor = 1u64;
    while bytes / divisor >= base && unit < units.len() - 1 {
        divisor *= base;
        unit += 1;
        value = bytes as f64 / divisor as f64;
    }
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

    #[test]
    fn fractional() {
        assert_eq!(format_binary(1536), "1.5 KiB");
        assert_eq!(format_decimal(1500), "1.5 KB");
    }
}
