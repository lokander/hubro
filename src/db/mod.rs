//! Backend-agnostic database access layer.
//!
//! The UI talks to [`ConnectionRegistry`] / [`DbPool`] and renders the
//! backend-neutral models in [`value`] and [`schema`]; only this module knows
//! about sqlx and concrete drivers. Async flow in components: run the
//! `DbPool` futures inside `use_resource`/spawned tasks and store the
//! resulting values in signals — never hold a signal borrow across an await.

mod caps;
mod clipboard;
mod ddl;
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
mod sqlserver;
mod staged;
mod url;
mod value;

pub use caps::{
    Capabilities, Restriction, TableAccess, WriteProtection, CONNECTION_READ_ONLY,
    MARKED_READ_ONLY, NO_DDL, NO_MUTATE, NO_OFFSET_PAGING, NO_QUERY, USER_READ_ONLY,
};
pub use clipboard::{raw_cell_text, render_copy, CopyBlock, CopyFormat};
pub use ddl::{Ddl, DdlObject, DdlSource};
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
pub use schema::{
    ColumnMeta, ForeignKeyMeta, Generated, IndexMeta, Internal, TableKind, TableMeta, TypeDetail,
    TypeRef,
};
pub use script::{
    classify_statement, needs_confirmation, run_script, script_refusal, split_statements,
    statement_needs, statement_preview, ScriptError, StatementKind, StatementNeeds,
    StatementOutcome, StatementResult,
};
pub use sqlite::open_sqlite;
pub use sqlserver::{
    build_mssql_url, mssql_url_target, mssql_url_via_local_port, mssql_url_with_password,
    normalize_mssql_url, open_mssql, open_mssql_with, MssqlAuth, MssqlPool, MssqlTx,
};
pub use staged::{
    apply_staged, AppliedCounts, CheckedStatement, RowLocator, StagedChange, StagedError,
};
pub use value::{ColumnInfo, QueryResult, Value};
