//! DDL text for one schema object (FRE-108).
//!
//! Two sources, and telling them apart is the point. SQLite stores the
//! original `CREATE` statement in `sqlite_master`, Postgres renders view and
//! index definitions with `pg_get_viewdef`/`pg_get_indexdef`, and SQL Server
//! keeps module text in `sys.sql_modules`; all of those come back verbatim as
//! [`DdlSource::Native`]. Nothing generates `CREATE TABLE` on Postgres or SQL
//! Server (and SQL Server has no index generator either), so those statements
//! are rebuilt from the catalog as [`DdlSource::Reconstructed`] and rendered
//! behind a header that says so — a rebuild that quietly dropped a `DEFAULT`,
//! a `COLLATE`, or an `ON DELETE CASCADE` is a *wrong* statement someone may
//! paste into a migration.
//!
//! Everything here is a pure function over data structures; the async catalog
//! reads that fill [`TableExtras`] / [`IndexExtras`] live in the per-backend
//! modules (`sqlite.rs`, `postgres.rs`, `sqlserver.rs`).

use std::collections::BTreeMap;

use super::page::{quote_ident, Dialect};
use super::schema::{IndexMeta, TableKind, TableMeta};

/// Which of a table's objects to render DDL for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DdlObject {
    /// The table, view, or materialized view itself. For a table this also
    /// includes the standalone indexes on it, so one action yields the whole
    /// object rather than a statement that silently loses its indexes.
    Object,
    /// One index on the table, by name.
    Index(String),
}

/// Where a DDL text came from — see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DdlSource {
    /// The server's own stored/generated definition, reproduced as given.
    Native,
    /// Rebuilt from catalog metadata because the backend has no generator.
    Reconstructed,
}

/// The header prefixed to every reconstructed statement. Native output never
/// carries it: the presence of the header is how a reader tells a rebuild
/// from the server's own definition before pasting it somewhere consequential.
const RECONSTRUCTED_HEADER: &str = "\
-- Reconstructed by hubro from catalog metadata. This is NOT the server's own
-- definition — this backend has no DDL generator for this object type.
-- Review it before running it anywhere.";

/// One object's DDL: the statement(s), where they came from, and — for a
/// reconstruction — the attributes the rebuild could not represent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ddl {
    /// The statement text alone, with no provenance header.
    pub sql: String,
    pub source: DdlSource,
    /// Things the reconstruction does not reproduce, each a noun phrase
    /// (e.g. "check constraints"). Listed in the header so a gap is visible
    /// rather than implied. Always empty for [`DdlSource::Native`].
    pub caveats: Vec<String>,
}

impl Ddl {
    /// The server's own definition, shown as-is.
    pub fn native(sql: impl Into<String>) -> Ddl {
        Ddl {
            sql: sql.into(),
            source: DdlSource::Native,
            caveats: Vec::new(),
        }
    }

    /// A rebuild from catalog metadata, with what it leaves out.
    pub fn reconstructed(sql: impl Into<String>, caveats: Vec<String>) -> Ddl {
        Ddl {
            sql: sql.into(),
            source: DdlSource::Reconstructed,
            caveats,
        }
    }

    /// The text shown in the UI and put on the clipboard: native output
    /// verbatim, a reconstruction behind [`RECONSTRUCTED_HEADER`] plus its
    /// caveat line. Copy carries the header deliberately — the warning has to
    /// travel with the SQL, not stay behind in the window it was copied from.
    pub fn text(&self) -> String {
        match self.source {
            DdlSource::Native => self.sql.clone(),
            DdlSource::Reconstructed => {
                let mut out = String::from(RECONSTRUCTED_HEADER);
                if !self.caveats.is_empty() {
                    out.push_str("\n-- Not reproduced: ");
                    out.push_str(&self.caveats.join("; "));
                    out.push('.');
                }
                out.push_str("\n\n");
                out.push_str(&self.sql);
                out
            }
        }
    }
}

