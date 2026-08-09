//! Postgres backend: connecting, query execution (buffered, capped/streaming,
//! and export paths), script transactions, DDL retrieval, and full
//! multi-schema introspection — every user schema's tables and views with
//! columns, primary keys, indexes, and foreign keys, with extension
//! bookkeeping and child partitions marked internal (FRE-88).

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::io::Write;

use sqlx::postgres::types::{PgInterval, PgTimeTz};
use sqlx::postgres::{
    PgDatabaseError, PgErrorPosition, PgHasArrayType, PgPool, PgPoolOptions, PgRow, PgTypeKind,
    PgValueFormat, PgValueRef,
};
use sqlx::types::chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use sqlx::types::{Decimal, JsonValue, Uuid};
use sqlx::{Row as _, TypeInfo as _, ValueRef as _};

use super::ddl::{
    create_table_sql, create_view_sql, terminate, ColumnExtra, Ddl, DdlObject, TableExtras,
};
use super::error::DbError;
use super::export::ExportFormat;
use super::schema::{
    ColumnMeta, ForeignKeyMeta, Generated, IndexMeta, Internal, TableKind, TableMeta, TypeDetail,
    TypeRef,
};
use super::sql::Dialect;
use super::sqlx_common::{self, get};
use super::staged::CheckedStatement;
use super::value::{row_opt_text, row_text, trim_fraction, QueryResult, Value};

/// Splices a password into a Postgres URL — see [`super::url::with_password`].
pub fn url_with_password(url: &str, password: &str) -> Result<String, DbError> {
    super::url::with_password(url, password)
}

/// Canonicalizes a Postgres URL into the stable saved-connection locator —
/// see [`super::url::UrlScheme::normalize`] (`postgresql://` → `postgres://`,
/// default port 5432).
pub fn normalize_pg_url(url: &str) -> Result<String, DbError> {
    super::url::POSTGRES.normalize(url)
}

/// The host and port a Postgres URL points at (default port 5432) — see
/// [`super::url::UrlScheme::target`].
pub fn url_target(url: &str) -> Result<(String, u16), DbError> {
    super::url::POSTGRES.target(url)
}

/// Rewrites a URL to connect through a forwarded local port — see
/// [`super::url::via_local_port`].
pub fn url_via_local_port(url: &str, port: u16) -> Result<String, DbError> {
    super::url::via_local_port(url, port)
}

/// Builds a password-free URL from the individual connection-form fields —
/// see [`super::url::UrlScheme::build`] (TLS param `sslmode`).
pub fn build_url(
    host: &str,
    port: &str,
    database: &str,
    user: &str,
    sslmode: &str,
) -> Result<String, DbError> {
    super::url::POSTGRES.build(host, port, database, user, sslmode)
}

/// Connects to Postgres from a URL (`postgres://user@host:port/db?sslmode=…`).
/// The URL may carry a password; saved config never does — callers splice a
/// session password in via [`url_with_password`].
pub async fn open_postgres(url: &str) -> Result<PgPool, DbError> {
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(url)
        .await
        .map_err(|e| DbError::Connect(friendly_connect_error(&e)))?;
    sqlx::query("SELECT 1")
        .fetch_one(&pool)
        .await
        .map_err(|e| DbError::Connect(friendly_connect_error(&e)))?;
    Ok(pool)
}

/// Categorizes common failure modes so the connections screen reads well:
/// auth, network/DNS, TLS, wrong-server.
fn friendly_connect_error(err: &sqlx::Error) -> String {
    let msg = err.to_string();
    let lower = msg.to_lowercase();
    if lower.contains("password authentication failed") {
        format!("authentication failed — {msg}")
    } else if lower.contains("role") && lower.contains("does not exist") {
        format!("unknown role — {msg}")
    } else if lower.contains("unexpected response from sslrequest") {
        // Something answered the Postgres handshake with garbage — usually a
        // different database server (e.g. SQL Server) on that host/port.
        format!("the server doesn't appear to be Postgres — check the host and port — {msg}")
    } else if lower.contains("unsupportedcertversion") {
        // rustls refuses to parse an X.509 v1 certificate, so the connection
        // dies before any `sslmode` policy gets a say — including `prefer`,
        // which reads as "encrypt if you can" but cannot fall back once the
        // TLS handshake has begun. libpq connects to such servers happily, so
        // without this the failure looks like hubro being broken rather than
        // the certificate being two decades out of date (FRE-89).
        //
        // Not exotic: the official `citusdata/citus` image generates exactly
        // such a certificate on first boot. Verified that a *valid* v3
        // self-signed certificate connects fine at both `prefer` and
        // `require`, so this is specifically about the version, not about
        // hubro verifying more strictly than libpq does.
        format!(
            "the server's TLS certificate is X.509 v1, which modern TLS libraries reject — \
             reissue it as v3, or use sslmode=disable if the network is trusted — {msg}"
        )
    } else if lower.contains("tls") || lower.contains("ssl") {
        format!("TLS error — {msg}")
    } else if lower.contains("connection refused")
        || lower.contains("timed out")
        || lower.contains("failed to lookup")
    {
        format!("network error — {msg}")
    } else {
        msg
    }
}

type PgQuery<'q> = sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>;

/// Binds backend-neutral [`Value`] parameters onto a prepared query.
///
/// NULL caveat: `Value::Null` binds as `None::<String>`, i.e. a NULL of
/// type text. That is fine for the current uses (filter values are text),
/// but Postgres rejects a text NULL for non-text columns in e.g.
/// `SET col = $n`. Cell editing solves this at the SQL level: the staged
/// SQL builders (`staged::ParamSql::value_sql`) render NULL values inline
/// as the literal `NULL` instead of binding them, so a NULL never reaches
/// this function from an edit.
fn bind_params<'q>(mut query: PgQuery<'q>, params: &[Value]) -> PgQuery<'q> {
    for param in params {
        query = match param {
            Value::Null => query.bind(None::<String>),
            Value::Integer(i) => query.bind(*i),
            Value::Real(r) => query.bind(*r),
            Value::Text(t) => query.bind(t.clone()),
            Value::Blob(b) => query.bind(b.clone()),
        };
    }
    query
}

/// Runs an arbitrary query, decoding every cell into the backend-neutral
/// [`Value`] model. Free-form SQL whose result is shown to the user, so a
/// zero-row result recovers its headers (FRE-138).
pub async fn query(pool: &PgPool, sql: &str) -> Result<QueryResult, DbError> {
    let mut result = query_with(pool, sql, &[]).await?;
    sqlx_common::fill_headers(&mut result, pool, sql).await;
    Ok(result)
}

/// Like [`query`], with bound parameters.
///
/// No header recovery on a zero-row result here — see
/// [`super::sqlite::query_with`] for why the projection-built callers skip it.
/// It costs more on this backend: sqlx's Postgres `describe` issues a second
/// `pg_attribute` query whose result would be discarded with the rest.
pub async fn query_with(
    pool: &PgPool,
    sql: &str,
    params: &[Value],
) -> Result<QueryResult, DbError> {
    let stream = bind_params(sqlx::query(sql), params).fetch(pool);
    sqlx_common::collect_all(stream, decode_value, |e| query_error(e, sql)).await
}

