//! The Schema pane (FRE-69): the selected table's structure, alongside Data
//! and SQL.
//!
//! Everything here comes from the introspection the sidebar already
//! triggered — no backend work of its own — so switching to this pane is
//! instant and works offline of the connection. It replaces the sidebar's
//! expandable per-table tree, which showed the same facts in a space too
//! narrow for them.

use dioxus::prelude::*;

use crate::db::{ColumnMeta, ConnectionId, Generated, TableKind, TableMeta, TypeDetail};

use super::notice::{Banner, BannerKind, DelayedLoading, EmptyState};
use super::state::{AppState, SchemaLoad, TableRef};

/// Structure of one table or view: its columns, and (tables only) indexes.
#[component]
pub fn SchemaPane(id: ConnectionId, table: TableRef) -> Element {
    let state = use_context::<AppState>();
    let schema = state
        .schemas
        .read()
        .get(&id)
        .cloned()
        .unwrap_or(SchemaLoad::Loading);
    let meta: Option<TableMeta> = match &schema {
        SchemaLoad::Ready(tables) => tables
            .iter()
            .find(|t| t.name == table.name && t.schema == table.schema)
            .cloned(),
        _ => None,
    };
    rsx! {
        div { class: "min-h-0 flex-1 overflow-auto",
            match schema {
                SchemaLoad::Loading => rsx! {
                    DelayedLoading { label: "Loading schema…" }
                },
                SchemaLoad::Failed(err) => rsx! {
                    div { class: "p-3",
                        Banner { kind: BannerKind::Error, message: err }
                    }
                },
                SchemaLoad::Ready(_) => match meta {
                    // The selected table vanished from a reloaded schema
                    // (dropped or renamed underneath us).
                    None => rsx! {
                        EmptyState {
                            icon: rsx! { TableIcon {} },
                            title: "This table is no longer in the schema",
                            hint: "It may have been dropped or renamed; reload the schema.",
                        }
                    },
                    Some(meta) => rsx! {
                        SchemaBody { meta }
                    },
                },
            }
        }
    }
}

/// A neutral glyph for the pane's empty state; the sidebar's own icons are
/// Lucide, so this stays consistent with them.
#[component]
fn TableIcon() -> Element {
    rsx! {
        dioxus_icons::lucide::Table2 { size: 40 }
    }
}