/// Per-column facts a faithful `CREATE TABLE` needs that [`ColumnMeta`] does
/// not carry, read from the catalog when the DDL is requested.
///
/// [`ColumnMeta`]: super::schema::ColumnMeta
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ColumnExtra {
    /// The exact declared type as the server prints it, overriding
    /// `ColumnMeta::type_name`. Needed on Postgres, where introspection reads
    /// `information_schema.columns.data_type` — which reports `varchar(20)`
    /// as the bare `character varying` and any user type as `USER-DEFINED`.
    pub type_name: Option<String>,
    /// Non-default collation. Omitted when the column uses its type's
    /// default, which is the overwhelmingly common case.
    pub collation: Option<String>,
    /// A clause that *replaces* the type and everything after it, for a
    /// column defined by an expression: SQL Server's `AS (expr) PERSISTED`.
    pub computed: Option<String>,
    /// Whether a [`Self::computed`] column is persisted. T-SQL only accepts an
    /// explicit `NOT NULL` on a *persisted* computed column — a virtual one
    /// derives its nullability from the expression — so the renderer needs to
    /// know which it is before it may write the nullability out.
    pub computed_persisted: bool,
    /// A clause inserted after the type: an identity specification
    /// (`GENERATED ALWAYS AS IDENTITY`, `IDENTITY(1,1)`) or Postgres'
    /// `GENERATED ALWAYS AS (expr) STORED`.
    pub identity: Option<String>,
    /// The default expression as the *catalog* renders it, overriding
    /// `ColumnMeta::default`.
    pub default: Option<String>,
    /// The name of the default constraint, where the backend names them (SQL
    /// Server does, and dropping a default is done by that name — including
    /// the auto-generated `DF__tbl__col__…` form, which is only stable if it
    /// is written out).
    pub default_constraint: Option<String>,
}

/// Table facts beyond [`TableMeta`], gathered per object by the backend.
///
/// Every field is optional in the sense that an empty one falls back to what
/// `TableMeta` already knows; the backend records what it could not supply in
/// `caveats` so the gap reaches the header rather than disappearing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableExtras {
    /// Extra facts per column name. Columns without an entry render from
    /// `TableMeta` alone.
    pub columns: BTreeMap<String, ColumnExtra>,
    /// Complete table-level constraint clauses in catalog order, each a full
    /// body ready to place inside the parentheses (e.g.
    /// `CONSTRAINT "t_pkey" PRIMARY KEY ("id")`). Server-rendered where the
    /// backend can (`pg_get_constraintdef`).
    ///
    /// `None` means the backend could **not read them**, which is emphatically
    /// not the same as `Some(vec![])` — a table with no constraints at all is
    /// the most common shape in any database. Only `None` falls back to the
    /// primary key and foreign keys `TableMeta` carries, and only `None` adds
    /// the "check constraints / referential actions are missing" caveats. A
    /// caveat that fires when nothing is wrong teaches people to skim past the
    /// ones that matter.
    pub constraints: Option<Vec<String>>,
    /// Complete `CREATE INDEX` statements for the indexes that do not back a
    /// constraint (those are already in `constraints`), appended after the
    /// table.
    pub indexes: Vec<String>,
    /// Attributes this backend's fetch knowingly does not reproduce.
    pub caveats: Vec<String>,
}

/// Index facts beyond [`IndexMeta`], gathered per index by the backend.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexExtras {
    /// Key columns with their sort direction, already quoted and rendered
    /// (e.g. `"created_at" DESC`). Empty falls back to `IndexMeta::columns`,
    /// which carries no direction.
    pub key_columns: Vec<String>,
    /// Non-key `INCLUDE` columns (SQL Server), quoted.
    pub included_columns: Vec<String>,
    /// The predicate of a partial/filtered index, without the `WHERE`.
    pub filter: Option<String>,
    /// Whether the index is clustered (SQL Server). Rendered as the explicit
    /// `CLUSTERED`/`NONCLUSTERED` keyword, which is never a no-op there: a
    /// clustered index *is* the table's storage.
    pub clustered: bool,
    /// Attributes this backend's fetch knowingly does not reproduce.
    pub caveats: Vec<String>,
}

/// Schema-qualified, quoted object name.
fn qualified(schema: Option<&str>, name: &str) -> String {
    match schema {
        Some(schema) => format!("{}.{}", quote_ident(schema), quote_ident(name)),
        None => quote_ident(name),
    }
}

