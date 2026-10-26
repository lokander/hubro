//! Small helpers shared across the app: display formatting, and the one
//! rule every search box matches by.

/// Whether `text` contains `query`, case-insensitively — the rule every
/// search box in the app matches by, so two lists never disagree on the same
/// input.
///
/// Folds with full Unicode lowercase on both sides, not ASCII: `Årsrapport`
/// has to be found by `årsrapp`. That is why this runs in Rust rather than
/// as SQLite `LIKE`, whose case folding stops at ASCII. An empty (or
/// whitespace-only) query matches everything, so an unfiltered list renders
/// through the same path.
pub fn search_matches(text: &str, query: &str) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return true;
    }
    text.to_lowercase().contains(&query.to_lowercase())
}

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

/// Formats a count with thousands grouping, e.g. row counts in the schema pane.
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

/// `byte` as two lowercase hex digits, as ASCII bytes — for a writer that
/// takes bytes ([`push_hex`] is the one that takes a `String`).
pub fn hex_digits(byte: u8) -> [u8; 2] {
    const DIGITS: [u8; 16] = *b"0123456789abcdef";
    [DIGITS[(byte >> 4) as usize], DIGITS[(byte & 0x0f) as usize]]
}

/// Appends `bytes` to `out` as lowercase hex, two digits a byte — the one
/// hex spelling every blob rendering shares (export, the clipboard, SQL
/// literals, the staged-edit key, the hex viewer).
///
/// By hand rather than through `write!(out, "{byte:02x}")`, which runs the
/// formatting machinery per byte; the hex viewer formats up to 16k of them
/// for one cell.
pub fn push_hex(out: &mut String, bytes: &[u8]) {
    out.reserve(bytes.len() * 2);
    for &byte in bytes {
        let [high, low] = hex_digits(byte);
        out.push(high as char);
        out.push(low as char);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn hex_is_two_lowercase_digits_a_byte() {
        let mut out = String::from("x");
        super::push_hex(&mut out, &[0x00, 0x0f, 0xa0, 0xff, 0x5c]);
        assert_eq!(out, "x000fa0ff5c");
        // The same spelling `{:02x}` gives, for every byte.
        for byte in 0..=255u8 {
            let mut one = String::new();
            super::push_hex(&mut one, &[byte]);
            assert_eq!(one, format!("{byte:02x}"));
            assert_eq!(super::hex_digits(byte), *format!("{byte:02x}").as_bytes());
        }
    }

    use super::*;

    #[test]
    fn search_folds_case_beyond_ascii() {
        assert!(search_matches("Årsrapport", "årsrapp"));
        assert!(search_matches("årsrapport", "ÅRSRAPP"));
        assert!(search_matches("SELECT 1", "select"));
        assert!(!search_matches("Årsrapport", "staging"));
        assert!(search_matches("anything", "  "));
    }

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
