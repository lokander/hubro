//! DDL for SQL Server objects (FRE-108): the stored module text for views,
//! and — since SQL Server ships no server-side generator for tables and
//! indexes (SSMS scripts those client-side through SMO) — a reconstruction
//! from the sys catalog, rendered by the shared writers in [`crate::db::ddl`].

use super::pool::{query_with, MssqlPool};
use super::{format_mssql_type, strip_default_parens};
use crate::db::ddl::{
    create_index_sql, create_table_sql, ColumnExtra, Ddl, DdlObject, IndexExtras, TableExtras,
};
use crate::db::error::DbError;
use crate::db::schema::{IndexMeta, TableKind, TableMeta};
use crate::db::sql::{quote_ident, Dialect};
use crate::db::value::{row_flag, row_opt_int, row_opt_text, row_text, Value};

/// DDL for one object (FRE-108).
///
/// SQL Server keeps the *original text* of every module in `sys.sql_modules`,
/// so a view's definition is reproduced exactly as it was written —
/// deliberately preferred over `sp_helptext`, which returns the same text
/// chopped into 255-character rows that have to be stitched back together.
///
/// Tables and indexes have no generator (SSMS scripts those client-side
/// through SMO), so both are rebuilt here from the sys catalog and labelled
/// as reconstructions.
pub async fn fetch_ddl(
    pool: &MssqlPool,
    table: &TableMeta,
    object: &DdlObject,
) -> Result<Ddl, DbError> {
    let schema = table.schema.clone().unwrap_or_else(|| "dbo".into());
    let params = [Value::Text(schema.clone()), Value::Text(table.name.clone())];

    if let DdlObject::Index(name) = object {
        return index_ddl(pool, table, &params, name).await;
    }

    if matches!(table.kind, TableKind::View | TableKind::MaterializedView) {
        let rows = query_with(
            pool,
            "SELECT m.definition \
             FROM sys.sql_modules m \
             JOIN sys.objects o ON o.object_id = m.object_id \
             JOIN sys.schemas s ON s.schema_id = o.schema_id \
             WHERE s.name = @P1 AND o.name = @P2",
            &params,
        )
        .await?;
        let Some(definition) = rows.rows.first().and_then(|r| row_opt_text(r, 0)) else {
            // A module created WITH ENCRYPTION has a NULL definition.
            return Err(DbError::Introspect(format!(
                "no readable definition for {schema}.{} — the module may be encrypted",
                table.name
            )));
        };
        // Left byte-for-byte: the stored batch may end in a line comment, so
        // appending a terminator could comment it out.
        return Ok(Ddl::native(definition.trim_end()));
    }

    let extras = match table_ddl_extras(pool, &params).await {
        Ok(extras) => extras,
        // A failed catalog read degrades to the metadata-only rebuild rather
        // than failing the action outright. The standing caveats are extended,
        // never replaced: the worse output must not claim to reproduce more
        // than the good one.
        Err(err) => {
            let mut caveats = mssql_standing_caveats();
            caveats.push(format!(
                "column collations, identity seeds, computed columns, constraints and indexes \
                 — reading the sys catalog failed ({})",
                err.message()
            ));
            TableExtras {
                caveats,
                ..TableExtras::default()
            }
        }
    };
    Ok(create_table_sql(Dialect::SqlServer, table, &extras))
}

/// What SSMS would script and this rebuild does not, however the catalog read
/// went.
fn mssql_standing_caveats() -> Vec<String> {
    vec![
        "filegroup, partition scheme and index options (FILLFACTOR, compression)".into(),
        "triggers and system-versioning (temporal) settings".into(),
        "extended properties and permissions".into(),
    ]
}

