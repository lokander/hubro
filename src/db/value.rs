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
}
