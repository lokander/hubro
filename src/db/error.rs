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
}

impl DbError {
    pub fn message(&self) -> &str {
        match self {
            DbError::Connect(m) | DbError::Query(m) | DbError::Introspect(m) => m,
        }
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DbError::Connect(m) => write!(f, "connection failed: {m}"),
            DbError::Query(m) => write!(f, "query failed: {m}"),
            DbError::Introspect(m) => write!(f, "schema introspection failed: {m}"),
        }
    }
}

impl std::error::Error for DbError {}
