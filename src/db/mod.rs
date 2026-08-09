//! Backend-agnostic database access layer.
//!
//! The UI talks to [`ConnectionRegistry`] / [`DbPool`] and renders the
//! backend-neutral models in [`value`] and [`schema`]; only this module knows
//! about sqlx and concrete drivers. Async flow in components: run the
//! `DbPool` futures inside `use_resource`/spawned tasks and store the
//! resulting values in signals — never hold a signal borrow across an await.
//!
//! Result contract, identical on every backend (FRE-138): a *user-facing*
//! query that returns no rows still returns its columns, so an empty `SELECT`
//! shows its headers instead of a blank pane. SQL Server gets them from TDS
//! metadata; the sqlx backends have no row to read them off and describe the
//! statement instead (see [`sqlx_common::fill_headers`]). A statement with no
//! result set at all — a `DROP TABLE` routed through the query path —
//! legitimately reports no columns.
//!
//! "User-facing" is the whole scope of that describe: [`DbPool::query`] and
//! [`DbPool::query_capped`], the two entry points that hand a result straight
//! to the grid. The internal reads (page fetch, cell fetch, DDL catalog
//! queries) build their own projection and so already know their columns, so
//! paying a round trip to recover headers nobody reads would be pure cost —
//! double on Postgres, whose `describe` also issues a `pg_attribute` query.

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
mod sql;
mod sqlite;
mod sqlserver;
mod sqlx_common;
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
    classify_column, ColumnClass, Filter, FilterOp, Page, PageRequest, PreviewInfo, SortDir,
    PREVIEW_BYTES,
};
pub use postgres::{
    build_url, normalize_pg_url, url_target, url_via_local_port, url_with_password,
};
pub use registry::{
    CellFetch, Connection, ConnectionId, ConnectionRegistry, DbPool, FETCH_CELL_MAX_BYTES,
    MAX_QUERY_ROWS, QUERY_CELL_CAP,
};
pub use rowkey::{detect_row_identity, RowIdentity};
pub use schema::{
    ColumnMeta, ForeignKeyMeta, Generated, IndexMeta, Internal, TableKind, TableMeta, TypeDetail,
    TypeRef,
};
pub use script::{
    classify_statement, needs_confirmation, run_script, script_refusal, split_statements,
    statement_needs, statement_preview, ScriptError, StatementKind, StatementNeeds,
    StatementOutcome, StatementResult,
};
pub use sql::Dialect;
pub use sqlite::open_sqlite;
pub use sqlserver::{
    build_mssql_url, mssql_url_target, mssql_url_via_local_port, mssql_url_with_password,
    normalize_mssql_url, open_mssql, open_mssql_with, MssqlAuth, MssqlPool, MssqlTx,
};
pub use staged::{
    apply_staged, AppliedCounts, CheckedStatement, RowLocator, StagedChange, StagedError,
};
pub use value::{ColumnInfo, QueryResult, Value};
