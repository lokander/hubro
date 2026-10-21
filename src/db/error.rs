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
    /// serialization failure. Not hubro's judgement: this variant is
    /// only ever built from a code the server sent.
    ///
    /// It is a distinct variant rather than a flag on the others because the
    /// only thing anyone does with it is decide whether to run the operation
    /// again, and that decision belongs to the caller: the catalog reads in
    /// [`DbPool`](super::DbPool) retry once, every other path reports it like
    /// any other failure.
    Transient(String),
    /// The server answered the login with its wrong-credentials code: SQL
    /// Server error 18456, PostgreSQL SQLSTATE `28P01`. Only ever built from
    /// that code, never from message text, for the same reason as
    /// [`DbError::Transient`].
    ///
    /// This is the one connect failure that may cost a stored secret: the
    /// connect flow reads it as "the remembered password has gone stale",
    /// deletes that password from the keyring and asks for a new one. So
    /// every other code the server can refuse a login with stays a
    /// [`DbError::Connect`], including failures whose text mentions a failed
    /// login.
    ///
    /// The code is the server's word, not proof that the password was wrong.
    /// Measured cases where it is sent for a password that was right: a SQL
    /// Server *contained-database* user (the usual Azure SQL Database setup)
    /// naming a database that does not exist, since the user lives in that
    /// database and cannot be found (an *offline* one is 942, and kept); a
    /// login denied `CONNECT SQL`; a PostgreSQL role whose `VALID UNTIL` has
    /// passed, which `auth_failed` reports as a wrong password. SQL
    /// authentication switched off on the server, and a pooler such as
    /// pgbouncer refusing on the server's behalf, are expected to look the
    /// same but were not measured. These are known residuals of deciding on
    /// the wire, where they cannot be told apart from a wrong password. SQL Server's error 4060 ("Cannot open database … requested by
    /// the login. The login failed.") is the case that made this a variant:
    /// it is sent for a database that does not exist, is offline or
    /// restoring, or that this login may not open — measured, all four — and
    /// only *after* the password was accepted. A wrong password together with
    /// any of those is 18456. Deleting the password there destroys a working
    /// credential and then asks for one that cannot help.
    ///
    /// Displays exactly as [`DbError::Connect`] does: what the user reads is
    /// not the decision, and the message carries the classifier's own words.
    CredentialsRejected(String),
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
            | DbError::Transient(m)
            | DbError::CredentialsRejected(m) => m,
        }
    }

    /// Whether the server said this failure is worth attempting again.
    pub fn is_transient(&self) -> bool {
        matches!(self, DbError::Transient(_))
    }

    /// Whether the server refused the credentials themselves (see
    /// [`DbError::CredentialsRejected`]) — the only failure on which a
    /// stored password may be treated as stale.
    pub fn credentials_rejected(&self) -> bool {
        matches!(self, DbError::CredentialsRejected(_))
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DbError::Connect(m) | DbError::CredentialsRejected(m) => {
                write!(f, "connection failed: {m}")
            }
            DbError::Query(m) => write!(f, "query failed: {m}"),
            DbError::Introspect(m) => write!(f, "schema introspection failed: {m}"),
            DbError::RowCountMismatch(m) => write!(f, "write aborted: {m}"),
            DbError::Unsupported(m) => write!(f, "not supported here: {m}"),
            DbError::Transient(m) => write!(f, "temporary failure, try again: {m}"),
        }
    }
}

impl std::error::Error for DbError {}
