//! Schema edits for the operations dialects agree on (FRE-122).
//!
//! Deliberately narrow. DDL is where engines diverge most, and the divergence
//! is not cosmetic: SQLite cannot change a column's type at all, every backend
//! spells constraint editing differently, and each engine the "Broader database
//! support" milestone adds multiplies both the generation and the tests. A
//! generator that emits valid SQL on one backend and silently wrong SQL on
//! another is worse than no generator, so this covers only [`SchemaOp`] — the
//! operations whose syntax is near-identical everywhere — and Show DDL
//! ([`ddl`](super::ddl)) plus the script tab serve everyone else.
//!
//! Three rules hold this together:
//!
//! - **Everything here is a pure function over plain data.** No pool, no I/O.
//!   What gets run and what gets refused are both decided by functions a test
//!   can execute directly, rather than by the shape of a component tree.
//! - **The generated statement is shown before it runs, and is editable.** So
//!   nothing here is the last word on what reaches the server:
//!   [`script_refusal`](super::script::script_refusal) re-checks the text that
//!   is actually run against the same capabilities. This module decides what to
//!   *offer*; that one decides what may *execute*.
//! - **Where a dialect genuinely differs, it says so** ([`SchemaOp::note`])
//!   rather than pretending. SQLite has no `TRUNCATE`, and the honest thing is
//!   to show the `DELETE FROM` that replaces it — not to hide it behind a
//!   button labelled truncate.

use super::caps::Capabilities;
use super::schema::{TableKind, TableMeta};
use super::script::script_refusal;
use super::sql::{qualified, quote_ident, Dialect};

/// One schema change, as the dialog collects it.
///
/// The set is closed on purpose — see the module docs. Changing a column's
/// type, nullability or default, editing constraints, and creating a table from
/// scratch are all deliberately absent; if one turns out to be needed it wants
/// its own issue and its own per-dialect test plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaOp {
    CreateIndex {
        name: String,
        /// Key columns in index order. Every entry must name a column of the
        /// table — the dialog only offers the table's own columns.
        columns: Vec<String>,
        unique: bool,
    },
    DropIndex {
        name: String,
    },
    /// Adds a **nullable column with no default** — the one `ADD COLUMN` form
    /// every backend supports identically, and the only one that cannot fail
    /// on an existing row.
    AddColumn {
        name: String,
        /// The declared type, as the user typed it. Dialect-specific by
        /// nature, which is why it is their text rather than a generated
        /// guess, and why it is shown before it runs.
        type_name: String,
    },
    RenameTable {
        new_name: String,
    },
    RenameColumn {
        column: String,
        new_name: String,
    },
    DropTable,
    /// Empties the table. On SQLite this is a `DELETE FROM` — see
    /// [`SchemaOp::note`].
    Truncate,
}

/// Why an operation cannot be offered on this object or connection, when it
/// cannot be. The sentences a disabled action shows.
pub const NOT_A_TABLE: &str = "Schema editing is offered for tables, not views.";

impl SchemaOp {
    /// The label the UI shows for this operation.
    pub fn label(&self) -> &'static str {
        match self {
            SchemaOp::CreateIndex { .. } => "Create index",
            SchemaOp::DropIndex { .. } => "Drop index",
            SchemaOp::AddColumn { .. } => "Add column",
            SchemaOp::RenameTable { .. } => "Rename table",
            SchemaOp::RenameColumn { .. } => "Rename column",
            SchemaOp::DropTable => "Drop table",
            SchemaOp::Truncate => "Truncate",
        }
    }

    /// Whether this operation destroys data that no rollback of the statement
    /// itself brings back — the two that need a confirmation proportional to
    /// the damage rather than a plain "Run".
    ///
    /// `DROP INDEX` is deliberately **not** destructive by this definition: an
    /// index is derived data, and recreating it costs time rather than
    /// information. `RENAME` and `ADD COLUMN` change the schema but destroy
    /// nothing.
    pub fn destroys_data(&self) -> bool {
        matches!(self, SchemaOp::DropTable | SchemaOp::Truncate)
    }

    /// What this operation does differently on `dialect` than its name
    /// suggests, or `None` when it does exactly what it says.
    ///
    /// Shown beside the generated SQL. The point is that the difference is
    /// visible *before* running rather than discovered afterwards.
    pub fn note(&self, dialect: Dialect) -> Option<&'static str> {
        match (self, dialect) {
            (SchemaOp::Truncate, Dialect::Sqlite) => Some(
                "SQLite has no TRUNCATE. This deletes every row instead — which SQLite \
                 optimises the same way, but AUTOINCREMENT counters keep their values.",
            ),
            (SchemaOp::RenameTable { .. } | SchemaOp::RenameColumn { .. }, Dialect::SqlServer) => {
                Some(
                    "SQL Server renames through sp_rename, which takes the new name unqualified \
                     and leaves references to the old name (views, procedures) pointing at a \
                     name that no longer exists.",
                )
            }
            _ => None,
        }
    }
}

