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
//! "User-facing" is the whole scope of that describe: [`DbPool::query`],
//! [`DbPool::query_capped`] and [`ScriptTx::query_capped`] — the entry points
//! that hand a result straight to the grid. The internal reads (page fetch,
//! cell fetch, DDL catalog queries) build their own projection and so already
//! know their columns, so paying a round trip to recover headers nobody reads
//! would be pure cost —
//! double on Postgres, whose `describe` also issues a `pg_attribute` query.

mod caps;
mod clipboard;
mod coerce;
mod ddl;
mod error;
mod export;
mod fk;
mod import;
mod page;
mod plan;
mod postgres;
mod registry;
mod rowkey;
mod schema;
mod schema_edit;
mod script;
mod sql;
mod sqlite;
mod sqlserver;
mod sqlx_common;
mod staged;
mod stats;
mod url;
mod value;

pub use caps::{
    unreadable_reason, Capabilities, Restriction, TableAccess, WriteProtection,
    CONNECTION_READ_ONLY, MARKED_READ_ONLY, NO_DDL, NO_GUARDED_WRITE, NO_MUTATE, NO_OFFSET_PAGING,
    NO_QUERY, UNGUARDED_WRITES, USER_READ_ONLY,
};
pub use clipboard::{raw_cell_text, render_copy, CopyBlock, CopyFormat};
pub use coerce::{bool_checked, bool_value, classify_type, TypeClass};
// The cell editor validates numbers with the import's rules rather than its
// own copy of them (FRE-112); nothing outside the crate needs this.
pub(crate) use coerce::{is_bit_string, parse_numeric_text, validate_bit_literal};
pub use ddl::{Ddl, DdlObject, DdlSource};
pub use error::DbError;
pub use export::{write_result, ExportFormat};
pub use fk::{build_fk_filter, resolve_referenced_column};
pub use import::{
    default_mapping, import_refusal, is_importable, mapping_from_header, open_source, preview,
    run_import, sniff_dialect, sniff_encoding, sniff_file, sniff_shape, ColumnBinding, CsvDialect,
    CsvReader, EmptyField, Encoding, ErrorMode, FileSniff, ImportError, ImportOptions,
    ImportReport, JsonReader, JsonShape, ReadError, Record, RecordSource, SkippedRow, SourceField,
    SourceFormat, SourcePreview, SourceValue, MAX_REPORTED_SKIPS,
};
pub use page::{
    classify_column, ColumnClass, Filter, FilterOp, Page, PageRequest, PreviewInfo, SortDir,
    PREVIEW_BYTES,
};
pub use plan::{
    explain_statement, ExplainSupport, PlanDisplay, PlanNode, PlanTree, EXPENSIVE_SHARE, NO_EXPLAIN,
};
pub use postgres::{
    build_url, normalize_pg_url, url_target, url_via_local_port, url_with_password, PgFlavor,
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
pub use schema_edit::{
    op_problem, schema_edit_refusal, schema_op_sql, SchemaOp, NOTHING_TO_RUN, NOT_A_TABLE,
};
pub use script::{
    classify_statement, needs_confirmation, run_script, script_refusal, split_statements,
    statement_needs, statement_preview, Rollback, ScriptError, StatementKind, StatementNeeds,
    StatementOutcome, StatementResult,
};
pub use sql::Dialect;
pub use sqlite::{check_sqlite_file, open_sqlite, SqliteFileError, SqliteFileErrorKind};
pub use sqlserver::{
    build_mssql_url, mssql_url_target, mssql_url_via_local_port, mssql_url_with_password,
    normalize_mssql_url, open_mssql, open_mssql_with, MssqlAuth, MssqlPool, MssqlTx,
};
pub use staged::{
    apply_staged, AppliedCounts, CheckedStatement, RowLocator, StagedChange, StagedError,
};
pub use stats::{RowCount, TableStats};
pub use value::{ColumnInfo, QueryResult, Value};
