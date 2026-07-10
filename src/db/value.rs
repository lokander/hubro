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
}