/// Per-table facts for the `CREATE TABLE` rebuild: column collations,
/// identity specifications, computed-column expressions, every constraint
/// (primary key, unique, foreign key *with its referential actions*, check),
/// and the indexes that do not back a constraint.
async fn table_ddl_extras(pool: &MssqlPool, params: &[Value]) -> Result<TableExtras, DbError> {
    // A column's collation is only worth writing when it differs from the
    // database default — otherwise every char column would carry noise.
    let column_rows = query_with(
        pool,
        "SELECT c.name, t.name, c.max_length, c.precision, c.scale, \
                CASE WHEN c.collation_name <> \
                          CONVERT(nvarchar(128), DATABASEPROPERTYEX(DB_NAME(), 'Collation')) \
                     THEN c.collation_name END, \
                CAST(ic.seed_value AS bigint), CAST(ic.increment_value AS bigint), \
                cc.definition, cc.is_persisted, d.name, d.definition \
         FROM sys.columns c \
         JOIN sys.objects o ON o.object_id = c.object_id \
         JOIN sys.schemas s ON s.schema_id = o.schema_id \
         JOIN sys.types t ON t.user_type_id = c.user_type_id \
         LEFT JOIN sys.identity_columns ic \
           ON ic.object_id = c.object_id AND ic.column_id = c.column_id \
         LEFT JOIN sys.computed_columns cc \
           ON cc.object_id = c.object_id AND cc.column_id = c.column_id \
         LEFT JOIN sys.default_constraints d \
           ON d.parent_object_id = c.object_id AND d.parent_column_id = c.column_id \
         WHERE s.name = @P1 AND o.name = @P2 \
         ORDER BY c.column_id",
        params,
    )
    .await?;

    // Primary key first, then unique constraints, each with its backing
    // index's clustering — not cosmetic on SQL Server, where a clustered
    // index *is* the table's row storage.
    let key_rows = query_with(
        pool,
        "SELECT kc.name, kc.type, i.type_desc, col.name, ic.is_descending_key \
         FROM sys.key_constraints kc \
         JOIN sys.objects o ON o.object_id = kc.parent_object_id \
         JOIN sys.schemas s ON s.schema_id = o.schema_id \
         JOIN sys.indexes i \
           ON i.object_id = kc.parent_object_id AND i.index_id = kc.unique_index_id \
         JOIN sys.index_columns ic \
           ON ic.object_id = i.object_id AND ic.index_id = i.index_id AND ic.key_ordinal > 0 \
         JOIN sys.columns col \
           ON col.object_id = ic.object_id AND col.column_id = ic.column_id \
         WHERE s.name = @P1 AND o.name = @P2 \
         ORDER BY CASE WHEN kc.type = 'PK' THEN 0 ELSE 1 END, kc.name, ic.key_ordinal",
        params,
    )
    .await?;

    // The referential actions are the reason this query exists: a rebuilt FK
    // that lost its ON DELETE CASCADE is a different constraint.
    let fk_rows = query_with(
        pool,
        "SELECT fk.name, rs.name, ro.name, pc.name, rc.name, \
                fk.delete_referential_action_desc, fk.update_referential_action_desc, \
                fk.is_disabled, fk.is_not_trusted \
         FROM sys.foreign_keys fk \
         JOIN sys.objects o ON o.object_id = fk.parent_object_id \
         JOIN sys.schemas s ON s.schema_id = o.schema_id \
         JOIN sys.objects ro ON ro.object_id = fk.referenced_object_id \
         JOIN sys.schemas rs ON rs.schema_id = ro.schema_id \
         JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
         JOIN sys.columns pc \
           ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id \
         JOIN sys.columns rc \
           ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
         WHERE s.name = @P1 AND o.name = @P2 \
         ORDER BY fk.name, fkc.constraint_column_id",
        params,
    )
    .await?;

    // Column-level CHECKs are table-level constraints in the catalog, so they
    // come back here too and are re-emitted as such.
    let check_rows = query_with(
        pool,
        "SELECT cc.name, cc.definition, cc.is_disabled, cc.is_not_trusted \
         FROM sys.check_constraints cc \
         JOIN sys.objects o ON o.object_id = cc.parent_object_id \
         JOIN sys.schemas s ON s.schema_id = o.schema_id \
         WHERE s.name = @P1 AND o.name = @P2 \
         ORDER BY cc.name",
        params,
    )
    .await?;

    let index_rows = query_with(
        pool,
        &format!(
            "{INDEX_DDL_SELECT} AND i.is_primary_key = 0 AND i.is_unique_constraint = 0 \
             ORDER BY i.name, ic.is_included_column, ic.key_ordinal, ic.index_column_id"
        ),
        params,
    )
    .await?;

    let mut extras = TableExtras {
        caveats: mssql_standing_caveats(),
        // The read succeeded, so an empty list means "no table-level
        // constraints", not "we could not tell".
        constraints: Some(Vec::new()),
        ..TableExtras::default()
    };

    for row in &column_rows.rows {
        let persisted = row_flag(row, 9);
        let computed = row_opt_text(row, 8).map(|definition| {
            // The catalog already parenthesizes the expression.
            format!(
                "AS {definition}{}",
                if persisted { " PERSISTED" } else { "" }
            )
        });
        let identity = row_opt_int(row, 6).map(|seed| {
            let increment = row_opt_int(row, 7).unwrap_or(1);
            format!("IDENTITY({seed},{increment})")
        });
        extras.columns.insert(
            row_text(row, 0),
            ColumnExtra {
                type_name: Some(format_mssql_type(
                    &row_text(row, 1),
                    row_opt_int(row, 2).unwrap_or_default(),
                    row_opt_int(row, 3).unwrap_or_default(),
                    row_opt_int(row, 4).unwrap_or_default(),
                )),
                collation: row_opt_text(row, 5),
                computed,
                computed_persisted: persisted,
                identity,
                // Unwrapped from the catalog's parenthesis armor, same as
                // introspection does, then re-emitted under its own name:
                // dropping a default on SQL Server needs that name, and an
                // unnamed one gets a random `DF__tbl__col__…`.
                default: row_opt_text(row, 11).map(|d| strip_default_parens(&d).to_string()),
                default_constraint: row_opt_text(row, 10),
            },
        );
    }

    let constraints = extras.constraints.get_or_insert_with(Vec::new);
    constraints.extend(key_constraints(&key_rows.rows));
    constraints.extend(fk_constraints(&fk_rows.rows));
    for row in &check_rows.rows {
        constraints.push(format!(
            "CONSTRAINT {} CHECK {}",
            quote_ident(&row_text(row, 0)),
            row_text(row, 1)
        ));
    }

    // A constraint the user deliberately disabled (or left untrusted with
    // WITH NOCHECK) comes back enforced — a behaviour change, so it is named.
    //
    // This is a choice, not a limitation: `ALTER TABLE … NOCHECK CONSTRAINT`
    // could be appended the same way `CREATE INDEX` already is. It is declared
    // rather than reproduced because the two failure directions are not
    // symmetric. Re-creating a constraint enforced fails *loudly* on the first
    // offending row, and the user fixes it. Emitting NOCHECK hands them a
    // constraint that looks present and quietly enforces nothing — which is
    // this feature's own hazard, DDL that reads authoritative and isn't,
    // relocated rather than avoided. Don't "fix" this by adding the trailer.
    //
    // Only emitted when such a constraint actually exists: a caveat that fires
    // when nothing is wrong is worse than no caveat at all.
    let mut unenforced: Vec<String> = Vec::new();
    for (rows, disabled, untrusted) in [(&check_rows.rows, 2, 3), (&fk_rows.rows, 7, 8)] {
        for row in rows {
            let name = row_text(row, 0);
            if (row_flag(row, disabled) || row_flag(row, untrusted)) && !unenforced.contains(&name)
            {
                unenforced.push(name);
            }
        }
    }
    if !unenforced.is_empty() {
        extras.caveats.push(format!(
            "the disabled / untrusted state of {} — {} re-created enforced and trusted",
            unenforced.join(", "),
            if unenforced.len() == 1 {
                "it is"
            } else {
                "they are"
            }
        ));
    }

    for index in collect_index_ddl(&index_rows.rows) {
        match index.rowstore_ddl(params.first(), params.get(1)) {
            Some(sql) => extras.indexes.push(sql),
            // A columnstore/XML/spatial/hash index has no key list this
            // renderer can produce; emitting `CREATE INDEX … ()` would be a
            // syntax error dressed up as output.
            None => extras.caveats.push(format!(
                "index {} ({} — not a rowstore index)",
                index.meta.name, index.type_desc
            )),
        }
    }
    Ok(extras)
}