/// What is wrong with the operation as filled in, or `None` when it is
/// complete. Names are the user's, so they are checked for being *present*
/// rather than for being valid identifiers — quoting handles the rest, and a
/// name the server rejects fails with the server's own message.
pub fn op_problem(op: &SchemaOp) -> Option<&'static str> {
    let blank = |s: &String| s.trim().is_empty();
    match op {
        SchemaOp::CreateIndex { name, columns, .. } => {
            if blank(name) {
                Some("The index needs a name.")
            } else if columns.is_empty() {
                Some("Choose at least one column to index.")
            } else {
                None
            }
        }
        SchemaOp::DropIndex { name } => blank(name).then_some("No index chosen."),
        SchemaOp::AddColumn { name, type_name } => {
            if blank(name) {
                Some("The column needs a name.")
            } else if blank(type_name) {
                Some("The column needs a type.")
            } else {
                None
            }
        }
        SchemaOp::RenameTable { new_name } => blank(new_name).then_some("The new name is empty."),
        SchemaOp::RenameColumn { column, new_name } => {
            if blank(column) {
                Some("No column chosen.")
            } else if blank(new_name) {
                Some("The new name is empty.")
            } else {
                None
            }
        }
        SchemaOp::DropTable | SchemaOp::Truncate => None,
    }
}

/// Why `op` is not offered on this object through this connection, or `None`
/// when it is.
///
/// `caps` must be the connection's **effective** capabilities — the backend's
/// narrowed by the user's write protection (FRE-111), i.e.
/// [`Connection::capabilities`](super::registry::Connection::capabilities).
/// Reading the backend's own answer here would offer schema edits on a
/// connection the user marked read-only.
///
/// **The capability half is delegated to
/// [`script_refusal`](super::script::script_refusal), over the statement this
/// operation actually generates.** That is the same call the run path makes on
/// the text that finally reaches the server, so the disabled button and the
/// refused run cannot disagree — and cannot drift apart as either side gains a
/// case. Asking [`statement_needs`](super::script::statement_needs) about the
/// generated text rather than declaring the answer per variant also gets the
/// dialect differences for free: SQLite's truncate is a `DELETE`, and is
/// charged for changing rows rather than for changing the schema, which is what
/// it does.
///
/// Object-level [`TableAccess`](super::caps::TableAccess) is deliberately *not*
/// consulted: it narrows `mutate` for reasons about addressing one row — a
/// table with no primary key, a view's rows having no identity — and none of
/// those bear on whether the table can be dropped or emptied. What does bear on
/// it is the object's *kind*, which is checked here.
pub fn schema_edit_refusal(
    caps: Capabilities,
    dialect: Dialect,
    table: &TableMeta,
    op: &SchemaOp,
) -> Option<&'static str> {
    if table.kind != TableKind::Table {
        return Some(NOT_A_TABLE);
    }
    let statements = [schema_op_sql(dialect, table, op)];
    script_refusal(caps, &statements, dialect).map(|(_, reason)| reason)
}