/// Adds the statement terminator that stored/generated definitions leave off.
///
/// The semicolon goes on its own line when the statement ends *inside* a `--`
/// line comment — `sqlite_master` happily stores `CREATE INDEX … -- note`, and
/// appending `;` to that line comments the terminator out. One unterminated
/// statement then swallows the next one, and the whole output stops being
/// runnable (FRE-108).
pub fn terminate(sql: &str) -> String {
    let trimmed = sql.trim_end();
    if trimmed.ends_with(';') {
        return trimmed.to_string();
    }
    if ends_inside_line_comment(trimmed) {
        format!("{trimmed}\n;")
    } else {
        format!("{trimmed};")
    }
}

/// Whether `sql` ends while a `--` line comment is still open. Scans string
/// literals, quoted identifiers, and block comments so a `--` inside any of
/// them doesn't count.
fn ends_inside_line_comment(sql: &str) -> bool {
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                // A line comment runs to the newline; reaching the end without
                // one means the statement ends inside it.
                return !sql[i..].contains('\n');
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => match sql[i + 2..].find("*/") {
                Some(end) => i += 2 + end + 2,
                // Unterminated block comment: nothing can close it, so the
                // terminator has to go on its own line either way.
                None => return true,
            },
            quote @ (b'\'' | b'"' | b'`') => {
                i += 1;
                while i < bytes.len() && bytes[i] != quote {
                    i += 1;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    false
}

/// Wraps a native view body (`pg_get_viewdef`) in the `CREATE VIEW` header
/// the server does not return. Only Postgres needs this: SQLite and SQL
/// Server both store the complete original statement.
pub fn create_view_sql(table: &TableMeta, body: &str) -> String {
    let keyword = match table.kind {
        TableKind::MaterializedView => "CREATE MATERIALIZED VIEW",
        _ => "CREATE VIEW",
    };
    let name = qualified(table.schema.as_deref(), &table.name);
    // pg_get_viewdef(…, true) pretty-prints and terminates with a semicolon
    // already; anything else gets one so the statement is runnable.
    format!("{keyword} {name} AS\n{}", terminate(body))
}

/// Renders `CREATE TABLE` from `table` plus whatever `extras` the backend
/// could read. Pure: every dialect difference is a formatting decision made
/// from `dialect`, never a catalog lookup.
pub fn create_table_sql(dialect: Dialect, table: &TableMeta, extras: &TableExtras) -> Ddl {
    let mut items: Vec<String> = Vec::with_capacity(table.columns.len() + 2);
    for column in &table.columns {
        let extra = extras.columns.get(&column.name);
        items.push(column_clause(dialect, column, extra));
    }
    match &extras.constraints {
        Some(constraints) => items.extend(constraints.iter().cloned()),
        None => items.extend(fallback_constraints(table)),
    }

    let mut sql = format!(
        "CREATE TABLE {} (\n    {}\n);",
        qualified(table.schema.as_deref(), &table.name),
        items.join(",\n    ")
    );
    for index in &extras.indexes {
        sql.push_str("\n\n");
        sql.push_str(&terminate(index));
    }

    let mut caveats = extras.caveats.clone();
    if extras.constraints.is_none() {
        // The metadata fallback — reached only when the catalog read failed.
        // TableMeta carries a primary key and the foreign-key column pairs,
        // and nothing else about constraints.
        caveats.push("check constraints".into());
        caveats.push("unique constraints".into());
        caveats.push("foreign-key ON DELETE / ON UPDATE actions".into());
        caveats.push("constraint names".into());
    }
    Ddl::reconstructed(sql, caveats)
}

/// One column's line inside `CREATE TABLE`.
///
/// A computed/generated column defined by an expression replaces the type and
/// everything after it (that is the SQL Server grammar; Postgres' generated
/// columns arrive through `identity` instead). Otherwise the order is type,
/// collation, identity, default, nullability — accepted by both dialects that
/// reach this function.
///
/// Primary keys are deliberately *not* rendered inline, even for a
/// single-column key: they arrive as a table-level clause from the catalog
/// (or from [`fallback_constraints`]), and emitting both would duplicate the
/// constraint.
fn column_clause(
    dialect: Dialect,
    column: &super::schema::ColumnMeta,
    extra: Option<&ColumnExtra>,
) -> String {
    let name = quote_ident(&column.name);
    if let Some(computed) = extra.and_then(|e| e.computed.as_deref()) {
        let mut clause = format!("{name} {computed}");
        // A persisted computed column carries its own nullability and T-SQL
        // accepts it here; dropping it would turn `NOT NULL` into nullable.
        // A non-persisted one derives nullability from the expression and
        // rejects an explicit `NOT NULL`.
        if extra.is_some_and(|e| e.computed_persisted) && !column.nullable {
            clause.push_str(" NOT NULL");
        }
        return clause;
    }
    let type_name = extra
        .and_then(|e| e.type_name.clone())
        .unwrap_or_else(|| column.type_name.clone());
    let mut clause = format!("{name} {type_name}");
    if let Some(collation) = extra.and_then(|e| e.collation.as_deref()) {
        // Postgres collation names are ordinary identifiers and need
        // quoting ("en_US.utf8" contains a dot); T-SQL collation names are
        // keywords and must NOT be quoted.
        let rendered = match dialect {
            Dialect::SqlServer => collation.to_string(),
            Dialect::Sqlite | Dialect::Postgres => quote_ident(collation),
        };
        clause.push_str(&format!(" COLLATE {rendered}"));
    }
    // The catalog's own default expression wins over the browsable metadata's
    // (it may carry the constraint name); either way an identity column has no
    // literal default, so the two are mutually exclusive on both dialects.
    let default = extra
        .and_then(|e| e.default.as_ref())
        .or(column.default.as_ref());
    if let Some(identity) = extra.and_then(|e| e.identity.as_deref()) {
        clause.push(' ');
        clause.push_str(identity);
    } else if let Some(default) = default {
        match extra.and_then(|e| e.default_constraint.as_deref()) {
            Some(name) => clause.push_str(&format!(
                " CONSTRAINT {} DEFAULT {default}",
                quote_ident(name)
            )),
            None => clause.push_str(&format!(" DEFAULT {default}")),
        }
    }
    if !column.nullable {
        clause.push_str(" NOT NULL");
    }
    clause
}

/// Constraint clauses rebuilt from [`TableMeta`] alone, used when the backend
/// could not read the catalog's own constraint definitions. Deliberately
/// limited to what the metadata actually proves: the primary key and the
/// foreign-key column pairs. Referential actions, check constraints, unique
/// constraints, and constraint names are not in `TableMeta`, and inventing
/// them would be worse than the caveat that says they are missing.
fn fallback_constraints(table: &TableMeta) -> Vec<String> {
    let mut out = Vec::new();
    let pk = table.primary_key();
    if !pk.is_empty() {
        let columns: Vec<String> = pk.iter().map(|c| quote_ident(&c.name)).collect();
        out.push(format!("PRIMARY KEY ({})", columns.join(", ")));
    }
    for fk in &table.foreign_keys {
        let columns: Vec<String> = fk.columns.iter().map(|c| quote_ident(c)).collect();
        let target = qualified(fk.referenced_schema.as_deref(), &fk.referenced_table);
        // A `None` referenced column means the FK points at the target's
        // implicit primary key (SQLite allows omitting it), which is exactly
        // how it is written back.
        let referenced: Vec<String> = fk
            .referenced_columns
            .iter()
            .flatten()
            .map(|c| quote_ident(c))
            .collect();
        let target_columns = if referenced.len() == fk.columns.len() {
            format!(" ({})", referenced.join(", "))
        } else {
            String::new()
        };
        out.push(format!(
            "FOREIGN KEY ({}) REFERENCES {target}{target_columns}",
            columns.join(", ")
        ));
    }
    out
}

/// Renders `CREATE INDEX` for one index on `table`.
pub fn create_index_sql(
    dialect: Dialect,
    table: &TableMeta,
    index: &IndexMeta,
    extras: &IndexExtras,
) -> Ddl {
    let unique = if index.unique { "UNIQUE " } else { "" };
    // Clustered-ness is not optional detail on SQL Server: a clustered index
    // is the table's physical row store, so recreating one as nonclustered
    // silently changes the table. Other dialects have no such keyword.
    let clustering = match dialect {
        Dialect::SqlServer if extras.clustered => "CLUSTERED ",
        Dialect::SqlServer => "NONCLUSTERED ",
        _ => "",
    };
    let key_columns: Vec<String> = if extras.key_columns.is_empty() {
        index.columns.iter().map(|c| quote_ident(c)).collect()
    } else {
        extras.key_columns.clone()
    };
    let mut sql = format!(
        "CREATE {unique}{clustering}INDEX {} ON {} ({})",
        quote_ident(&index.name),
        qualified(table.schema.as_deref(), &table.name),
        key_columns.join(", ")
    );
    if !extras.included_columns.is_empty() {
        sql.push_str(&format!(
            " INCLUDE ({})",
            extras.included_columns.join(", ")
        ));
    }
    if let Some(filter) = &extras.filter {
        sql.push_str(&format!(" WHERE {filter}"));
    }
    sql.push(';');

    let mut caveats = extras.caveats.clone();
    // A partial index whose predicate could not be read would silently become
    // a full index — the single most dangerous thing this renderer can emit.
    if index.partial && extras.filter.is_none() {
        caveats.push("the index predicate (this index is partial/filtered)".into());
    }
    if extras.key_columns.is_empty() {
        caveats.push("per-column sort direction".into());
    }
    Ddl::reconstructed(sql, caveats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::{ColumnMeta, ForeignKeyMeta, Generated, TypeDetail};

    fn column(name: &str, type_name: &str, nullable: bool) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            type_name: type_name.into(),
            nullable,
            primary_key_position: None,
            default: None,
            generated: Generated::Never,
            type_detail: TypeDetail::Plain,
        }
    }

    fn table(schema: Option<&str>, name: &str, columns: Vec<ColumnMeta>) -> TableMeta {
        TableMeta {
            schema: schema.map(str::to_string),
            name: name.into(),
            kind: TableKind::Table,
            columns,
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            restriction: None,
            internal: None,
            kind_label: None,
        }
    }

    #[test]
    fn native_ddl_carries_no_header() {
        let ddl = Ddl::native("CREATE TABLE t (a);");
        assert_eq!(ddl.text(), "CREATE TABLE t (a);");
        assert_eq!(ddl.source, DdlSource::Native);
    }

    #[test]
    fn reconstructed_ddl_is_labelled_and_lists_its_gaps() {
        let ddl = Ddl::reconstructed("CREATE TABLE t (a int);", vec!["triggers".into()]);
        let text = ddl.text();
        assert!(text.starts_with("-- Reconstructed by hubro from catalog metadata."));
        assert!(text.contains("NOT the server's own"));
        assert!(text.contains("-- Not reproduced: triggers."));
        assert!(text.ends_with("CREATE TABLE t (a int);"));
        // No caveats: the header stands alone, with no dangling list.
        let clean = Ddl::reconstructed("CREATE TABLE t (a int);", vec![]);
        assert!(!clean.text().contains("Not reproduced"));
    }

    #[test]
    fn postgres_table_renders_types_defaults_identity_and_constraints() {
        let mut id = column("id", "integer", false);
        id.primary_key_position = Some(1);
        let mut name = column("name", "character varying", false);
        name.default = Some("'anon'::text".into());
        let meta = table(Some("public"), "person", vec![id, name]);
        let mut extras = TableExtras {
            constraints: Some(vec![r#"CONSTRAINT "person_pkey" PRIMARY KEY ("id")"#.into()]),
            indexes: vec![
                "CREATE INDEX person_name_idx ON public.person USING btree (name)".into(),
            ],
            ..TableExtras::default()
        };
        extras.columns.insert(
            "id".into(),
            ColumnExtra {
                identity: Some("GENERATED ALWAYS AS IDENTITY".into()),
                ..ColumnExtra::default()
            },
        );
        extras.columns.insert(
            "name".into(),
            ColumnExtra {
                // The exact type the catalog prints, not data_type's
                // length-less `character varying`.
                type_name: Some("character varying(40)".into()),
                collation: Some("en_US.utf8".into()),
                ..ColumnExtra::default()
            },
        );
        let ddl = create_table_sql(Dialect::Postgres, &meta, &extras);
        assert_eq!(
            ddl.sql,
            concat!(
                "CREATE TABLE \"public\".\"person\" (\n",
                "    \"id\" integer GENERATED ALWAYS AS IDENTITY NOT NULL,\n",
                "    \"name\" character varying(40) COLLATE \"en_US.utf8\" \
                 DEFAULT 'anon'::text NOT NULL,\n",
                "    CONSTRAINT \"person_pkey\" PRIMARY KEY (\"id\")\n",
                ");\n\n",
                "CREATE INDEX person_name_idx ON public.person USING btree (name);"
            )
        );
        // Server-rendered constraints: no fallback caveats.
        assert!(ddl.caveats.is_empty());
        assert_eq!(ddl.source, DdlSource::Reconstructed);
    }

    #[test]
    fn sqlserver_table_renders_identity_computed_and_unquoted_collations() {
        let mut id = column("id", "int", false);
        id.primary_key_position = Some(1);
        let note = column("note", "nvarchar(50)", true);
        // Persisted and NOT NULL: T-SQL accepts the nullability here and the
        // column is a different column without it (FRE-108 review).
        let total = column("total", "int", false);
        let loose = column("loose", "int", false);
        let meta = table(Some("dbo"), "orders", vec![id, note, total, loose]);
        let mut extras = TableExtras {
            constraints: Some(vec![
                r#"CONSTRAINT "PK_orders" PRIMARY KEY CLUSTERED ("id" ASC)"#.into(),
            ]),
            ..TableExtras::default()
        };
        extras.columns.insert(
            "id".into(),
            ColumnExtra {
                identity: Some("IDENTITY(1,1)".into()),
                ..ColumnExtra::default()
            },
        );
        extras.columns.insert(
            "note".into(),
            ColumnExtra {
                collation: Some("Latin1_General_CI_AS".into()),
                default: Some("N'hi'".into()),
                default_constraint: Some("DF_orders_note".into()),
                ..ColumnExtra::default()
            },
        );
        extras.columns.insert(
            "total".into(),
            ColumnExtra {
                computed: Some("AS ([id]*(2)) PERSISTED".into()),
                computed_persisted: true,
                ..ColumnExtra::default()
            },
        );
        // A non-persisted computed column derives its nullability from the
        // expression and rejects an explicit NOT NULL, so none is written.
        extras.columns.insert(
            "loose".into(),
            ColumnExtra {
                computed: Some("AS ([id]*(3))".into()),
                ..ColumnExtra::default()
            },
        );
        let ddl = create_table_sql(Dialect::SqlServer, &meta, &extras);
        assert_eq!(
            ddl.sql,
            concat!(
                "CREATE TABLE \"dbo\".\"orders\" (\n",
                "    \"id\" int IDENTITY(1,1) NOT NULL,\n",
                // T-SQL collation names are keywords, never quoted; the
                // default keeps the name you would need to drop it by.
                "    \"note\" nvarchar(50) COLLATE Latin1_General_CI_AS \
                 CONSTRAINT \"DF_orders_note\" DEFAULT N'hi',\n",
                // A computed column replaces the type entirely.
                "    \"total\" AS ([id]*(2)) PERSISTED NOT NULL,\n",
                "    \"loose\" AS ([id]*(3)),\n",
                "    CONSTRAINT \"PK_orders\" PRIMARY KEY CLUSTERED (\"id\" ASC)\n",
                ");"
            )
        );
    }

    #[test]
    fn a_table_with_no_constraints_gets_no_constraint_caveats() {
        // A successful read that found nothing must not look like a failed
        // read: false "not reproduced" entries on the commonest table shape in
        // a database train people to skim past the ones that matter.
        let meta = table(
            Some("public"),
            "plain",
            vec![column("a", "integer", true), column("b", "text", true)],
        );
        let extras = TableExtras {
            constraints: Some(Vec::new()),
            caveats: vec!["triggers".into()],
            ..TableExtras::default()
        };
        let ddl = create_table_sql(Dialect::Postgres, &meta, &extras);
        assert_eq!(
            ddl.sql,
            "CREATE TABLE \"public\".\"plain\" (\n    \"a\" integer,\n    \"b\" text\n);"
        );
        assert_eq!(ddl.caveats, ["triggers"]);
        assert!(!ddl.text().contains("check constraints"));
    }

    #[test]
    fn terminate_keeps_a_trailing_line_comment_from_eating_the_semicolon() {
        assert_eq!(
            terminate("CREATE INDEX i ON t(a)"),
            "CREATE INDEX i ON t(a);"
        );
        assert_eq!(
            terminate("CREATE INDEX i ON t(a);\n"),
            "CREATE INDEX i ON t(a);"
        );
        // `sqlite_master` stores the statement as written, comment and all;
        // appending `;` to that last line would comment the terminator out and
        // silently swallow whatever statement follows.
        assert_eq!(
            terminate("CREATE INDEX i ON t(a) -- note"),
            "CREATE INDEX i ON t(a) -- note\n;"
        );
        // A closed comment, or one inside a literal/identifier, is not open at
        // the end and needs no special treatment.
        assert_eq!(
            terminate("CREATE INDEX i -- note\n ON t(a)"),
            "CREATE INDEX i -- note\n ON t(a);"
        );
        assert_eq!(
            terminate("CREATE TABLE t (a TEXT DEFAULT '-- not a comment')"),
            "CREATE TABLE t (a TEXT DEFAULT '-- not a comment');"
        );
        assert_eq!(
            terminate("CREATE TABLE t (\"a -- b\" TEXT)"),
            "CREATE TABLE t (\"a -- b\" TEXT);"
        );
        // Doubled quotes inside a literal keep the scanner in step.
        assert_eq!(
            terminate("CREATE TABLE t (a TEXT DEFAULT 'O''Brien') /* tail */"),
            "CREATE TABLE t (a TEXT DEFAULT 'O''Brien') /* tail */;"
        );
        assert_eq!(
            terminate("CREATE TABLE t (a) /* unclosed"),
            "CREATE TABLE t (a) /* unclosed\n;"
        );
    }

    #[test]
    fn table_without_catalog_constraints_falls_back_and_says_what_is_missing() {
        let mut id = column("id", "integer", false);
        id.primary_key_position = Some(1);
        let owner = column("owner_id", "integer", true);
        let mut meta = table(Some("public"), "pet", vec![id, owner]);
        meta.foreign_keys.push(ForeignKeyMeta {
            columns: vec!["owner_id".into()],
            referenced_schema: Some("public".into()),
            referenced_table: "person".into(),
            referenced_columns: vec![Some("id".into())],
        });
        // `constraints: None` — the catalog read failed, which is what makes
        // the fallback (and its caveats) correct here.
        let ddl = create_table_sql(Dialect::Postgres, &meta, &TableExtras::default());
        assert!(TableExtras::default().constraints.is_none());
        assert_eq!(
            ddl.sql,
            concat!(
                "CREATE TABLE \"public\".\"pet\" (\n",
                "    \"id\" integer NOT NULL,\n",
                "    \"owner_id\" integer,\n",
                "    PRIMARY KEY (\"id\"),\n",
                "    FOREIGN KEY (\"owner_id\") REFERENCES \"public\".\"person\" (\"id\")\n",
                ");"
            )
        );
        // The gaps are named, not implied.
        assert!(ddl.caveats.iter().any(|c| c.contains("check constraints")));
        assert!(ddl
            .caveats
            .iter()
            .any(|c| c.contains("ON DELETE / ON UPDATE")));
        assert!(ddl.text().contains("-- Not reproduced:"));
    }

    #[test]
    fn fallback_foreign_key_to_an_implicit_primary_key_omits_the_column_list() {
        // SQLite allows `REFERENCES person` with no column list; the metadata
        // records that as a `None` referenced column.
        let mut meta = table(None, "pet", vec![column("owner_id", "INTEGER", true)]);
        meta.foreign_keys.push(ForeignKeyMeta {
            columns: vec!["owner_id".into()],
            referenced_schema: None,
            referenced_table: "person".into(),
            referenced_columns: vec![None],
        });
        let ddl = create_table_sql(Dialect::Sqlite, &meta, &TableExtras::default());
        assert!(ddl
            .sql
            .contains("FOREIGN KEY (\"owner_id\") REFERENCES \"person\"\n"));
    }

    #[test]
    fn composite_primary_key_keeps_key_order() {
        let mut a = column("a", "integer", false);
        a.primary_key_position = Some(2);
        let mut b = column("b", "integer", false);
        b.primary_key_position = Some(1);
        let meta = table(None, "t", vec![a, b]);
        let ddl = create_table_sql(Dialect::Sqlite, &meta, &TableExtras::default());
        assert!(ddl.sql.contains("PRIMARY KEY (\"b\", \"a\")"));
    }

    #[test]
    fn sqlserver_index_renders_clustering_direction_include_and_filter() {
        let meta = table(Some("dbo"), "orders", vec![]);
        let index = IndexMeta {
            name: "IX_orders_open".into(),
            unique: true,
            partial: true,
            columns: vec!["created_at".into()],
        };
        let extras = IndexExtras {
            key_columns: vec!["\"created_at\" DESC".into()],
            included_columns: vec!["\"total\"".into()],
            filter: Some("([status]='open')".into()),
            clustered: false,
            caveats: Vec::new(),
        };
        let ddl = create_index_sql(Dialect::SqlServer, &meta, &index, &extras);
        assert_eq!(
            ddl.sql,
            "CREATE UNIQUE NONCLUSTERED INDEX \"IX_orders_open\" ON \"dbo\".\"orders\" \
             (\"created_at\" DESC) INCLUDE (\"total\") WHERE ([status]='open');"
        );
        assert!(ddl.caveats.is_empty());
    }

    #[test]
    fn clustered_sqlserver_index_says_so() {
        let meta = table(Some("dbo"), "orders", vec![]);
        let index = IndexMeta {
            name: "CIX_orders".into(),
            unique: false,
            partial: false,
            columns: vec!["id".into()],
        };
        let extras = IndexExtras {
            key_columns: vec!["\"id\" ASC".into()],
            clustered: true,
            ..IndexExtras::default()
        };
        let ddl = create_index_sql(Dialect::SqlServer, &meta, &index, &extras);
        assert_eq!(
            ddl.sql,
            "CREATE CLUSTERED INDEX \"CIX_orders\" ON \"dbo\".\"orders\" (\"id\" ASC);"
        );
    }

    #[test]
    fn partial_index_without_a_predicate_is_flagged_loudly() {
        // Emitting this as a full index would silently widen a uniqueness
        // guarantee — the caveat must name it.
        let meta = table(None, "t", vec![]);
        let index = IndexMeta {
            name: "idx".into(),
            unique: true,
            partial: true,
            columns: vec!["a".into()],
        };
        let ddl = create_index_sql(Dialect::Sqlite, &meta, &index, &IndexExtras::default());
        assert_eq!(ddl.sql, "CREATE UNIQUE INDEX \"idx\" ON \"t\" (\"a\");");
        assert!(ddl.caveats.iter().any(|c| c.contains("partial/filtered")));
        assert!(ddl.caveats.iter().any(|c| c.contains("sort direction")));
        // No CLUSTERED keyword outside SQL Server.
        assert!(!ddl.sql.contains("CLUSTERED"));
    }

    #[test]
    fn view_wrapper_matches_the_object_kind_and_terminates_once() {
        let mut meta = table(Some("public"), "v", vec![]);
        meta.kind = TableKind::View;
        assert_eq!(
            create_view_sql(&meta, " SELECT 1;\n"),
            "CREATE VIEW \"public\".\"v\" AS\n SELECT 1;"
        );
        // A body without the server's semicolon still gets one.
        assert_eq!(
            create_view_sql(&meta, "SELECT 1"),
            "CREATE VIEW \"public\".\"v\" AS\nSELECT 1;"
        );
        meta.kind = TableKind::MaterializedView;
        assert!(create_view_sql(&meta, "SELECT 1")
            .starts_with("CREATE MATERIALIZED VIEW \"public\".\"v\" AS"));
    }
}
