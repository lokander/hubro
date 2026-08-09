use std::fmt;

/// Error from any database operation, in a form the UI can display and store
/// in signals (hence `Clone + PartialEq`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbError {
    /// Opening or validating a connection failed.
    Connect(String),
    /// Executing a query failed.
    Query(String),
    /// Reading schema metadata failed.
    Introspect(String),
    /// A guarded write affected an unexpected number of rows and was rolled
    /// back (see [`DbPool::execute_checked`](super::DbPool::execute_checked)).
    RowCountMismatch(String),
    /// The operation isn't available on this connection or object — the
    /// capability it needs isn't declared (see
    /// [`Capabilities`](super::caps::Capabilities)). The UI gates these paths
    /// up front, so reaching one means a gate was missed; the message is the
    /// same sentence the disabled affordance would have shown.
    Unsupported(String),
}

impl DbError {
    /// The guarded-write failure every backend raises when a statement in a
    /// checked batch matched an unexpected number of rows. One constructor so
    /// the three `execute_all_checked` implementations cannot drift in a
    /// message the UI shows verbatim (and the tests match on).
    pub(super) fn row_count_mismatch(affected: u64, expected: u64) -> DbError {
        DbError::RowCountMismatch(format!(
            "statement affected {affected} rows, expected {expected} — rolled back"
        ))
    }

    pub fn message(&self) -> &str {
        match self {
            DbError::Connect(m)
            | DbError::Query(m)
            | DbError::Introspect(m)
            | DbError::RowCountMismatch(m)
            | DbError::Unsupported(m) => m,
        }
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DbError::Connect(m) => write!(f, "connection failed: {m}"),
            DbError::Query(m) => write!(f, "query failed: {m}"),
            DbError::Introspect(m) => write!(f, "schema introspection failed: {m}"),
            DbError::RowCountMismatch(m) => write!(f, "write aborted: {m}"),
            DbError::Unsupported(m) => write!(f, "not supported here: {m}"),
        }
    }
}

impl std::error::Error for DbError {}