/// `PRIMARY KEY` / `UNIQUE` clauses from the key-constraint rows (one row per
/// key column, each constraint's rows contiguous).
fn key_constraints(rows: &[Vec<Value>]) -> Vec<String> {
    let mut keys: Vec<(String, String, Vec<String>)> = Vec::new();
    for row in rows {
        let name = row_text(row, 0);
        // kc.type is char(2), so 'PK'/'UQ' arrive padded.
        let kind = if row_text(row, 1).trim() == "PK" {
            "PRIMARY KEY"
        } else {
            "UNIQUE"
        };
        let clustering = if row_text(row, 2).trim() == "CLUSTERED" {
            "CLUSTERED"
        } else {
            "NONCLUSTERED"
        };
        let column = format!(
            "{} {}",
            quote_ident(&row_text(row, 3)),
            if row_flag(row, 4) { "DESC" } else { "ASC" }
        );
        match keys.last_mut() {
            Some((last, _, columns)) if *last == name => columns.push(column),
            _ => keys.push((name, format!("{kind} {clustering}"), vec![column])),
        }
    }
    keys.into_iter()
        .map(|(name, kind, columns)| {
            format!(
                "CONSTRAINT {} {kind} ({})",
                quote_ident(&name),
                columns.join(", ")
            )
        })
        .collect()
}