/// Streams `sql` one row at a time (`fetch`, not `fetch_all`), decoding and
/// retaining at most `max_rows` rows and capping each cell to `cell_cap`
/// bytes, so the free-form query path never scales with table or value size
/// (FRE-33). Returns the (bounded) result and whether more rows existed
/// beyond the cap.
pub async fn query_capped(
    pool: &PgPool,
    sql: &str,
    params: &[Value],
    max_rows: u64,
    cell_cap: usize,
) -> Result<(QueryResult, bool), DbError> {
    let stream = bind_params(sqlx::query(sql), params).fetch(pool);
    let (mut result, truncated) =
        sqlx_common::collect_capped(stream, max_rows, cell_cap, decode_value, |e| {
            query_error(e, sql)
        })
        .await?;
    sqlx_common::fill_headers(&mut result, pool, sql).await;
    Ok((result, truncated))
}

/// [`query_capped`] against a single connection (e.g. one borrowed from a
/// transaction) rather than the pool — the read path for statements inside an
/// atomically-wrapped script (FRE-38). No bound params: scripts are raw text.
pub async fn query_capped_conn(
    conn: &mut sqlx::postgres::PgConnection,
    sql: &str,
    max_rows: u64,
    cell_cap: usize,
) -> Result<(QueryResult, bool), DbError> {
    let stream = sqlx::query(sql).fetch(&mut *conn);
    let (mut result, truncated) =
        sqlx_common::collect_capped(stream, max_rows, cell_cap, decode_value, |e| {
            query_error(e, sql)
        })
        .await?;
    sqlx_common::fill_headers(&mut result, &mut *conn, sql).await;
    Ok((result, truncated))
}

/// Runs a non-row statement on a single connection (e.g. one borrowed from a
/// transaction) rather than the pool, returning affected rows — the write
/// path for statements inside an atomically-wrapped script (FRE-38).
pub async fn execute_conn(
    conn: &mut sqlx::postgres::PgConnection,
    sql: &str,
) -> Result<u64, DbError> {
    sqlx_common::execute(conn, sql, |e| query_error(e, sql)).await
}

/// Streams a query to `out` in the given format, pulling rows one at a time
/// (`fetch`, not `fetch_all`) and writing each incrementally — peak memory is
/// one decoded row plus the writer's buffer. Returns the number of data rows
/// written. When the result is empty the column names come from a statement
/// describe so the header (CSV) / empty array (JSON) still reflect the query.
pub async fn export(
    pool: &PgPool,
    sql: &str,
    params: &[Value],
    format: ExportFormat,
    out: &mut impl Write,
) -> Result<u64, DbError> {
    let stream = bind_params(sqlx::query(sql), params).fetch(pool);
    if let Some(rows) =
        sqlx_common::export_stream(stream, format, out, decode_value, |e| query_error(e, sql))
            .await?
    {
        return Ok(rows);
    }
    // No rows streamed, so no row carried the column names: describe the
    // statement instead. This one propagates its failure — an export whose
    // header cannot be determined is an empty file that says nothing.
    let columns = sqlx_common::describe_columns(pool, sql)
        .await
        .map_err(|e| query_error(e, sql))?;
    sqlx_common::export_empty(format, columns, out)?;
    Ok(0)
}

/// Executes a statement without decoding rows, returning the driver's
/// affected-row count.
pub async fn execute(pool: &PgPool, sql: &str) -> Result<u64, DbError> {
    sqlx_common::execute(pool, sql, |e| query_error(e, sql)).await
}

/// Executes parameterized writes inside ONE transaction, committing only
/// when every statement affected exactly its `expected_rows` rows. Any SQL
/// error or count mismatch rolls the whole batch back; the error carries the
/// index of the failing statement (`None` for begin/commit failures, which
/// belong to no statement). This is the safety net for row edits: a WHERE
/// clause that unexpectedly matches more (or fewer) rows than the one being
/// edited must never commit — and with staged edits (FRE-14), neither may
/// any sibling change in the same batch.
pub async fn execute_all_checked(
    pool: &PgPool,
    statements: &[CheckedStatement],
) -> Result<(), (Option<usize>, DbError)> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| (None, DbError::Query(e.to_string())))?;
    for (index, statement) in statements.iter().enumerate() {
        let result = bind_params(sqlx::query(&statement.sql), &statement.params)
            .execute(&mut *tx)
            .await;
        // Dropping the transaction would roll back too; do it explicitly and
        // ignore secondary errors — the original failure is what the caller
        // needs.
        let done = match result {
            Ok(done) => done,
            Err(e) => {
                let _ = tx.rollback().await;
                return Err((Some(index), query_error(e, &statement.sql)));
            }
        };
        let affected = done.rows_affected();
        if affected != statement.expected_rows {
            let _ = tx.rollback().await;
            return Err((
                Some(index),
                DbError::row_count_mismatch(affected, statement.expected_rows),
            ));
        }
    }
    tx.commit()
        .await
        .map_err(|e| (None, DbError::Query(e.to_string())))
}

/// Builds a query error, appending "line L, column C" when the server
/// reported an error cursor. Postgres sends the position as a 1-based
/// *character* index into the query text; positions into internally
/// generated queries ([`PgErrorPosition::Internal`]) don't map to the user's
/// text and are ignored.
fn query_error(err: sqlx::Error, sql: &str) -> DbError {
    let mut message = err.to_string();
    if let sqlx::Error::Database(db_err) = &err {
        if let Some(pg) = db_err.try_downcast_ref::<PgDatabaseError>() {
            if let Some(PgErrorPosition::Original(position)) = pg.position() {
                if let Some((line, column)) = line_col(sql, position) {
                    message.push_str(&format!(" (line {line}, column {column})"));
                }
            }
        }
    }
    DbError::Query(message)
}