/// The statement `op` generates against `table` on `dialect`, terminated.
///
/// Every identifier is quoted through [`quote_ident`], including the ones the
/// user typed: they are names, never SQL, and a name with a quote or a space in
/// it is legal on all three backends.
pub fn schema_op_sql(dialect: Dialect, table: &TableMeta, op: &SchemaOp) -> String {
    let object = qualified(table.schema.as_deref(), &table.name);
    match op {
        SchemaOp::CreateIndex {
            name,
            columns,
            unique,
        } => {
            let unique = if *unique { "UNIQUE " } else { "" };
            let columns: Vec<String> = columns.iter().map(|c| quote_ident(c)).collect();
            format!(
                "CREATE {unique}INDEX {} ON {object} ({});",
                quote_ident(name.trim()),
                columns.join(", ")
            )
        }
        // Where the index *lives* is the divergence. On SQLite and Postgres an
        // index is a schema object addressed on its own; on SQL Server it is
        // owned by its table and cannot be dropped without naming it.
        SchemaOp::DropIndex { name } => match dialect {
            Dialect::SqlServer => format!("DROP INDEX {} ON {object};", quote_ident(name)),
            Dialect::Sqlite => format!("DROP INDEX {};", quote_ident(name)),
            Dialect::Postgres => format!(
                "DROP INDEX {};",
                qualified(table.schema.as_deref(), name.trim())
            ),
        },
        SchemaOp::AddColumn { name, type_name } => {
            let name = quote_ident(name.trim());
            let type_name = type_name.trim();
            match dialect {
                // T-SQL's ADD takes no COLUMN keyword. The explicit NULL
                // states what the operation promises in the statement the user
                // reads, and pins it against the session's nullability
                // default.
                //
                // It was added to defend against `ANSI_NULL_DFLT_OFF`, on the
                // documented rule that a column added without stated
                // nullability is then NOT NULL — which would fail outright on a
                // table with rows. That does **not** reproduce: on SQL Server
                // 2022 (16.0.4265) the setting changes what `CREATE TABLE`
                // makes of a bare column and leaves `ALTER TABLE … ADD`
                // nullable either way (`sqlserver_add_column_ignores_the_session_null_default`
                // pins both halves). The keyword stays because it says what is
                // meant; the justification is corrected rather than left
                // standing as something nothing checks.
                Dialect::SqlServer => format!("ALTER TABLE {object} ADD {name} {type_name} NULL;"),
                Dialect::Sqlite | Dialect::Postgres => {
                    format!("ALTER TABLE {object} ADD COLUMN {name} {type_name};")
                }
            }
        }
        SchemaOp::RenameTable { new_name } => {
            let new_name = new_name.trim();
            match dialect {
                Dialect::SqlServer => format!(
                    "EXEC sp_rename {}, {};",
                    string_literal(&sp_rename_target(table.schema.as_deref(), &[&table.name])),
                    string_literal(new_name)
                ),
                // Both spell it the same way, and both take the new name
                // unqualified: the table stays in the schema it is in.
                Dialect::Sqlite | Dialect::Postgres => {
                    format!("ALTER TABLE {object} RENAME TO {};", quote_ident(new_name))
                }
            }
        }
        SchemaOp::RenameColumn { column, new_name } => {
            let new_name = new_name.trim();
            match dialect {
                Dialect::SqlServer => format!(
                    "EXEC sp_rename {}, {}, 'COLUMN';",
                    string_literal(&sp_rename_target(
                        table.schema.as_deref(),
                        &[&table.name, column]
                    )),
                    string_literal(new_name)
                ),
                Dialect::Sqlite | Dialect::Postgres => format!(
                    "ALTER TABLE {object} RENAME COLUMN {} TO {};",
                    quote_ident(column),
                    quote_ident(new_name)
                ),
            }
        }
        SchemaOp::DropTable => format!("DROP TABLE {object};"),
        SchemaOp::Truncate => match dialect {
            // SQLite has no TRUNCATE at all; DELETE with no WHERE is what it
            // offers, and `SchemaOp::note` says so rather than leaving the
            // reader to notice the keyword changed.
            Dialect::Sqlite => format!("DELETE FROM {object};"),
            Dialect::Postgres | Dialect::SqlServer => format!("TRUNCATE TABLE {object};"),
        },
    }
}

/// The dotted name `sp_rename` takes for its first argument, bracket-quoted.
///
/// **Brackets rather than the `"…"` used everywhere else in this crate.**
/// `sp_rename` parses this out of a *string*, and how it reads a `"` there
/// depends on the session's QUOTED_IDENTIFIER setting — brackets do not depend
/// on anything. `]` is escaped by doubling, exactly as `"` is in
/// [`quote_ident`].
fn sp_rename_target(schema: Option<&str>, parts: &[&str]) -> String {
    let mut out = String::new();
    for part in schema.iter().copied().chain(parts.iter().copied()) {
        if !out.is_empty() {
            out.push('.');
        }
        out.push('[');
        out.push_str(&part.replace(']', "]]"));
        out.push(']');
    }
    out
}