/// `FOREIGN KEY` clauses from the foreign-key rows (one row per key column,
/// each constraint's rows contiguous), including referential actions.
fn fk_constraints(rows: &[Vec<Value>]) -> Vec<String> {
    struct Fk {
        name: String,
        columns: Vec<String>,
        target: String,
        referenced: Vec<String>,
        actions: String,
    }
    let mut fks: Vec<Fk> = Vec::new();
    for row in rows {
        let name = row_text(row, 0);
        let column = quote_ident(&row_text(row, 3));
        let referenced = quote_ident(&row_text(row, 4));
        if let Some(last) = fks.last_mut() {
            if last.name == name {
                last.columns.push(column);
                last.referenced.push(referenced);
                continue;
            }
        }
        // NO_ACTION is the default and stays implicit, as every tool that
        // scripts these does; anything else has to be written out.
        let mut actions = String::new();
        for (idx, keyword) in [(5, "DELETE"), (6, "UPDATE")] {
            let action = row_text(row, idx);
            if !action.is_empty() && action != "NO_ACTION" {
                actions.push_str(&format!(" ON {keyword} {}", action.replace('_', " ")));
            }
        }
        fks.push(Fk {
            name,
            columns: vec![column],
            target: format!(
                "{}.{}",
                quote_ident(&row_text(row, 1)),
                quote_ident(&row_text(row, 2))
            ),
            referenced: vec![referenced],
            actions,
        });
    }
    fks.into_iter()
        .map(|fk| {
            format!(
                "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({}){}",
                quote_ident(&fk.name),
                fk.columns.join(", "),
                fk.target,
                fk.referenced.join(", "),
                fk.actions
            )
        })
        .collect()
}

/// The shared projection behind both index reads: one row per index column,
/// key columns and `INCLUDE` columns alike. Callers append their own filter
/// and `ORDER BY`.
const INDEX_DDL_SELECT: &str = "\
    SELECT i.name, i.is_unique, i.type_desc, i.filter_definition, \
           i.is_primary_key, i.is_unique_constraint, \
           col.name, ic.is_descending_key, ic.is_included_column \
    FROM sys.indexes i \
    JOIN sys.objects o ON o.object_id = i.object_id \
    JOIN sys.schemas s ON s.schema_id = o.schema_id \
    JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
    JOIN sys.columns col ON col.object_id = ic.object_id AND col.column_id = ic.column_id \
    WHERE s.name = @P1 AND o.name = @P2 \
      AND i.index_id > 0 AND i.is_hypothetical = 0 AND i.is_disabled = 0";

/// One index gathered from [`INDEX_DDL_SELECT`] rows, before rendering.
struct IndexDdl {
    meta: IndexMeta,
    extras: IndexExtras,
    /// `sys.indexes.type_desc` verbatim, so the caller can tell a rowstore
    /// index (the only shape this renderer speaks) from a columnstore, XML,
    /// spatial, or hash one.
    type_desc: String,
    /// Whether the index exists only because a PRIMARY KEY / UNIQUE
    /// constraint created it — a `CREATE INDEX` is not how it came to be, so
    /// the caller says so rather than pretending otherwise.
    constraint_backed: bool,
}

impl IndexDdl {
    /// Whether this is a plain b-tree index. Everything else (columnstore, XML,
    /// spatial, memory-optimized hash) has a different `CREATE` grammar; a
    /// columnstore's columns are all non-key, so pushing one through the
    /// rowstore renderer yields `CREATE INDEX … ()` — a syntax error dressed
    /// up as output.
    fn is_rowstore(&self) -> bool {
        matches!(self.type_desc.as_str(), "CLUSTERED" | "NONCLUSTERED")
    }