/// Maps a 1-based character position into 1-based line and column numbers.
/// Returns `None` for positions outside the text (except one-past-the-end,
/// which the server reports e.g. for input that stops too early).
fn line_col(sql: &str, position: usize) -> Option<(usize, usize)> {
    if position == 0 {
        return None;
    }
    let mut line = 1usize;
    let mut column = 1usize;
    let mut seen = 0usize;
    for c in sql.chars() {
        seen += 1;
        if seen == position {
            return Some((line, column));
        }
        if c == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (position == seen + 1).then_some((line, column))
}

/// Full multi-schema introspection: every user schema's tables and views
/// with columns, primary keys, indexes (incl. unique), and foreign keys —
/// parity with the SQLite metadata model. Six batched queries regardless
/// of table count.
pub async fn introspect(pool: &PgPool) -> Result<Vec<TableMeta>, DbError> {
    let map_err = |e: sqlx::Error| DbError::Introspect(e.to_string());

    // Objects that are the database's own bookkeeping rather than the user's
    // data (FRE-88), from the three sources Postgres has. All three are
    // catalog facts, never name patterns — a user table genuinely called
    // `spatial_ref_sys` is the user's.
    //
    //  1. Schemas an extension created. `pg_depend` records the namespace →
    //     extension dependency, so this catches TimescaleDB's seven schemas,
    //     PostGIS's `tiger`/`topology` and Citus's catalogs by construction.
    //     A schema the *user* made has no such dependency even when an
    //     extension puts objects in it — which is the distinction that
    //     matters, since `public` holds PostGIS's functions and is still the
    //     user's schema.
    //  2. Individual objects an extension installs into an ordinary schema:
    //     PostGIS's `spatial_ref_sys`, `pg_stat_statements`'s view. Same
    //     catalog, keyed on `pg_class` instead of `pg_namespace`.
    //  3. Child partitions of declaratively partitioned tables. Not an
    //     extension matter at all, but a table partitioned by day floods the
    //     tree exactly as Timescale's chunks do.
    //
    // `deptype = 'e'` on the first two: that is the code for "member of the
    // extension". The neighbouring `'x'` means the opposite — the object is
    // *not* a member, it merely gets dropped with the extension — so counting
    // it would attribute the user's own object to an extension.
    let internal_rows = sqlx::query(
        "SELECT n.nspname AS schema_name, NULL::name AS object_name, e.extname \
         FROM pg_namespace n \
         JOIN pg_depend d ON d.classid = 'pg_namespace'::regclass AND d.objid = n.oid \
          AND d.deptype = 'e' \
         JOIN pg_extension e ON e.oid = d.refobjid \
          AND d.refclassid = 'pg_extension'::regclass \
         UNION ALL \
         SELECT n.nspname, c.relname, e.extname \
         FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_depend d ON d.classid = 'pg_class'::regclass AND d.objid = c.oid \
          AND d.deptype = 'e' \
         JOIN pg_extension e ON e.oid = d.refobjid \
          AND d.refclassid = 'pg_extension'::regclass \
         UNION ALL \
         SELECT n.nspname, c.relname, NULL \
         FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relispartition",
    )
    .fetch_all(pool)
    .await
    .map_err(map_err)?;
    // Schema-wide entries and per-object entries are kept apart so an object
    // rule can win over its schema's — the object is the more specific fact.
    let mut internal_schemas: HashMap<String, Internal> = HashMap::new();
    let mut internal_objects: HashMap<(String, String), Internal> = HashMap::new();
    for row in &internal_rows {
        let schema: String = get(row, "schema_name")?;
        let object: Option<String> = get(row, "object_name")?;
        let extension: Option<String> = get(row, "extname")?;
        let reason = match extension {
            Some(extension) => Internal::Extension(extension),
            None => Internal::Partition,
        };
        match object {
            // An object can qualify twice — a partition an extension owns
            // hits branches 2 and 3 — and the UNION has no defined row order,
            // so name the winner rather than letting it be whichever row
            // arrived last. Naming the extension is the more useful of the
            // two, since "which extension" is the part the user can act on.
            Some(object) => match internal_objects.entry((schema, object)) {
                Entry::Occupied(mut slot) => {
                    if matches!(reason, Internal::Extension(_)) {
                        slot.insert(reason);
                    }
                }
                Entry::Vacant(slot) => {
                    slot.insert(reason);
                }
            },
            None => {
                internal_schemas.insert(schema, reason);
            }
        }
    }

    // Resolved before anything below mutates the maps these read.
    let has_extension = |name: &str| {
        internal_schemas
            .values()
            .chain(internal_objects.values())
            .any(|reason| matches!(reason, Internal::Extension(ext) if ext == name))
    };
    let (has_citus, has_timescale) = (has_extension("citus"), has_extension("timescaledb"));

    // Citus shard tables (FRE-89). Citus normally keeps these out of
    // `pg_class` for client queries itself, so introspection never sees them
    // and this finds nothing — but that hiding is a *setting*
    // (`citus.show_shards_for_app_name_prefixes`), and with it widened a
    // 26-table database reports 290, of which 264 are shards. They are not
    // extension members and not partitions, so nothing else here catches
    // them.
    //
    // `pg_dist_shard` is the catalog Citus itself uses, and `shard_name()` is
    // Citus's own function for naming a row of it — asking beats reproducing
    // the rule, which is not the plain `<table>_<shardid>` it looks like:
    // Citus hashes and truncates once that would exceed NAMEDATALEN, so a
    // 63-character table shards into `..._01ae35b2_102008`.
    //
    // Its answer is then resolved back to a real `pg_class` row rather than
    // used as a name, because `shard_name()` qualifies anything outside
    // `public` — a shard in a `sales` schema comes back as
    // `sales.orders_102008`, which matches no `relname` anywhere. Resolving
    // it yields the schema and the bare name the caller needs, and means no
    // string is ever parsed.
    //
    // Two things that resolution has to be pinned against:
    //
    // `to_regclass` rather than a `::regclass` cast, because the cast *raises*
    // on a name it cannot resolve, and a multi-node coordinator's
    // `pg_dist_shard` lists shards living on workers with no local relation.
    // The cast would error, the `if let Ok` below would swallow it, and
    // nothing at all would be marked.
    //
    // `sc.relnamespace = c.relnamespace`, because `to_regclass` resolves
    // through the search path: a user table named `decoy.pub_102008` with
    // `decoy` ahead of `public` otherwise resolves *instead of* the real
    // shard, and the user's own table is what gets hidden while the shard
    // stays visible — backwards on both counts, and not gated behind the
    // visibility setting, since name resolution goes through the catalog
    // cache rather than the `pg_class` scan Citus filters. Shards always live
    // in their table's schema, so pinning the namespace costs nothing.
    //
    // Residual, and benign: a `search_path` that excludes `public` leaves
    // `public` shards unresolvable, so they go unmarked — the same place they
    // would be if Citus were hiding them, which by default it is.
    // Best-effort for the same reason as the Timescale labels below.
    if has_citus {
        let shards = sqlx::query(
            "SELECT sn.nspname AS schema_name, sc.relname AS object_name \
             FROM pg_dist_shard s \
             JOIN pg_class c ON c.oid = s.logicalrelid \
             JOIN pg_class sc \
               ON sc.oid = pg_catalog.to_regclass( \
                    pg_catalog.shard_name(s.logicalrelid, s.shardid)) \
              AND sc.relnamespace = c.relnamespace \
             JOIN pg_namespace sn ON sn.oid = sc.relnamespace",
        )
        .fetch_all(pool)
        .await;
        if let Ok(rows) = shards {
            for row in &rows {
                let schema: String = get(row, "schema_name")?;
                let object: String = get(row, "object_name")?;
                // Same precedence as the branches above: naming the extension
                // beats naming the shape, so a shard that is also a partition
                // reports the extension rather than disagreeing with them.
                internal_objects.insert((schema, object), Internal::Extension("citus".to_string()));
            }
        }
    }

    // Timescale's own vocabulary for objects that are otherwise ordinary
    // tables and views (FRE-88), so a hypertable reads as one instead of
    // looking like a table that mysteriously has chunks. Best-effort: these
    // information views are stable across Timescale 2.x, but a badge is not
    // worth failing a whole introspection over if a future version moves
    // them, and the extension is absent on nearly every Postgres database.
    let mut kind_labels: HashMap<(String, String), String> = HashMap::new();
    if has_timescale {
        let labelled = [
            (
                "SELECT hypertable_schema AS s, hypertable_name AS n \
                 FROM timescaledb_information.hypertables",
                "hypertable",
            ),
            (
                "SELECT view_schema AS s, view_name AS n \
                 FROM timescaledb_information.continuous_aggregates",
                "continuous aggregate",
            ),
        ];
        for (sql, label) in labelled {
            let Ok(rows) = sqlx::query(sql).fetch_all(pool).await else {
                continue;
            };
            for row in &rows {
                let schema: String = get(row, "s")?;
                let name: String = get(row, "n")?;
                kind_labels.insert((schema, name), label.to_string());
            }
        }
    }

    // Tables and views across all non-system schemas. Materialized views
    // (relkind 'm') are not in information_schema, so they come from a
    // pg_catalog UNION (FRE-41).
    let table_rows = sqlx::query(
        "SELECT table_schema, table_name, table_type \
         FROM information_schema.tables \
         WHERE table_schema NOT IN ('pg_catalog', 'information_schema') \
         UNION ALL \
         SELECT n.nspname, c.relname, 'MATERIALIZED VIEW' \
         FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind = 'm' \
           AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
         ORDER BY table_schema, table_name",
    )
    .fetch_all(pool)
    .await
    .map_err(map_err)?;

    // Columns with PK positions resolved in SQL (LEFT JOIN on the pkey
    // constraint), one row per column. Identity and generated columns have a
    // NULL column_default even though the database supplies their value, so
    // `is_identity`/`identity_generation`/`is_generated` are surfaced
    // separately and mapped into `ColumnMeta.generated` (FRE-25 required-
    // column detection and read-only gating).
    // Materialized-view columns aren't in information_schema.columns either, so
    // they come from pg_catalog (pg_attribute) via a UNION (FRE-41). Matviews
    // have no PK, identity, or generated columns, so those are constant here;
    // `format_type` yields the type name (with modifiers, e.g.
    // `character varying(255)`). `ord` orders columns within a relation across
    // both halves of the UNION.
    let column_rows = sqlx::query(
        "SELECT c.table_schema, c.table_name, c.column_name, c.data_type, \
                c.is_nullable, c.column_default, \
                c.is_identity, c.identity_generation, c.is_generated, \
                pk.ordinal_position AS pk_position, \
                c.ordinal_position AS ord, \
                ut.typtype::text AS typtype, ut.typcategory::text AS typcategory, \
                ut.oid::int8 AS type_oid, \
                c.udt_schema AS type_schema, c.udt_name AS type_base \
         FROM information_schema.columns c \
         LEFT JOIN pg_namespace un ON un.nspname = c.udt_schema \
         LEFT JOIN pg_type ut ON ut.typname = c.udt_name AND ut.typnamespace = un.oid \
         LEFT JOIN ( \
             SELECT kcu.table_schema, kcu.table_name, kcu.column_name, kcu.ordinal_position \
             FROM information_schema.table_constraints tc \
             JOIN information_schema.key_column_usage kcu \
               ON kcu.constraint_name = tc.constraint_name \
              AND kcu.constraint_schema = tc.constraint_schema \
              AND kcu.table_schema = tc.table_schema \
              AND kcu.table_name = tc.table_name \
             WHERE tc.constraint_type = 'PRIMARY KEY' \
         ) pk ON pk.table_schema = c.table_schema \
             AND pk.table_name = c.table_name \
             AND pk.column_name = c.column_name \
         WHERE c.table_schema NOT IN ('pg_catalog', 'information_schema') \
         UNION ALL \
         SELECT n.nspname, c.relname, a.attname, \
                format_type(a.atttypid, a.atttypmod), \
                CASE WHEN a.attnotnull THEN 'NO' ELSE 'YES' END, \
                NULL::text, 'NO', NULL::text, 'NEVER', \
                NULL::int, a.attnum::int, \
                t.typtype::text, t.typcategory::text, t.oid::int8, \
                tn.nspname, t.typname \
         FROM pg_attribute a \
         JOIN pg_class c ON c.oid = a.attrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_type t ON t.oid = a.atttypid \
         JOIN pg_namespace tn ON tn.oid = t.typnamespace \
         WHERE c.relkind = 'm' AND a.attnum > 0 AND NOT a.attisdropped \
           AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
         ORDER BY table_schema, table_name, ord",
    )
    .fetch_all(pool)
    .await
    .map_err(map_err)?;

    // Enum variants for every enum type in the database, keyed by type OID
    // (FRE-71). One query rather than per-column: enum types are few and a
    // type is typically shared by several columns. `enumsortorder` is
    // declaration order, which is the order the editor's dropdown offers.
    let enum_rows = sqlx::query(
        "SELECT enumtypid::int8 AS type_oid, enumlabel::text AS label \
         FROM pg_enum ORDER BY enumtypid, enumsortorder",
    )
    .fetch_all(pool)
    .await
    .map_err(map_err)?;
    let mut enum_variants: HashMap<i64, Vec<String>> = HashMap::new();
    for row in &enum_rows {
        let type_oid: i64 = get(row, "type_oid")?;
        let label: String = get(row, "label")?;
        enum_variants.entry(type_oid).or_default().push(label);
    }

    // Indexes from pg_catalog (information_schema has no index view).
    // Expression-index entries have a 0 attnum and no attribute row; those
    // key positions surface as NULL column names. Partial indexes
    // (indpred) are flagged so row-identity detection can reject them;
    // invalid indexes (e.g. from a failed CREATE INDEX CONCURRENTLY) make
    // no guarantees at all and are dropped entirely.
    let index_rows = sqlx::query(
        "SELECT n.nspname AS table_schema, t.relname AS table_name, \
                i.relname AS index_name, ix.indisunique AS is_unique, \
                ix.indpred IS NOT NULL AS is_partial, \
                k.ord AS key_position, a.attname AS column_name \
         FROM pg_index ix \
         JOIN pg_class t ON t.oid = ix.indrelid \
         JOIN pg_class i ON i.oid = ix.indexrelid \
         JOIN pg_namespace n ON n.oid = t.relnamespace \
         CROSS JOIN LATERAL unnest(ix.indkey) WITH ORDINALITY AS k(attnum, ord) \
         LEFT JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = k.attnum \
         WHERE n.nspname NOT IN ('pg_catalog', 'information_schema') \
           AND n.nspname NOT LIKE 'pg\\_%' \
           AND k.ord <= ix.indnkeyatts \
           AND ix.indisvalid \
         ORDER BY n.nspname, t.relname, i.relname, k.ord",
    )
    .fetch_all(pool)
    .await
    .map_err(map_err)?;

    // Foreign keys from pg_constraint; conkey/confkey arrays preserve the
    // multi-column ordering that information_schema loses.
    let fk_rows = sqlx::query(
        "SELECT n.nspname AS table_schema, t.relname AS table_name, \
                rn.nspname AS ref_schema, rt.relname AS ref_table, \
                (SELECT array_agg(a.attname ORDER BY x.ord) \
                 FROM unnest(c.conkey) WITH ORDINALITY AS x(attnum, ord) \
                 JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = x.attnum \
                ) AS columns, \
                (SELECT array_agg(a.attname ORDER BY x.ord) \
                 FROM unnest(c.confkey) WITH ORDINALITY AS x(attnum, ord) \
                 JOIN pg_attribute a ON a.attrelid = c.confrelid AND a.attnum = x.attnum \
                ) AS ref_columns \
         FROM pg_constraint c \
         JOIN pg_class t ON t.oid = c.conrelid \
         JOIN pg_namespace n ON n.oid = t.relnamespace \
         JOIN pg_class rt ON rt.oid = c.confrelid \
         JOIN pg_namespace rn ON rn.oid = rt.relnamespace \
         WHERE c.contype = 'f' \
           AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
           AND n.nspname NOT LIKE 'pg\\_%' \
         ORDER BY n.nspname, t.relname, c.conname",
    )
    .fetch_all(pool)
    .await
    .map_err(map_err)?;

    // Group the batched rows per (schema, table).
    let mut tables: Vec<TableMeta> = Vec::with_capacity(table_rows.len());
    for row in &table_rows {
        let table_type: String = get(row, "table_type")?;
        let schema: String = get(row, "table_schema")?;
        let name: String = get(row, "table_name")?;
        let key = (schema.clone(), name.clone());
        // The object's own rule first, then its schema's — most specific wins.
        let internal = internal_objects
            .get(&key)
            .or_else(|| internal_schemas.get(&schema))
            .cloned();
        let kind_label = kind_labels.get(&key).cloned();
        tables.push(TableMeta {
            schema: Some(schema),
            name,
            kind: match table_type.as_str() {
                "VIEW" => TableKind::View,
                "MATERIALIZED VIEW" => TableKind::MaterializedView,
                _ => TableKind::Table,
            },
            columns: Vec::new(),
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            // No per-object narrowing: kind and row identity already carry
            // everything this backend knows about writability (FRE-87).
            restriction: None,
            internal,
            kind_label,
        });
    }
    // (schema, table) → index into `tables`, built once so grouping the
    // column/index/FK rows below is a hash lookup per row instead of a linear
    // scan over every table (FRE-133).
    let mut table_index: HashMap<(String, String), usize> = HashMap::with_capacity(tables.len());
    for (idx, table) in tables.iter().enumerate() {
        if let Some(schema) = &table.schema {
            table_index
                .entry((schema.clone(), table.name.clone()))
                .or_insert(idx);
        }
    }

    for row in &column_rows {
        let schema: String = get(row, "table_schema")?;
        let table: String = get(row, "table_name")?;
        let Some(&idx) = table_index.get(&(schema, table)) else {
            continue;
        };
        let nullable: String = get(row, "is_nullable")?;
        let pk_position: Option<i32> = get(row, "pk_position")?;
        // Both `GENERATED ALWAYS AS IDENTITY` and `GENERATED ALWAYS AS
        // (expr) STORED` are database-assigned and not writable, so both map
        // to `Generated::Always`; `GENERATED BY DEFAULT AS IDENTITY` is
        // overridable. `serial` is not generated — it carries a real
        // `nextval(…)` default and stays `Never`.
        let is_identity: String = get(row, "is_identity")?;
        let identity_generation: Option<String> = get(row, "identity_generation")?;
        let is_generated: String = get(row, "is_generated")?;
        let generated = if is_generated == "ALWAYS"
            || (is_identity == "YES" && identity_generation.as_deref() == Some("ALWAYS"))
        {
            Generated::Always
        } else if is_identity == "YES" {
            Generated::ByDefault
        } else {
            Generated::Never
        };
        // `data_type` is opaque for these two ('USER-DEFINED', 'ARRAY'), so
        // the editor's structure comes from pg_type instead (FRE-71).
        let typtype: Option<String> = get(row, "typtype")?;
        let typcategory: Option<String> = get(row, "typcategory")?;
        let type_oid: Option<i64> = get(row, "type_oid")?;
        let type_schema: Option<String> = get(row, "type_schema")?;
        let type_base: Option<String> = get(row, "type_base")?;
        let type_ref = match (type_schema, type_base) {
            (Some(schema), Some(name)) => Some(TypeRef { schema, name }),
            _ => None,
        };
        let type_detail = match (typtype.as_deref(), typcategory.as_deref(), type_ref) {
            (Some("e"), _, Some(type_ref)) => type_oid
                .and_then(|oid| enum_variants.get(&oid))
                .map(|variants| TypeDetail::Enum {
                    type_ref,
                    variants: variants.clone(),
                })
                .unwrap_or_default(),
            (_, Some("A"), Some(type_ref)) => TypeDetail::Array { type_ref },
            _ => TypeDetail::Plain,
        };
        tables[idx].columns.push(ColumnMeta {
            name: get(row, "column_name")?,
            type_name: get(row, "data_type")?,
            nullable: nullable == "YES",
            primary_key_position: pk_position.map(|p| p as u32),
            default: get::<Option<String>, _>(row, "column_default")?,
            generated,
            type_detail,
        });
    }

    for row in &index_rows {
        let schema: String = get(row, "table_schema")?;
        let table: String = get(row, "table_name")?;
        let Some(&idx) = table_index.get(&(schema, table)) else {
            continue;
        };
        let index_name: String = get(row, "index_name")?;
        let column: Option<String> = get(row, "column_name")?;
        let column = column.unwrap_or_else(|| "<expr>".to_string());
        let unique: bool = get(row, "is_unique")?;
        let partial: bool = get(row, "is_partial")?;
        let indexes = &mut tables[idx].indexes;
        match indexes.last_mut() {
            Some(last) if last.name == index_name => last.columns.push(column),
            _ => indexes.push(IndexMeta {
                name: index_name,
                unique,
                partial,
                columns: vec![column],
            }),
        }
    }

    for row in &fk_rows {
        let schema: String = get(row, "table_schema")?;
        let table: String = get(row, "table_name")?;
        let Some(&idx) = table_index.get(&(schema, table)) else {
            continue;
        };
        let ref_columns: Vec<String> = get(row, "ref_columns")?;
        tables[idx].foreign_keys.push(ForeignKeyMeta {
            columns: get(row, "columns")?,
            referenced_schema: Some(get(row, "ref_schema")?),
            referenced_table: get(row, "ref_table")?,
            referenced_columns: ref_columns.into_iter().map(Some).collect(),
        });
    }

    Ok(tables)
}

/// DDL for one object (FRE-108).
///
/// Postgres renders view and index definitions itself (`pg_get_viewdef`,
/// `pg_get_indexdef`), and those come back verbatim. It has no `CREATE TABLE`
/// generator at all — `pg_dump` assembles that text in client code — so a
/// table is rebuilt from the catalog here.
///
/// The rebuild deliberately re-reads `pg_attribute`/`pg_attrdef`/`pg_constraint`
/// instead of leaning on the browsable [`TableMeta`]: introspection takes column
/// types from `information_schema.columns.data_type`, which reports `varchar(20)`
/// as a bare `character varying` and every user-defined type as `USER-DEFINED`.
/// Reconstructing from that would emit column types that are quietly *wrong*,
/// which is worse than not offering the feature. `pg_get_constraintdef`
/// likewise supplies check constraints and referential actions that the
/// browsable metadata does not carry at all, and `pg_get_expr(adbin, adrelid)`
/// supplies both the column defaults and the generation expressions — the same
/// call `information_schema.columns.column_default` is built from, read here so
/// one source covers both.
pub async fn fetch_ddl(
    pool: &PgPool,
    table: &TableMeta,
    object: &DdlObject,
) -> Result<Ddl, DbError> {
    // Postgres always qualifies; `public` is only a defensive default for a
    // TableMeta that somehow lost its schema.
    let schema = table.schema.clone().unwrap_or_else(|| "public".into());
    let params = [Value::Text(schema.clone())];

    if let DdlObject::Index(name) = object {
        let rows = query_with(
            pool,
            "SELECT pg_get_indexdef(c.oid) AS def \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname::text = $1 AND c.relname::text = $2 AND c.relkind IN ('i', 'I')",
            &[params[0].clone(), Value::Text(name.clone())],
        )
        .await?;
        let Some(def) = rows.rows.first().and_then(|r| row_opt_text(r, 0)) else {
            return Err(DbError::Introspect(format!(
                "no index named {name} in {schema}"
            )));
        };
        return Ok(Ddl::native(terminate(&def)));
    }

    let params = [params[0].clone(), Value::Text(table.name.clone())];
    if matches!(table.kind, TableKind::View | TableKind::MaterializedView) {
        let rows = query_with(
            pool,
            "SELECT pg_get_viewdef(c.oid, true) AS def \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname::text = $1 AND c.relname::text = $2",
            &params,
        )
        .await?;
        let Some(body) = rows.rows.first().and_then(|r| row_opt_text(r, 0)) else {
            return Err(DbError::Introspect(format!(
                "no view definition for {schema}.{}",
                table.name
            )));
        };
        // Only the CREATE header is ours; the body is the server's own text,
        // so this stays native (no "reconstructed" warning).
        return Ok(Ddl::native(create_view_sql(table, &body)));
    }

    // A catalog read that fails still yields usable output: the renderer falls
    // back to what TableMeta knows and the failure becomes a visible caveat,
    // rather than the whole action erroring out. The standing caveats are
    // *extended*, never replaced — the degraded output must not claim to
    // reproduce more than the good one.
    let extras = match table_ddl_extras(pool, &params).await {
        Ok(extras) => extras,
        Err(err) => {
            let mut caveats = pg_standing_caveats();
            caveats.push(format!(
                "column types, defaults, collations, constraints and indexes — reading the \
                 catalog failed ({})",
                err.message()
            ));
            TableExtras {
                caveats,
                ..TableExtras::default()
            }
        }
    };
    Ok(create_table_sql(Dialect::Postgres, table, &extras))
}

/// What a `pg_dump` of the same table carries and this rebuild does not,
/// regardless of how the catalog read went. Named rather than implied, so a
/// reader knows the boundary.
fn pg_standing_caveats() -> Vec<String> {
    vec![
        "sequences behind nextval() defaults".into(),
        "storage parameters, tablespace, partitioning and inheritance".into(),
        "triggers and row-level security policies".into(),
        "comments, ownership and privileges".into(),
    ]
}

/// Reads the per-table facts a faithful `CREATE TABLE` needs: exact column
/// types, defaults, identity/generated clauses, non-default collations, every
/// constraint as the server renders it, and the indexes that do not back a
/// constraint (those are already covered by their constraint).
async fn table_ddl_extras(pool: &PgPool, params: &[Value]) -> Result<TableExtras, DbError> {
    // A column's collation only matters when it differs from its type's
    // default; emitting the default one everywhere would be noise.
    let column_rows = query_with(
        pool,
        "SELECT a.attname::text AS name, \
                format_type(a.atttypid, a.atttypmod) AS type_name, \
                pg_get_expr(d.adbin, d.adrelid) AS default_expr, \
                a.attidentity::text AS identity, \
                a.attgenerated::text AS generated, \
                CASE WHEN a.attcollation <> t.typcollation THEN co.collname::text END AS collation \
         FROM pg_attribute a \
         JOIN pg_class c ON c.oid = a.attrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_type t ON t.oid = a.atttypid \
         LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
         LEFT JOIN pg_collation co ON co.oid = a.attcollation \
         WHERE n.nspname::text = $1 AND c.relname::text = $2 \
           AND a.attnum > 0 AND NOT a.attisdropped \
         ORDER BY a.attnum",
        params,
    )
    .await?;

    // Not-null constraints are also in pg_constraint from PG 17 (contype
    // 'n'); they are already rendered per column, so only the table-level
    // types are taken.
    let constraint_rows = query_with(
        pool,
        "SELECT con.conname::text AS name, pg_get_constraintdef(con.oid) AS def \
         FROM pg_constraint con \
         JOIN pg_class c ON c.oid = con.conrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname::text = $1 AND c.relname::text = $2 \
           AND con.contype IN ('p', 'u', 'f', 'c', 'x') \
         ORDER BY CASE con.contype \
                    WHEN 'p' THEN 0 WHEN 'u' THEN 1 WHEN 'f' THEN 2 WHEN 'c' THEN 3 ELSE 4 \
                  END, con.conname",
        params,
    )
    .await?;

    let index_rows = query_with(
        pool,
        "SELECT pg_get_indexdef(i.indexrelid) AS def \
         FROM pg_index i \
         JOIN pg_class c ON c.oid = i.indrelid \
         JOIN pg_class ic ON ic.oid = i.indexrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname::text = $1 AND c.relname::text = $2 AND i.indisvalid \
           AND NOT EXISTS ( \
             SELECT 1 FROM pg_constraint con WHERE con.conindid = i.indexrelid \
           ) \
         ORDER BY ic.relname",
        params,
    )
    .await?;

    let mut extras = TableExtras {
        caveats: pg_standing_caveats(),
        // The read succeeded, so an empty list here means "this table has no
        // table-level constraints", not "we could not tell".
        constraints: Some(Vec::new()),
        ..TableExtras::default()
    };
    for row in &column_rows.rows {
        let name = row_text(row, 0);
        let default_expr = row_opt_text(row, 2);
        // A generated column keeps its generation expression in pg_attrdef
        // too, so it must render as a generation clause and NOT as a DEFAULT.
        let (identity, generation_used) = match row_text(row, 3).as_str() {
            "a" => (Some("GENERATED ALWAYS AS IDENTITY".to_string()), false),
            "d" => (Some("GENERATED BY DEFAULT AS IDENTITY".to_string()), false),
            _ => match (row_text(row, 4).as_str(), &default_expr) {
                ("", _) => (None, false),
                (kind, Some(expr)) => {
                    // attgenerated: 's' = STORED, 'v' = VIRTUAL (PG 18, and
                    // the default for a bare GENERATED ALWAYS AS there).
                    // Guessing STORED for a virtual column would silently
                    // materialize it, so an unrecognized kind is written
                    // without a storage keyword and caveated instead.
                    let storage = match kind {
                        "s" => " STORED",
                        "v" => " VIRTUAL",
                        other => {
                            extras.caveats.push(format!(
                                "the storage kind of generated column {name} \
                                 (unrecognized pg_attribute.attgenerated {other:?})"
                            ));
                            ""
                        }
                    };
                    (Some(format!("GENERATED ALWAYS AS ({expr}){storage}")), true)
                }
                (_, None) => (None, false),
            },
        };
        extras.columns.insert(
            name,
            ColumnExtra {
                type_name: row_opt_text(row, 1),
                collation: row_opt_text(row, 5),
                // Postgres has no column form that replaces the type.
                computed: None,
                computed_persisted: false,
                identity,
                default: if generation_used { None } else { default_expr },
                // Postgres does not name defaults.
                default_constraint: None,
            },
        );
    }
    let constraints = extras.constraints.get_or_insert_with(Vec::new);
    for row in &constraint_rows.rows {
        constraints.push(format!(
            "CONSTRAINT {} {}",
            super::sql::quote_ident(&row_text(row, 0)),
            row_text(row, 1)
        ));
    }
    for row in &index_rows.rows {
        extras.indexes.push(row_text(row, 0));
    }
    Ok(extras)
}

/// Decodes scalar and rich Postgres types into the backend-neutral [`Value`]
/// model. Rich types (dates, numeric, uuid, json, intervals, arrays, enums)
/// render as `Value::Text` in a form close to what `psql` would print.
///
/// Cell data never errors the page: a type without a dedicated arm — or one
/// whose dedicated decode fails (NaN or >28-digit numeric, multidimensional
/// array, …) — degrades through [`decode_fallback`] to a text cast where
/// possible, then to a `<typename>` marker. The only `Err` left is an
/// out-of-range column index, which is a programming error.
fn decode_value(row: &PgRow, idx: usize) -> Result<Value, DbError> {
    let raw = row
        .try_get_raw(idx)
        .map_err(|e| DbError::Query(e.to_string()))?;
    if raw.is_null() {
        return Ok(Value::Null);
    }
    Ok(decode_typed(row, idx, &raw).unwrap_or_else(|| decode_fallback(row, idx, &raw)))
}

/// Type-specific decoding. Returns `None` both for types without a
/// dedicated arm and for values a dedicated arm cannot represent; the
/// caller degrades those via [`decode_fallback`].
fn decode_typed(row: &PgRow, idx: usize, raw: &PgValueRef) -> Option<Value> {
    match raw.type_info().name() {
        // Booleans render as text: "true"/"false" reads better in a viewer
        // than 0/1.
        "BOOL" => Some(Value::Text(row.try_get::<bool, _>(idx).ok()?.to_string())),
        "INT2" => Some(Value::Integer(row.try_get::<i16, _>(idx).ok()? as i64)),
        "INT4" => Some(Value::Integer(row.try_get::<i32, _>(idx).ok()? as i64)),
        "INT8" => Some(Value::Integer(row.try_get::<i64, _>(idx).ok()?)),
        "FLOAT4" => Some(Value::Real(row.try_get::<f32, _>(idx).ok()? as f64)),
        "FLOAT8" => Some(Value::Real(row.try_get::<f64, _>(idx).ok()?)),
        "TEXT" | "VARCHAR" | "BPCHAR" | "NAME" | "CHAR" => {
            Some(Value::Text(row.try_get::<String, _>(idx).ok()?))
        }
        "BYTEA" => Some(Value::Blob(row.try_get::<Vec<u8>, _>(idx).ok()?)),
        // Date/time family. `%.f` prints fractional seconds only when
        // non-zero; trailing zeros are trimmed to match Postgres output.
        // 'infinity'/'-infinity' must be caught from the wire bytes before
        // chrono: sqlx 0.8 panics (not errors) decoding them.
        "TIMESTAMP" => {
            if let Some(inf) = infinity_marker(raw) {
                return Some(Value::Text(inf.to_string()));
            }
            let ts = row.try_get::<NaiveDateTime, _>(idx).ok()?;
            Some(Value::Text(trim_fraction(
                ts.format("%Y-%m-%d %H:%M:%S%.f").to_string(),
            )))
        }
        "TIMESTAMPTZ" => {
            if let Some(inf) = infinity_marker(raw) {
                return Some(Value::Text(inf.to_string()));
            }
            // Postgres sends timestamptz as an instant; render in UTC with
            // an explicit offset so the timezone-awareness is visible.
            let ts = row.try_get::<DateTime<Utc>, _>(idx).ok()?;
            let local = trim_fraction(ts.format("%Y-%m-%d %H:%M:%S%.f").to_string());
            Some(Value::Text(format!("{local}+00:00")))
        }
        "DATE" => {
            if let Some(inf) = infinity_marker(raw) {
                return Some(Value::Text(inf.to_string()));
            }
            let d = row.try_get::<NaiveDate, _>(idx).ok()?;
            Some(Value::Text(d.format("%Y-%m-%d").to_string()))
        }
        "TIME" => {
            let t = row.try_get::<NaiveTime, _>(idx).ok()?;
            Some(Value::Text(trim_fraction(
                t.format("%H:%M:%S%.f").to_string(),
            )))
        }
        "TIMETZ" => {
            let t = row.try_get::<PgTimeTz, _>(idx).ok()?;
            let time = trim_fraction(t.time.format("%H:%M:%S%.f").to_string());
            Some(Value::Text(format!("{time}{}", t.offset)))
        }
        "INTERVAL" => {
            let iv = row.try_get::<PgInterval, _>(idx).ok()?;
            Some(Value::Text(format_interval(&iv)))
        }
        // Exact decimal string via rust_decimal — must not round-trip
        // through f64. NaN and >28 significant digits are not representable
        // and degrade to the marker.
        "NUMERIC" => Some(Value::Text(
            row.try_get::<Decimal, _>(idx).ok()?.to_string(),
        )),
        "UUID" => Some(Value::Text(row.try_get::<Uuid, _>(idx).ok()?.to_string())),
        // Compact JSON text (serde_json Display is compact).
        "JSON" | "JSONB" => Some(Value::Text(
            row.try_get::<JsonValue, _>(idx).ok()?.to_string(),
        )),
        // Arrays of the common element types render as a Postgres-style
        // literal. NULL elements render as NULL; text elements are not
        // quoted/escaped (this is a display form, not parseable syntax).
        // sqlx only decodes one-dimensional arrays; others degrade.
        "TEXT[]" | "VARCHAR[]" | "BPCHAR[]" | "NAME[]" => decode_array::<String>(row, idx, |v| v),
        "INT2[]" => decode_array::<i16>(row, idx, |v| v.to_string()),
        "INT4[]" => decode_array::<i32>(row, idx, |v| v.to_string()),
        "INT8[]" => decode_array::<i64>(row, idx, |v| v.to_string()),
        "FLOAT4[]" => decode_array::<f32>(row, idx, |v| v.to_string()),
        "FLOAT8[]" => decode_array::<f64>(row, idx, |v| v.to_string()),
        "BOOL[]" => decode_array::<bool>(row, idx, |v| v.to_string()),
        "UUID[]" => decode_array::<Uuid>(row, idx, |v| v.to_string()),
        "NUMERIC[]" => decode_array::<Decimal>(row, idx, |v| v.to_string()),
        _ => None,
    }
}

/// Graceful degradation for values [`decode_typed`] can't produce: enum
/// labels from the raw bytes, then a text cast, then a `<typename>` marker.
/// Infallible by design — one odd cell must not take down the page.
fn decode_fallback(row: &PgRow, idx: usize, raw: &PgValueRef) -> Value {
    // User-defined enums: the wire value (text or binary format) is the
    // label itself, but `try_get::<String>` refuses the unknown OID, so
    // read the raw bytes directly.
    if matches!(raw.type_info().kind(), PgTypeKind::Enum(_)) {
        if let Ok(label) = raw.as_str() {
            return Value::Text(label.to_string());
        }
    } else if let Ok(text) = row.try_get::<String, _>(idx) {
        return Value::Text(text);
    }
    Value::Text(format!("<{}>", raw.type_info().name().to_lowercase()))
}

/// Detects Postgres `infinity`/`-infinity` timestamp, timestamptz, and date
/// values from the wire bytes. Binary format encodes them as i64::MAX/MIN
/// (timestamp/timestamptz) or i32::MAX/MIN (date) big-endian; text format
/// spells them out. They must not reach chrono — sqlx 0.8 decodes via
/// `epoch + Duration`, whose overflow panics rather than erroring.
fn infinity_marker(raw: &PgValueRef) -> Option<&'static str> {
    let bytes = raw.as_bytes().ok()?;
    match raw.format() {
        PgValueFormat::Binary => {
            if bytes == i64::MAX.to_be_bytes() || bytes == i32::MAX.to_be_bytes() {
                Some("infinity")
            } else if bytes == i64::MIN.to_be_bytes() || bytes == i32::MIN.to_be_bytes() {
                Some("-infinity")
            } else if binary_datetime_exceeds_chrono(bytes) {
                // Postgres stores finite dates far beyond chrono's
                // ~year-262143 cap; sqlx's chrono decode would panic on the
                // overflow, so degrade those to a marker instead.
                Some("<out of chrono range>")
            } else {
                None
            }
        }
        // b"infinity" is coincidentally 8 bytes like a binary timestamp,
        // but the formats are branched on above so exact ASCII match here
        // is unambiguous.
        PgValueFormat::Text => match bytes {
            b"infinity" => Some("infinity"),
            b"-infinity" => Some("-infinity"),
            _ => None,
        },
    }
}

