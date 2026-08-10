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

/// Formats a count with thousands grouping, e.g. row counts in the schema pane
/// (FRE-118).
///
/// Grouped because the number's *magnitude* is the message — "is this table
/// thousands of rows or millions?" is unanswerable at a glance from
/// `12345678`. The separator is U+2009 THIN SPACE rather than a comma or a
/// period: each of those is a decimal point somewhere, and the app has no
/// locale with which to choose between them.
pub fn human_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() * 2);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('\u{2009}');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_group_from_the_right_and_never_lead_with_a_separator() {
        assert_eq!(human_count(0), "0");
        assert_eq!(human_count(999), "999");
        assert_eq!(human_count(1000), "1\u{2009}000");
        assert_eq!(human_count(12_345_678), "12\u{2009}345\u{2009}678");
        // A length that is an exact multiple of three is where a naive
        // implementation emits a leading separator.
        assert_eq!(human_count(100), "100");
        assert_eq!(human_count(123_456), "123\u{2009}456");
    }

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
