//! Backend-agnostic database access layer.
//!
//! The UI talks to [`ConnectionRegistry`] / [`DbPool`] and renders the
//! backend-neutral models in [`value`] and [`schema`]; only this module knows
//! about sqlx and concrete drivers. Async flow in components: run the
//! `DbPool` futures inside `use_resource`/spawned tasks and store the
//! resulting values in signals — never hold a signal borrow across an await.

mod error;
mod export;
mod fk;
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
pub use export::{write_result, ExportFormat};
pub use fk::{build_fk_filter, resolve_referenced_column};
pub use page::{
    classify_column, ColumnClass, Dialect, Filter, FilterOp, Page, PageRequest, PreviewInfo,
    SortDir, PREVIEW_BYTES,
};
pub use postgres::{
    build_url, normalize_pg_url, url_target, url_via_local_port, url_with_password,
};
pub use registry::{
    CellFetch, Connection, ConnectionId, ConnectionRegistry, DbPool, FETCH_CELL_MAX_BYTES,
    MAX_QUERY_ROWS, QUERY_CELL_CAP,
};
pub use rowkey::{delete_sql, detect_row_identity, update_sql, RowIdentity};
pub use schema::{ColumnMeta, ForeignKeyMeta, Generated, IndexMeta, TableKind, TableMeta};
pub use script::{
    classify_statement, needs_confirmation, run_script, split_statements, statement_preview,
    ScriptError, StatementKind, StatementOutcome, StatementResult,
};
pub use sqlite::open_sqlite;
pub use staged::{
    apply_staged, AppliedCounts, CheckedStatement, RowLocator, StagedChange, StagedError,
};
pub use value::{ColumnInfo, QueryResult, Value};