#[component]
fn SchemaBody(meta: TableMeta) -> Element {
    let qualified = match &meta.schema {
        Some(schema) => format!("{schema}.{}", meta.name),
        None => meta.name.clone(),
    };
    // Plain views have no indexes, so the section (and its "No indexes."
    // line) would just be noise. Materialized views are indexed on purpose,
    // and SQL Server indexed views report them as well, so anything that
    // actually has indexes shows them whatever its kind.
    let show_indexes = meta.kind == TableKind::Table || !meta.indexes.is_empty();
    rsx! {
        div { class: "p-4",
            div { class: "mb-3 flex items-baseline gap-2",
                h2 { class: "font-mono text-sm text-slate-900 dark:text-slate-200", "{qualified}" }
                match meta.kind {
                    TableKind::View => rsx! {
                        span { class: "rounded bg-violet-100 dark:bg-violet-900/50 px-1 text-xs text-violet-700 dark:text-violet-300",
                            "view"
                        }
                    },
                    TableKind::MaterializedView => rsx! {
                        span { class: "rounded bg-fuchsia-100 dark:bg-fuchsia-900/50 px-1 text-xs text-fuchsia-700 dark:text-fuchsia-300",
                            "matview"
                        }
                    },
                    TableKind::Table => rsx! {},
                }
            }
            table { class: "w-full border-collapse text-left",
                thead {
                    tr { class: "border-b border-slate-300 dark:border-slate-700",
                        th { class: "px-3 py-1.5 text-xs uppercase tracking-wide text-slate-500", "Column" }
                        th { class: "px-3 py-1.5 text-xs uppercase tracking-wide text-slate-500", "Type" }
                        th { class: "px-3 py-1.5 text-xs uppercase tracking-wide text-slate-500", "Null" }
                        th { class: "px-3 py-1.5 text-xs uppercase tracking-wide text-slate-500", "Key" }
                        th { class: "px-3 py-1.5 text-xs uppercase tracking-wide text-slate-500", "Default" }
                    }
                }
                tbody {
                    for column in meta.columns.clone() {
                        ColumnRow { key: "{column.name}", column }
                    }
                }
            }
            if show_indexes {
                h3 { class: "mt-5 mb-2 text-xs uppercase tracking-wide text-slate-500", "Indexes" }
                if meta.indexes.is_empty() {
                    p { class: "px-3 text-xs text-slate-500 dark:text-slate-500", "No indexes." }
                } else {
                    ul { class: "flex flex-col gap-1",
                        for index in meta.indexes.clone() {
                            li { class: "flex items-baseline gap-2 px-3 py-0.5 text-xs",
                                span { class: "truncate font-mono text-slate-900 dark:text-slate-300", "{index.name}" }
                                span { class: "font-mono text-slate-500",
                                    "({index.columns.join(\", \")})"
                                }
                                if index.unique {
                                    span { class: "rounded bg-emerald-100 dark:bg-emerald-900/50 px-1 text-emerald-700 dark:text-emerald-300",
                                        "unique"
                                    }
                                }
                                if index.partial {
                                    span { class: "rounded bg-slate-200 dark:bg-slate-800 px-1 text-slate-500 dark:text-slate-400",
                                        "partial"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn ColumnRow(column: ColumnMeta) -> Element {
    let type_name = display_type(&column);
    rsx! {
        tr { class: "border-b border-slate-200 dark:border-slate-800/60",
            td { class: "px-3 py-1 font-mono text-xs text-slate-900 dark:text-slate-300", "{column.name}" }
            td { class: "px-3 py-1 font-mono text-xs text-slate-500", "{type_name}" }
            td { class: "px-3 py-1 text-xs text-slate-500",
                if column.nullable { "yes" } else { "no" }
            }
            td { class: "px-3 py-1 text-xs",
                if let Some(position) = column.primary_key_position {
                    span { class: "rounded bg-amber-100 dark:bg-amber-900/50 px-1 text-amber-700 dark:text-amber-300",
                        // The position only matters for composite keys, so a
                        // single-column key stays an unadorned "PK".
                        if position > 1 { "PK {position}" } else { "PK" }
                    }
                }
            }
            td { class: "px-3 py-1 font-mono text-xs text-slate-500",
                // Identity and generated columns carry no literal default;
                // saying so beats an empty cell.
                match column.generated {
                    Generated::Always => rsx! {
                        span { class: "italic", "generated always" }
                    },
                    Generated::ByDefault => rsx! {
                        span { class: "italic", "identity" }
                    },
                    Generated::Never => rsx! {
                        div { class: "max-w-xs truncate",
                            title: column.default.clone().unwrap_or_default(),
                            {column.default.clone().unwrap_or_default()}
                        }
                    },
                }
            }
        }
    }
}

/// The type to show for a column. Postgres reports enum and array columns as
/// the opaque `USER-DEFINED` / `ARRAY`, so the real name introspected for the
/// editors (FRE-71) is shown instead — this pane is where a reader most wants
/// it.
fn display_type(column: &ColumnMeta) -> String {
    match &column.type_detail {
        TypeDetail::Enum { type_ref, .. } => type_ref.name.clone(),
        // pg_catalog names an array type `_elem`; `elem[]` is how it is
        // written everywhere else.
        TypeDetail::Array { type_ref } => match type_ref.name.strip_prefix('_') {
            Some(element) => format!("{element}[]"),
            None => type_ref.name.clone(),
        },
        TypeDetail::Plain if column.type_name.is_empty() => "any".to_string(),
        TypeDetail::Plain => column.type_name.to_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::TypeRef;

    fn column(name: &str, type_name: &str, detail: TypeDetail) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            type_name: type_name.into(),
            nullable: true,
            primary_key_position: None,
            default: None,
            generated: Generated::Never,
            type_detail: detail,
        }
    }

    #[test]
    fn display_type_prefers_the_real_name_over_the_opaque_one() {
        // An enum/array column's data_type says nothing; show what it is.
        assert_eq!(
            display_type(&column(
                "feeling",
                "USER-DEFINED",
                TypeDetail::Enum {
                    type_ref: TypeRef {
                        schema: "public".into(),
                        name: "mood".into(),
                    },
                    variants: vec!["sad".into()],
                }
            )),
            "mood"
        );
        assert_eq!(
            display_type(&column(
                "tags",
                "ARRAY",
                TypeDetail::Array {
                    type_ref: TypeRef {
                        schema: "pg_catalog".into(),
                        name: "_text".into(),
                    }
                }
            )),
            "text[]"
        );
    }

    #[test]
    fn display_type_lowercases_declared_types_and_names_the_untyped() {
        assert_eq!(
            display_type(&column("id", "INTEGER", TypeDetail::Plain)),
            "integer"
        );
        // SQLite columns may carry no declared type at all.
        assert_eq!(display_type(&column("x", "", TypeDetail::Plain)), "any");
    }
}
