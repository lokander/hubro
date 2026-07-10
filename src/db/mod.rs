//! Backend-agnostic database access layer.
//!
//! The UI talks to [`ConnectionRegistry`] / [`DbPool`] and renders the
//! backend-neutral models in [`value`] and [`schema`]; only this module knows
//! about sqlx and concrete drivers. Async flow in components: run the
//! `DbPool` futures inside `use_resource`/spawned tasks and store the
//! resulting values in signals — never hold a signal borrow across an await.

mod error;
mod page;
mod postgres;
mod registry;
mod rowkey;
mod schema;
mod script;
mod sqlite;
mod staged;
mod value;

pub use error::DbError;
pub use page::{Dialect, Filter, FilterOp, PageRequest, SortDir};
pub use postgres::{build_url, sanitized_url, url_target, url_via_local_port, url_with_password};
pub use registry::{Connection, ConnectionId, ConnectionRegistry, DbPool};
pub use rowkey::{delete_sql, detect_row_identity, update_sql, RowIdentity};
pub use schema::{ColumnMeta, ForeignKeyMeta, IndexMeta, TableKind, TableMeta};
pub use script::{
    classify_statement, needs_confirmation, run_script, split_statements, statement_preview,
    ScriptError, StatementKind, StatementOutcome, StatementResult,
};
pub use sqlite::open_sqlite;
pub use staged::{
    apply_staged, AppliedCounts, CheckedStatement, RowLocator, StagedChange, StagedError,
};
pub use value::{ColumnInfo, QueryResult, Value};