/// True when a binary timestamp (8-byte µs) or date (4-byte days) since the
/// 2000-01-01 Postgres epoch cannot be represented by chrono.
fn binary_datetime_exceeds_chrono(bytes: &[u8]) -> bool {
    let epoch_date = NaiveDate::from_ymd_opt(2000, 1, 1).expect("static date");
    match bytes.len() {
        8 => {
            let micros = i64::from_be_bytes(bytes.try_into().expect("length checked"));
            let epoch = epoch_date.and_hms_opt(0, 0, 0).expect("static time");
            // num_microseconds is None only if the span itself overflows
            // i64; treat that as "no bound" on the affected side.
            let max = NaiveDateTime::MAX
                .signed_duration_since(epoch)
                .num_microseconds()
                .unwrap_or(i64::MAX);
            let min = NaiveDateTime::MIN
                .signed_duration_since(epoch)
                .num_microseconds()
                .unwrap_or(i64::MIN);
            micros > max || micros < min
        }
        4 => {
            let days = i64::from(i32::from_be_bytes(
                bytes.try_into().expect("length checked"),
            ));
            let max = NaiveDate::MAX.signed_duration_since(epoch_date).num_days();
            let min = NaiveDate::MIN.signed_duration_since(epoch_date).num_days();
            days > max || days < min
        }
        _ => false,
    }
}

