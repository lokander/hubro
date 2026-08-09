use crate::util::human_bytes;

/// A single cell value, independent of the backend that produced it.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Short human-readable form for grid cells. Blobs render as a size tag
    /// rather than raw bytes; NULL is spelled out so it can be styled apart
    /// from the empty string.
    pub fn display(&self) -> String {
        match self {
            Value::Null => "NULL".to_string(),
            Value::Integer(i) => i.to_string(),
            Value::Real(r) => r.to_string(),
            Value::Text(t) => t.clone(),
            Value::Blob(b) => format!("<blob {}>", human_bytes(b.len() as u64)),
        }
    }
}

/// Truncates a decoded cell to at most `cap` bytes so a streamed result
/// (the free-form query path, [`super::DbPool::query_capped`]) can bound the
/// memory each retained cell costs even for a `SELECT *` returning huge
/// values. Text is cut on a char boundary; blobs are cut to `cap` bytes;
/// everything else is returned untouched. This is a display-only truncation
/// on a read-only result — nothing staged or exported round-trips through it.
pub(crate) fn cap_value(value: Value, cap: usize) -> Value {
    match value {
        Value::Text(t) if t.len() > cap => {
            let mut end = cap;
            while end > 0 && !t.is_char_boundary(end) {
                end -= 1;
            }
            Value::Text(t[..end].to_string())
        }
        Value::Blob(b) if b.len() > cap => Value::Blob(b[..cap].to_vec()),
        other => other,
    }
}

/// One decoded cell as text, `None` for SQL NULL (and for a column index the
/// row doesn't have).
///
/// These four accessors read a decoded row positionally, which is how every
/// backend consumes the catalog queries it runs through its own query path:
/// the SQL fixes the column order, so an index is the natural key and a
/// missing/NULL cell must degrade rather than error — a catalog column that
/// unexpectedly reads NULL should cost one caveat, not the whole DDL. They
/// live here because they operate on [`Value`], not on any driver's row type.
pub(crate) fn row_opt_text(row: &[Value], idx: usize) -> Option<String> {
    match row.get(idx) {
        Some(Value::Null) | None => None,
        Some(Value::Text(t)) => Some(t.clone()),
        Some(other) => Some(other.display()),
    }
}

/// [`row_opt_text`] with NULL flattened to the empty string.
pub(crate) fn row_text(row: &[Value], idx: usize) -> String {
    row_opt_text(row, idx).unwrap_or_default()
}

/// One decoded cell as an integer, `None` when it is NULL or not an integer.
pub(crate) fn row_opt_int(row: &[Value], idx: usize) -> Option<i64> {
    match row.get(idx) {
        Some(Value::Integer(n)) => Some(*n),
        _ => None,
    }
}

/// [`row_opt_int`] with NULL flattened to zero.
pub(crate) fn row_int(row: &[Value], idx: usize) -> i64 {
    row_opt_int(row, idx).unwrap_or(0)
}

/// One decoded cell as a boolean. Boolean catalog columns (SQL Server's `bit`)
/// decode as Integer 0/1.
pub(crate) fn row_flag(row: &[Value], idx: usize) -> bool {
    row_int(row, idx) != 0
}

/// Trims trailing zeros from a chrono-formatted fractional second: `%.f`
/// pads to 3/6/9 digits ("09.500"), both Postgres and the SQL Server display
/// print minimal ("09.5"). The input must end with the seconds field; the
/// fraction dot is the only dot.
pub(crate) fn trim_fraction(mut s: String) -> String {
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s
}

/// A result column as reported by the driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
}

/// Rows and columns from an arbitrary query, ready for the data grid.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct QueryResult {
    pub columns: Vec<ColumnInfo>,
    pub rows: Vec<Vec<Value>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_displays_as_marker() {
        assert_eq!(Value::Null.display(), "NULL");
        assert!(Value::Null.is_null());
    }

    #[test]
    fn scalars_display_plainly() {
        assert_eq!(Value::Integer(-42).display(), "-42");
        assert_eq!(Value::Real(1.5).display(), "1.5");
        assert_eq!(Value::Text("hi".into()).display(), "hi");
    }

    #[test]
    fn blobs_display_as_size_tag() {
        assert_eq!(Value::Blob(vec![0; 2048]).display(), "<blob 2.0 KB>");
        assert!(!Value::Blob(vec![]).is_null());
    }

    #[test]
    fn cap_value_truncates_text_and_blobs_but_not_scalars() {
        // Under the cap: untouched.
        assert_eq!(
            cap_value(Value::Text("hi".into()), 8),
            Value::Text("hi".into())
        );
        assert_eq!(
            cap_value(Value::Blob(vec![1, 2]), 8),
            Value::Blob(vec![1, 2])
        );
        // Over the cap: trimmed.
        assert_eq!(
            cap_value(Value::Text("abcdef".into()), 3),
            Value::Text("abc".into())
        );
        assert_eq!(
            cap_value(Value::Blob(vec![9; 100]), 4),
            Value::Blob(vec![9; 4])
        );
        // Scalars are never touched.
        assert_eq!(cap_value(Value::Integer(123456), 2), Value::Integer(123456));
        assert_eq!(cap_value(Value::Null, 0), Value::Null);
    }

    #[test]
    fn cap_value_cuts_text_on_a_char_boundary() {
        // "héllo": cutting at byte 2 would split 'é' (2 bytes); back off to 1.
        let capped = cap_value(Value::Text("héllo".into()), 2);
        assert_eq!(capped, Value::Text("h".into()));
    }

    #[test]
    fn row_accessors_degrade_on_null_and_missing_cells() {
        let row = [
            Value::Text("hi".into()),
            Value::Null,
            Value::Integer(0),
            Value::Integer(7),
        ];
        assert_eq!(row_opt_text(&row, 0), Some("hi".to_string()));
        assert_eq!(row_text(&row, 0), "hi");
        // Non-text cells render through `display`, so a numeric catalog column
        // read as text still says something useful.
        assert_eq!(row_text(&row, 3), "7");
        // NULL and past-the-end are the same "nothing there" for every arm.
        for idx in [1, 99] {
            assert_eq!(row_opt_text(&row, idx), None);
            assert_eq!(row_text(&row, idx), "");
            assert_eq!(row_opt_int(&row, idx), None);
            assert_eq!(row_int(&row, idx), 0);
            assert!(!row_flag(&row, idx));
        }
        assert_eq!(row_opt_int(&row, 2), Some(0));
        assert!(!row_flag(&row, 2));
        assert!(row_flag(&row, 3));
    }

    #[test]
    fn trim_fraction_strips_padding_zeros() {
        assert_eq!(trim_fraction("12:34:56.500".into()), "12:34:56.5");
        assert_eq!(trim_fraction("12:34:56.000".into()), "12:34:56");
        assert_eq!(trim_fraction("12:34:56".into()), "12:34:56");
    }
}