    /// The rendered `CREATE INDEX` statement without the reconstruction header
    /// (this one is embedded in a table's DDL, which carries its own), or
    /// `None` when the index is not a rowstore index.
    fn rowstore_ddl(&self, schema: Option<&Value>, name: Option<&Value>) -> Option<String> {
        if !self.is_rowstore() {
            return None;
        }
        let text = |value: Option<&Value>| match value {
            Some(Value::Text(t)) => Some(t.clone()),
            _ => None,
        };
        let table = TableMeta {
            schema: text(schema),
            name: text(name).unwrap_or_default(),
            kind: TableKind::Table,
            columns: Vec::new(),
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            restriction: None,
            internal: None,
            kind_label: None,
        };
        Some(create_index_sql(Dialect::SqlServer, &table, &self.meta, &self.extras).sql)
    }
}

/// Groups [`INDEX_DDL_SELECT`] rows (ordered so each index's rows are
/// contiguous, key columns before included ones) into one entry per index.
fn collect_index_ddl(rows: &[Vec<Value>]) -> Vec<IndexDdl> {
    let mut out: Vec<IndexDdl> = Vec::new();
    for row in rows {
        let name = row_text(row, 0);
        if out.last().map(|i| i.meta.name.as_str()) != Some(name.as_str()) {
            let filter = row_opt_text(row, 3);
            let type_desc = row_text(row, 2).trim().to_string();
            out.push(IndexDdl {
                meta: IndexMeta {
                    name: name.clone(),
                    unique: row_flag(row, 1),
                    // Same contract as introspection: a filtered index only
                    // guarantees uniqueness among the matching rows.
                    partial: filter.is_some(),
                    columns: Vec::new(),
                },
                extras: IndexExtras {
                    filter,
                    clustered: type_desc == "CLUSTERED",
                    ..IndexExtras::default()
                },
                type_desc,
                constraint_backed: row_flag(row, 4) || row_flag(row, 5),
            });
        }
        let entry = out.last_mut().expect("pushed above when absent");
        let column = row_text(row, 6);
        if row_flag(row, 8) {
            entry.extras.included_columns.push(quote_ident(&column));
        } else {
            entry.extras.key_columns.push(format!(
                "{} {}",
                quote_ident(&column),
                if row_flag(row, 7) { "DESC" } else { "ASC" }
            ));
            entry.meta.columns.push(column);
        }
    }
    out
}

/// DDL for one named index, including the ones a PRIMARY KEY / UNIQUE
/// constraint created: those are listed in the schema pane like any other
/// index, so answering "no such index" for them would be wrong.
async fn index_ddl(
    pool: &MssqlPool,
    table: &TableMeta,
    params: &[Value],
    name: &str,
) -> Result<Ddl, DbError> {
    let mut params = params.to_vec();
    params.push(Value::Text(name.to_string()));
    let rows = query_with(
        pool,
        &format!(
            "{INDEX_DDL_SELECT} AND i.name = @P3 \
             ORDER BY ic.is_included_column, ic.key_ordinal, ic.index_column_id"
        ),
        &params,
    )
    .await?;
    let Some(index) = collect_index_ddl(&rows.rows).into_iter().next() else {
        return Err(DbError::Introspect(format!(
            "no index named {name} on {}",
            table.name
        )));
    };
    if !index.is_rowstore() {
        // Declining is the honest answer: this renderer only speaks the
        // rowstore `CREATE INDEX` grammar, and forcing a columnstore through
        // it produces an empty key list — invalid SQL.
        return Err(DbError::Unsupported(format!(
            "{name} is a {} index; hubro can only script rowstore \
             (CLUSTERED / NONCLUSTERED) indexes",
            index.type_desc
        )));
    }
    let mut ddl = create_index_sql(Dialect::SqlServer, table, &index.meta, &index.extras);
    if index.constraint_backed {
        ddl.caveats.insert(
            0,
            "this index backs a PRIMARY KEY / UNIQUE constraint — it is created by that \
             constraint, not by a CREATE INDEX statement"
                .into(),
        );
    }
    ddl.caveats
        .push("index options (FILLFACTOR, compression) and filegroup placement".into());
    Ok(ddl)
}
