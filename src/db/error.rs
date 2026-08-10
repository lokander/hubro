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
    /// The *server* called this failure retryable — SQLSTATE `40001`,
    /// serialization failure (FRE-147). Not hubro's judgement: this variant is
    /// only ever built from a code the server sent.
    ///
    /// It is a distinct variant rather than a flag on the others because the
    /// only thing anyone does with it is decide whether to run the operation
    /// again, and that decision belongs to the caller: the catalog reads in
    /// [`DbPool`](super::DbPool) retry once, every other path reports it like
    /// any other failure.
    Transient(String),
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
            | DbError::Unsupported(m)
            | DbError::Transient(m) => m,
        }
    }

    /// Whether the server said this failure is worth attempting again.
    pub fn is_transient(&self) -> bool {
        matches!(self, DbError::Transient(_))
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
            DbError::Transient(m) => write!(f, "temporary failure, try again: {m}"),
        }
    }
}

impl std::error::Error for DbError {}