/// A SQL string literal, single quotes doubled. Only `sp_rename` needs one:
/// every other statement here passes identifiers as identifiers.
fn string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::caps::{WriteProtection, NO_DDL, NO_MUTATE};
    use crate::db::schema::{ColumnMeta, Generated, IndexMeta, TypeDetail};
    use crate::db::script::{split_statements, statement_needs};

    const DIALECTS: [Dialect; 3] = [Dialect::Sqlite, Dialect::Postgres, Dialect::SqlServer];

    fn table(schema: Option<&str>, name: &str) -> TableMeta {
        TableMeta {
            schema: schema.map(str::to_string),
            name: name.into(),
            kind: TableKind::Table,
            columns: vec![ColumnMeta {
                name: "id".into(),
                type_name: "int".into(),
                nullable: false,
                primary_key_position: Some(1),
                default: None,
                generated: Generated::Never,
                type_detail: TypeDetail::Plain,
            }],
            indexes: vec![IndexMeta {
                name: "idx_t_id".into(),
                unique: false,
                partial: false,
                columns: vec!["id".into()],
            }],
            foreign_keys: vec![],
            restriction: None,
            internal: None,
            kind_label: None,
        }
    }

    /// Every operation, filled in so `op_problem` is happy — the list the
    /// cross-cutting properties below iterate, so a new variant joins them by
    /// being added here.
    fn every_op() -> Vec<SchemaOp> {
        vec![
            SchemaOp::CreateIndex {
                name: "idx_new".into(),
                columns: vec!["id".into()],
                unique: false,
            },
            SchemaOp::CreateIndex {
                name: "idx_new".into(),
                columns: vec!["id".into()],
                unique: true,
            },
            SchemaOp::DropIndex {
                name: "idx_t_id".into(),
            },
            SchemaOp::AddColumn {
                name: "note".into(),
                type_name: "text".into(),
            },
            SchemaOp::RenameTable {
                new_name: "t2".into(),
            },
            SchemaOp::RenameColumn {
                column: "id".into(),
                new_name: "ident".into(),
            },
            SchemaOp::DropTable,
            SchemaOp::Truncate,
        ]
    }

    #[test]
    fn create_index_is_the_same_statement_on_every_dialect() {
        let meta = table(Some("app"), "orders");
        let op = SchemaOp::CreateIndex {
            name: "idx_orders_id".into(),
            columns: vec!["id".into(), "created at".into()],
            unique: true,
        };
        for dialect in DIALECTS {
            assert_eq!(
                schema_op_sql(dialect, &meta, &op),
                "CREATE UNIQUE INDEX \"idx_orders_id\" ON \"app\".\"orders\" \
                 (\"id\", \"created at\");",
                "{dialect:?}"
            );
        }
        // Non-unique drops only the keyword.
        let plain = SchemaOp::CreateIndex {
            name: "i".into(),
            columns: vec!["id".into()],
            unique: false,
        };
        assert_eq!(
            schema_op_sql(Dialect::Postgres, &meta, &plain),
            "CREATE INDEX \"i\" ON \"app\".\"orders\" (\"id\");"
        );
    }

    #[test]
    fn dropping_an_index_names_its_table_only_where_the_dialect_needs_it() {
        let meta = table(Some("app"), "orders");
        let op = SchemaOp::DropIndex {
            name: "idx_t_id".into(),
        };
        // SQL Server: an index belongs to its table and cannot be addressed
        // without it. Dropping the ON clause here is a syntax error, not a
        // difference in style.
        assert_eq!(
            schema_op_sql(Dialect::SqlServer, &meta, &op),
            "DROP INDEX \"idx_t_id\" ON \"app\".\"orders\";"
        );
        // Postgres: a schema object, qualified by the table's schema — an
        // index lives in the same schema as its table.
        assert_eq!(
            schema_op_sql(Dialect::Postgres, &meta, &op),
            "DROP INDEX \"app\".\"idx_t_id\";"
        );
        // SQLite has no schemas here at all.
        assert_eq!(
            schema_op_sql(Dialect::Sqlite, &table(None, "orders"), &op),
            "DROP INDEX \"idx_t_id\";"
        );
    }

    #[test]
    fn add_column_uses_each_dialects_accepted_spelling() {
        let meta = table(Some("dbo"), "orders");
        let op = SchemaOp::AddColumn {
            name: "note".into(),
            type_name: "nvarchar(200)".into(),
        };
        // T-SQL rejects the COLUMN keyword; the explicit NULL states the
        // nullability the operation promises.
        assert_eq!(
            schema_op_sql(Dialect::SqlServer, &meta, &op),
            "ALTER TABLE \"dbo\".\"orders\" ADD \"note\" nvarchar(200) NULL;"
        );
        for dialect in [Dialect::Sqlite, Dialect::Postgres] {
            assert_eq!(
                schema_op_sql(dialect, &meta, &op),
                "ALTER TABLE \"dbo\".\"orders\" ADD COLUMN \"note\" nvarchar(200);",
                "{dialect:?}"
            );
        }
    }

    #[test]
    fn renames_go_through_sp_rename_only_on_sqlserver() {
        let meta = table(Some("dbo"), "orders");
        let rename_table = SchemaOp::RenameTable {
            new_name: "invoices".into(),
        };
        let rename_column = SchemaOp::RenameColumn {
            column: "id".into(),
            new_name: "ident".into(),
        };
        assert_eq!(
            schema_op_sql(Dialect::SqlServer, &meta, &rename_table),
            "EXEC sp_rename '[dbo].[orders]', 'invoices';"
        );
        assert_eq!(
            schema_op_sql(Dialect::SqlServer, &meta, &rename_column),
            "EXEC sp_rename '[dbo].[orders].[id]', 'ident', 'COLUMN';"
        );
        for dialect in [Dialect::Sqlite, Dialect::Postgres] {
            assert_eq!(
                schema_op_sql(dialect, &meta, &rename_table),
                "ALTER TABLE \"dbo\".\"orders\" RENAME TO \"invoices\";",
                "{dialect:?}"
            );
            assert_eq!(
                schema_op_sql(dialect, &meta, &rename_column),
                "ALTER TABLE \"dbo\".\"orders\" RENAME COLUMN \"id\" TO \"ident\";",
                "{dialect:?}"
            );
        }
    }

    #[test]
    fn sp_rename_quotes_with_brackets_and_escapes_both_delimiters() {
        // The name reaches sp_rename inside a string literal, so a quote in it
        // would end the literal and everything after it would be parsed as
        // T-SQL. A `]` would end the bracket-quoted part the same way.
        let mut meta = table(Some("we'ird"), "od]d");
        meta.columns[0].name = "a'b]c".into();
        let op = SchemaOp::RenameColumn {
            column: "a'b]c".into(),
            new_name: "plain".into(),
        };
        assert_eq!(
            schema_op_sql(Dialect::SqlServer, &meta, &op),
            "EXEC sp_rename '[we''ird].[od]]d].[a''b]]c]', 'plain', 'COLUMN';"
        );
        // The new name is a literal too — a quote in it must not escape.
        let op = SchemaOp::RenameTable {
            new_name: "it's".into(),
        };
        assert!(schema_op_sql(Dialect::SqlServer, &meta, &op).ends_with("'it''s';"));
    }

    #[test]
    fn truncate_falls_back_to_delete_on_sqlite_and_says_so() {
        let meta = table(None, "orders");
        assert_eq!(
            schema_op_sql(Dialect::Sqlite, &meta, &SchemaOp::Truncate),
            "DELETE FROM \"orders\";"
        );
        // …and the substitution is stated, not hidden behind the button's
        // label. This is the whole reason the note exists.
        let note = SchemaOp::Truncate.note(Dialect::Sqlite).unwrap();
        assert!(note.contains("no TRUNCATE"), "{note}");
        for dialect in [Dialect::Postgres, Dialect::SqlServer] {
            assert_eq!(
                schema_op_sql(dialect, &meta, &SchemaOp::Truncate),
                "TRUNCATE TABLE \"orders\";",
                "{dialect:?}"
            );
            assert_eq!(SchemaOp::Truncate.note(dialect), None, "{dialect:?}");
        }
    }

    #[test]
    fn drop_table_is_the_same_everywhere() {
        for dialect in DIALECTS {
            assert_eq!(
                schema_op_sql(dialect, &table(Some("s"), "t"), &SchemaOp::DropTable),
                "DROP TABLE \"s\".\"t\";",
                "{dialect:?}"
            );
        }
    }

    #[test]
    fn identifiers_are_quoted_even_when_the_user_typed_them() {
        // A name is a name, never SQL. Without quoting, a column called
        // `x); DROP TABLE t; --` would be exactly that.
        let meta = table(None, "t");
        let op = SchemaOp::AddColumn {
            name: "x\"; DROP TABLE t; --".into(),
            type_name: "text".into(),
        };
        let sql = schema_op_sql(Dialect::Sqlite, &meta, &op);
        assert_eq!(
            sql,
            "ALTER TABLE \"t\" ADD COLUMN \"x\"\"; DROP TABLE t; --\" text;"
        );
        // One statement, not two: the splitter sees the `;` as identifier text.
        assert_eq!(split_statements(&sql, Dialect::Sqlite).len(), 1);
    }

    #[test]
    fn names_are_trimmed_so_a_stray_space_is_not_part_of_the_identifier() {
        let meta = table(None, "t");
        let op = SchemaOp::AddColumn {
            name: "  note  ".into(),
            type_name: "  text  ".into(),
        };
        assert_eq!(
            schema_op_sql(Dialect::Sqlite, &meta, &op),
            "ALTER TABLE \"t\" ADD COLUMN \"note\" text;"
        );
    }

    /// Every combination of the two capabilities the gate can turn on.
    /// `read_query` stays true throughout: a connection that cannot query at
    /// all has no schema pane to press a button in.
    fn capability_matrix() -> Vec<Capabilities> {
        let mut out = Vec::new();
        for mutate in [true, false] {
            for ddl in [true, false] {
                out.push(Capabilities {
                    mutate,
                    ddl,
                    ..Capabilities::FULL
                });
            }
        }
        out
    }

    #[test]
    fn nothing_is_offered_that_the_run_path_would_then_refuse() {
        // The property the whole gate exists for. The button asks about an
        // *operation*; the run path asks about the *text* — including text the
        // user has since edited. Those are different questions over different
        // inputs, and an operation offered by the first and refused by the
        // second is a button that does nothing but produce an error.
        //
        // Delegation makes this true by construction today. It is asserted
        // anyway because the construction is what a future edit would change:
        // the moment the gate grows a rule of its own, this fails.
        let meta = table(Some("s"), "t");
        for caps in capability_matrix() {
            for dialect in DIALECTS {
                for op in every_op() {
                    let sql = schema_op_sql(dialect, &meta, &op);
                    let offered = schema_edit_refusal(caps, dialect, &meta, &op).is_none();
                    let runnable =
                        script_refusal(caps, std::slice::from_ref(&sql), dialect).is_none();
                    assert_eq!(
                        offered, runnable,
                        "{caps:?} {dialect:?} {op:?}: offered={offered} runnable={runnable}: {sql}"
                    );
                }
            }
        }
    }

    #[test]
    fn emptying_a_table_needs_the_capability_to_change_rows() {
        // TRUNCATE is DDL by classification, so a gate that only asked about
        // `ddl` would let it wipe every row on a connection where a plain
        // DELETE is refused. On SQLite it *is* a DELETE, and is charged
        // accordingly — the same protection reached by the other route.
        let meta = table(None, "t");
        let no_mutate = Capabilities {
            mutate: false,
            ..Capabilities::FULL
        };
        for dialect in DIALECTS {
            assert_eq!(
                schema_edit_refusal(no_mutate, dialect, &meta, &SchemaOp::Truncate),
                Some(NO_MUTATE),
                "{dialect:?}"
            );
            // And the statement itself is charged for it, wherever it lands.
            assert!(
                statement_needs(&schema_op_sql(dialect, &meta, &SchemaOp::Truncate), dialect)
                    .mutate,
                "{dialect:?}"
            );
        }
    }

    #[test]
    fn a_connection_marked_read_only_is_offered_nothing() {
        // What FRE-111 promises, over the resolution FRE-87 built: marking a
        // connection read-only narrows both capabilities, so every operation
        // here — schema-only ones included — is refused.
        let caps = WriteProtection::ReadOnly.apply(Capabilities::FULL);
        let meta = table(Some("s"), "t");
        for dialect in DIALECTS {
            for op in every_op() {
                assert!(
                    schema_edit_refusal(caps, dialect, &meta, &op).is_some(),
                    "{dialect:?} {op:?} was offered on a read-only connection"
                );
            }
        }
        // Confirm narrows nothing by design — it interposes a prompt instead,
        // which is the dialog's job rather than the gate's.
        let confirm = WriteProtection::Confirm.apply(Capabilities::FULL);
        for op in every_op() {
            assert_eq!(
                schema_edit_refusal(confirm, Dialect::Postgres, &meta, &op),
                None,
                "{op:?}"
            );
        }
    }

    #[test]
    fn every_generated_statement_is_exactly_one_statement() {
        // The dialog runs what is in its box through the script path, so a
        // generator that emitted two statements would silently make a
        // non-atomic edit on a backend without transactional DDL.
        let meta = table(Some("s"), "t");
        for dialect in DIALECTS {
            for op in every_op() {
                let sql = schema_op_sql(dialect, &meta, &op);
                assert_eq!(
                    split_statements(&sql, dialect).len(),
                    1,
                    "{dialect:?} {op:?}: {sql}"
                );
                assert!(sql.ends_with(';'), "{dialect:?} {op:?}: {sql}");
            }
        }
    }

    #[test]
    fn a_connection_without_ddl_refuses_every_schema_change() {
        let caps = Capabilities {
            ddl: false,
            ..Capabilities::FULL
        };
        let meta = table(Some("s"), "t");
        for dialect in DIALECTS {
            for op in every_op() {
                // SQLite's truncate is the one exception, and an honest one:
                // `DELETE FROM t` changes no schema, so refusing it for want of
                // the DDL capability would state a reason that isn't true. It
                // is still refused without `mutate` — see
                // `emptying_a_table_needs_the_capability_to_change_rows`.
                let expected = match (dialect, &op) {
                    (Dialect::Sqlite, SchemaOp::Truncate) => None,
                    _ => Some(NO_DDL),
                };
                assert_eq!(
                    schema_edit_refusal(caps, dialect, &meta, &op),
                    expected,
                    "{dialect:?} {op:?}"
                );
            }
        }
    }

    #[test]
    fn nothing_is_offered_on_a_view() {
        let mut meta = table(Some("s"), "t");
        for kind in [TableKind::View, TableKind::MaterializedView] {
            meta.kind = kind;
            for op in every_op() {
                assert_eq!(
                    schema_edit_refusal(Capabilities::FULL, Dialect::Postgres, &meta, &op),
                    Some(NOT_A_TABLE),
                    "{kind:?} {op:?}"
                );
            }
        }
        // A table on a fully capable connection is offered everything.
        meta.kind = TableKind::Table;
        for op in every_op() {
            assert_eq!(
                schema_edit_refusal(Capabilities::FULL, Dialect::Postgres, &meta, &op),
                None,
                "{op:?}"
            );
        }
    }

    #[test]
    fn an_incomplete_form_names_what_is_missing() {
        assert_eq!(
            op_problem(&SchemaOp::CreateIndex {
                name: "  ".into(),
                columns: vec!["id".into()],
                unique: false,
            }),
            Some("The index needs a name.")
        );
        assert_eq!(
            op_problem(&SchemaOp::CreateIndex {
                name: "i".into(),
                columns: vec![],
                unique: false,
            }),
            Some("Choose at least one column to index.")
        );
        assert_eq!(
            op_problem(&SchemaOp::AddColumn {
                name: "c".into(),
                type_name: String::new(),
            }),
            Some("The column needs a type.")
        );
        assert_eq!(
            op_problem(&SchemaOp::RenameColumn {
                column: "a".into(),
                new_name: " ".into(),
            }),
            Some("The new name is empty.")
        );
        // The two that need nothing filled in are always complete.
        for op in [SchemaOp::DropTable, SchemaOp::Truncate] {
            assert_eq!(op_problem(&op), None, "{op:?}");
        }
        for op in every_op() {
            assert_eq!(op_problem(&op), None, "{op:?} is filled in");
        }
    }

    #[test]
    fn only_the_operations_that_lose_data_are_treated_as_destructive() {
        for op in every_op() {
            let expected = matches!(op, SchemaOp::DropTable | SchemaOp::Truncate);
            assert_eq!(op.destroys_data(), expected, "{op:?}");
            // Dropping an index loses no information — recreating it costs
            // time, not data — so it must not demand the typed-name ritual.
            if matches!(op, SchemaOp::DropIndex { .. }) {
                assert!(!op.destroys_data());
            }
        }
    }
}