/// Decodes a one-dimensional array column and renders it as a
/// Postgres-style literal, e.g. `{a,b,NULL}`.
fn decode_array<T>(row: &PgRow, idx: usize, fmt: impl Fn(T) -> String) -> Option<Value>
where
    T: for<'a> sqlx::Decode<'a, sqlx::Postgres> + sqlx::Type<sqlx::Postgres> + PgHasArrayType,
{
    let items: Vec<Option<T>> = row.try_get(idx).ok()?;
    let mut parts = Vec::with_capacity(items.len());
    for item in items {
        parts.push(match item {
            Some(v) => fmt(v),
            None => "NULL".to_string(),
        });
    }
    Some(Value::Text(format!("{{{}}}", parts.join(","))))
}

/// Compact human form for an interval, e.g. "1 mon 2 days 03:04:05.5".
/// Zero parts are omitted; a zero interval renders as "00:00:00". Each
/// part carries its own sign, matching how Postgres prints mixed-sign
/// intervals.
fn format_interval(iv: &PgInterval) -> String {
    let mut parts: Vec<String> = Vec::new();
    if iv.months != 0 {
        let unit = if iv.months.abs() == 1 { "mon" } else { "mons" };
        parts.push(format!("{} {unit}", iv.months));
    }
    if iv.days != 0 {
        let unit = if iv.days.abs() == 1 { "day" } else { "days" };
        parts.push(format!("{} {unit}", iv.days));
    }
    if iv.microseconds != 0 || parts.is_empty() {
        let sign = if iv.microseconds < 0 { "-" } else { "" };
        let abs = iv.microseconds.unsigned_abs();
        let secs = abs / 1_000_000;
        let micros = abs % 1_000_000;
        let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
        let mut time = format!("{sign}{h:02}:{m:02}:{s:02}");
        if micros != 0 {
            time.push_str(format!(".{micros:06}").trim_end_matches('0'));
        }
        parts.push(time);
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_wrappers_bind_the_postgres_scheme() {
        // One probe per wrapper; the shared behavior is covered in db::url.
        assert_eq!(
            url_with_password("postgres://user@db.example.com:5432/app", "p%rd").unwrap(),
            "postgres://user:p%25rd@db.example.com:5432/app"
        );
        assert_eq!(
            normalize_pg_url("postgresql://user@Db.Example.COM/app").unwrap(),
            "postgres://user@db.example.com:5432/app"
        );
        assert_eq!(
            url_target("postgres://u@db.internal/app").unwrap(),
            ("db.internal".to_string(), 5432)
        );
        assert_eq!(
            url_via_local_port("postgres://u@db.internal:5432/app?sslmode=disable", 40123).unwrap(),
            "postgres://u@127.0.0.1:40123/app?sslmode=disable"
        );
        assert_eq!(
            build_url("db.example.com", "", "app", "user", "prefer").unwrap(),
            "postgres://user@db.example.com:5432/app?sslmode=prefer"
        );
    }

    #[test]
    fn connect_errors_are_categorized() {
        let auth = sqlx::Error::Protocol("password authentication failed for user \"x\"".into());
        assert!(friendly_connect_error(&auth).starts_with("authentication failed"));
        let net = sqlx::Error::Protocol("Connection refused (os error 111)".into());
        assert!(friendly_connect_error(&net).starts_with("network error"));
        let tls = sqlx::Error::Protocol("TLS handshake failed".into());
        assert!(friendly_connect_error(&tls).starts_with("TLS error"));
        // A typo'd username is not a password problem — it must not trigger
        // the password prompt (which keys on "authentication failed").
        let role = sqlx::Error::Protocol("role \"nope\" does not exist".into());
        let friendly = friendly_connect_error(&role);
        assert!(friendly.starts_with("unknown role"));
        assert!(!friendly.contains("authentication failed"));
        // A non-Postgres server answering the handshake (e.g. SQL Server on
        // 1433) mentions SSL but is a wrong-server problem, not a TLS one.
        let not_pg = sqlx::Error::Protocol(
            "encountered unexpected or invalid data: unexpected response from SSLRequest: 0x00"
                .into(),
        );
        let friendly = friendly_connect_error(&not_pg);
        assert!(friendly.starts_with("the server doesn't appear to be Postgres"));
        assert!(!friendly.starts_with("TLS error"));
        // An X.509 v1 server certificate (FRE-89). rustls's own wording names
        // neither "tls" nor "ssl", so without its own arm this would fall
        // through to the uncategorized branch and reach the user as
        // `UnsupportedCertVersion` — true, and useless.
        let v1 = sqlx::Error::Protocol(
            "error communicating with database: invalid peer certificate: \
             Other(OtherError(UnsupportedCertVersion))"
                .into(),
        );
        let friendly = friendly_connect_error(&v1);
        assert!(friendly.contains("X.509 v1"), "{friendly}");
        assert!(friendly.contains("sslmode=disable"), "{friendly}");
    }

    #[test]
    fn interval_formats_compactly() {
        let iv = |months, days, microseconds| PgInterval {
            months,
            days,
            microseconds,
        };
        // All parts present, plural units, fractional seconds trimmed.
        assert_eq!(
            format_interval(&iv(14, 2, (3 * 3600 + 4 * 60 + 5) * 1_000_000)),
            "14 mons 2 days 03:04:05"
        );
        assert_eq!(
            format_interval(&iv(1, 1, 5_000_000 + 500_000)),
            "1 mon 1 day 00:00:05.5"
        );
        // Zero parts are omitted; all-zero renders a zero time.
        assert_eq!(format_interval(&iv(0, 3, 0)), "3 days");
        assert_eq!(format_interval(&iv(0, 0, 0)), "00:00:00");
        // Negative components each carry their sign.
        assert_eq!(
            format_interval(&iv(-1, -2, -5_000_000)),
            "-1 mon -2 days -00:00:05"
        );
    }

    #[test]
    fn line_col_maps_character_positions() {
        let sql = "SELECT 1 +\n  bad_col\nFROM t";
        assert_eq!(line_col(sql, 1), Some((1, 1)));
        assert_eq!(line_col(sql, 8), Some((1, 8)));
        // First char of line 2 ("SELECT 1 +\n" is 11 chars).
        assert_eq!(line_col(sql, 12), Some((2, 1)));
        assert_eq!(line_col(sql, 14), Some((2, 3)));
        // One past the end is valid (server points at missing input).
        let len = sql.chars().count();
        assert_eq!(line_col(sql, len + 1), Some((3, 7)));
        // Zero and far-out-of-range positions are dropped.
        assert_eq!(line_col(sql, 0), None);
        assert_eq!(line_col(sql, len + 2), None);
    }

    #[test]
    fn line_col_counts_characters_not_bytes() {
        // "ыыы" is 6 bytes but 3 chars; position 5 is the 'X'.
        assert_eq!(line_col("ыыыаX", 5), Some((1, 5)));
    }
}
