//! Small formatting helpers shared across the UI.

/// Formats a byte count for display, e.g. blob sizes in the data grid.
///
/// Uses 1024-based units with one decimal ("1.5 KB"); exact byte counts
/// below 1 KB ("512 B").
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    // {:.1} rounds, which can carry the value to 1024.0 — bump the unit so
    // 1048575 renders as "1.0 MB", not "1024.0 KB"
    if (value * 10.0).round() >= 10240.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_below_one_kb_are_exact() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1), "1 B");
        assert_eq!(human_bytes(1023), "1023 B");
    }

    #[test]
    fn scales_through_units() {
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(human_bytes(1024 * 1024 - 1), "1.0 MB");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.0 GB");
    }

    #[test]
    fn values_beyond_tb_stay_in_tb() {
        assert_eq!(human_bytes(u64::MAX), "16777216.0 TB");
    }
}
