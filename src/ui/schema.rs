//! The Schema pane (FRE-69): the selected table's structure, alongside Data
//! and SQL.
//!
//! Everything here comes from the introspection the sidebar already
//! triggered — no backend work of its own — so switching to this pane is
//! instant and works offline of the connection. It replaces the sidebar's
//! expandable per-table tree, which showed the same facts in a space too
//! narrow for them.

use dioxus::prelude::*;
use dioxus_icons::lucide::X;

use crate::db::{
    unreadable_reason, ColumnMeta, ConnectionId, DdlObject, DdlSource, Generated, RowCount,
    TableKind, TableMeta, TypeDetail,
};
use crate::util::{human_bytes, human_count};

use super::js::{copy_to_clipboard, focus_on_mount};
use super::notice::{Banner, BannerKind, DelayedLoading, EmptyState, KindBadge};
use super::state::{AppState, SchemaLoad, TableRef};

/// What the pane should show, derived from [`SchemaLoad`] inside one `read()`
/// scope. Cloning the whole `Ready(Vec<TableMeta>)` per render just to render
/// *one* table was the schema-pane half of FRE-134 — this carries only the
/// selected table's metadata out of the borrow (same shape as the sidebar's
/// `ListState`).
enum PaneState {
    Loading,
    Failed(String),
    /// The selected table vanished from a reloaded schema (dropped or
    /// renamed underneath us).
    Missing,
    Ready(TableMeta),
}

/// Structure of one table or view: its columns, and (tables only) indexes.
#[component]
pub fn SchemaPane(id: ConnectionId, table: TableRef) -> Element {
    let state = use_context::<AppState>();
    let pane = match state.schemas.read().get(&id) {
        Some(SchemaLoad::Failed(err)) => PaneState::Failed(err.clone()),
        Some(SchemaLoad::Ready(tables)) => tables
            .iter()
            .find(|t| t.name == table.name && t.schema == table.schema)
            .map(|meta| PaneState::Ready(meta.clone()))
            .unwrap_or(PaneState::Missing),
        Some(SchemaLoad::Loading) | None => PaneState::Loading,
    };
    rsx! {
        div { class: "min-h-0 flex-1 overflow-auto",
            match pane {
                PaneState::Loading => rsx! {
                    DelayedLoading { label: "Loading schema…" }
                },
                PaneState::Failed(err) => rsx! {
                    div { class: "p-3",
                        Banner { kind: BannerKind::Error, message: err }
                    }
                },
                PaneState::Missing => rsx! {
                    EmptyState {
                        icon: rsx! { TableIcon {} },
                        title: "This table is no longer in the schema",
                        hint: "It may have been dropped or renamed; reload the schema.",
                    }
                },
                PaneState::Ready(meta) => rsx! {
                    SchemaBody { id, meta }
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
fn SchemaBody(id: ConnectionId, meta: TableMeta) -> Element {
    let qualified = match &meta.schema {
        Some(schema) => format!("{schema}.{}", meta.name),
        None => meta.name.clone(),
    };
    let table = TableRef {
        schema: meta.schema.clone(),
        name: meta.name.clone(),
    };
    // Which object's DDL the overlay is showing, if any (FRE-108). Local to
    // the pane: closing it or switching table drops the request with it.
    let mut showing_ddl = use_signal(|| Option::<DdlObject>::None);
    // Plain views have no indexes, so the section (and its "No indexes."
    // line) would just be noise. Materialized views are indexed on purpose,
    // and SQL Server indexed views report them as well, so anything that
    // actually has indexes shows them whatever its kind.
    let show_indexes = meta.kind == TableKind::Table || !meta.indexes.is_empty();
    rsx! {
        div { class: "p-4",
            div { class: "mb-3 flex items-baseline gap-2",
                h2 { class: "font-mono text-sm text-slate-900 dark:text-slate-200", "{qualified}" }
                KindBadge { kind: meta.kind }
                // The object's own definition (FRE-108). Sits with the name
                // it describes rather than in a toolbar, so it is obvious
                // *what* the DDL will be for.
                button {
                    class: "ml-auto rounded border border-slate-300 dark:border-slate-700 px-1.5 py-0.5 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100",
                    onclick: move |_| showing_ddl.set(Some(DdlObject::Object)),
                    "Show DDL"
                }
            }
            // How big the thing is (FRE-118) — under the name it describes and
            // above the columns, since it is read far more often than any one
            // column. Withheld from objects with no rows at all (a RisingWave
            // sink, FRE-148): their emptiness is already stated by the grid,
            // and offering to count them would be offering a failure.
            if unreadable_reason(&meta).is_none() {
                TableStatsLine { id, table: table.clone() }
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
                                button {
                                    class: "ml-auto rounded border border-slate-300 dark:border-slate-700 px-1.5 py-0.5 text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100",
                                    onclick: {
                                        let name = index.name.clone();
                                        move |_| showing_ddl.set(Some(DdlObject::Index(name.clone())))
                                    },
                                    "DDL"
                                }
                            }
                        }
                    }
                }
            }
        }
        if let Some(object) = showing_ddl() {
            DdlOverlay {
                id,
                table: table.clone(),
                object,
                on_close: move |_| showing_ddl.set(None),
            }
        }
    }
}

/// The exact count to show for `current`, out of whatever was last counted.
///
/// `None` unless the stored count belongs to the table now on screen. The whole
/// point is the *inequality* case: a count is expensive, so it is kept after it
/// arrives, and the moment it is shown beside a different table it is a wrong
/// number carrying the one badge that promises it is right. Schema and name are
/// both compared, since the same table name in two schemas is two tables.
fn exact_for(
    counted: Option<&(TableRef, Result<u64, String>)>,
    current: &TableRef,
) -> Option<Result<u64, String>> {
    let (counted_table, result) = counted?;
    (counted_table == current).then(|| result.clone())
}

/// How big this table is (FRE-118): an estimated row count, its size on disk,
/// and the one action that turns the estimate into a real number.
///
/// The estimate loads on its own because it costs one catalog query; the exact
/// count never does, because it costs a scan. That asymmetry is the feature,
/// and it is why the two numbers arrive through different calls and render with
/// different badges — an estimate shown as though it were counted would be
/// worse than showing nothing.
///
/// Two independent defenses keep a number from outliving the table it
/// describes, and neither assumes the other:
///
///  - `table` is a [`ReadSignal`], so the estimate re-loads if this component
///    is ever re-rendered with a new table rather than remounted;
///  - the exact count is stored *with* the [`TableRef`] it counted, and
///    [`exact_for`] hands it back only for the table now on screen.
///
/// Today neither is what actually saves it: `SchemaPane` is keyed by
/// `table.key()` and `ConnectionView` by connection id (`src/ui/shell.rs`), so
/// a table or connection switch remounts this and resets both signals. That
/// keying is load-bearing and this does not rely on it — the two are written
/// so that removing either the key or the pairing still leaves the number
/// correct, because a stale count wearing the "exact" badge is the one failure
/// this whole feature is arranged to prevent. [`exact_for`] is the half that
/// can be tested without a renderer, and is.
#[component]
fn TableStatsLine(id: ConnectionId, table: ReadSignal<TableRef>) -> Element {
    let state = use_context::<AppState>();
    let stats = use_resource(move || {
        let table = table();
        async move { state.load_table_stats(id, table).await }
    });
    let mut counted = use_signal(|| Option::<(TableRef, Result<u64, String>)>::None);
    let mut counting = use_signal(|| false);

    let loaded = stats.read().clone();
    let exact = exact_for(counted.read().as_ref(), &table());
    let rows = match exact.as_ref().and_then(|r| r.as_ref().ok()) {
        Some(n) => Some(RowCount::Exact(*n)),
        None => loaded
            .as_ref()
            .and_then(|r| r.as_ref().ok())
            .and_then(|s| s.rows),
    };
    let bytes = loaded
        .as_ref()
        .and_then(|r| r.as_ref().ok())
        .and_then(|s| s.bytes);
    // Two independent failures, and they read differently: the estimate not
    // arriving is a shrug, a count the user asked for failing is an answer
    // they are owed.
    let stats_error = loaded.as_ref().and_then(|r| r.as_ref().err().cloned());
    let count_error = exact.as_ref().and_then(|r| r.as_ref().err().cloned());
    // The server answered, and answered nothing. Said once, plainly, rather
    // than rendered as the zeroes it is not.
    let nothing_known =
        loaded.is_some() && stats_error.is_none() && rows.is_none() && bytes.is_none();

    rsx! {
        div { class: "mb-4 flex items-baseline gap-2 text-xs text-slate-500 dark:text-slate-400",
            if let Some(rows) = rows {
                span { class: "shrink-0", title: rows.tooltip(),
                    // The tilde is the label a reader sees before any badge
                    // does its work, and it travels with the number.
                    if rows.is_estimate() {
                        "≈ "
                    }
                    "{human_count(rows.value())} rows"
                }
                span {
                    class: if rows.is_estimate() {
                        "shrink-0 rounded bg-amber-100 dark:bg-amber-900/50 px-1 text-amber-700 dark:text-amber-300"
                    } else {
                        "shrink-0 rounded bg-emerald-100 dark:bg-emerald-900/50 px-1 text-emerald-700 dark:text-emerald-300"
                    },
                    title: rows.tooltip(),
                    "{rows.label()}"
                }
            }
            if let Some(bytes) = bytes {
                if rows.is_some() {
                    span { class: "text-slate-400 dark:text-slate-600", "·" }
                }
                span { class: "shrink-0",
                    title: "Space the object occupies on disk, indexes and out-of-line storage included",
                    "{human_bytes(bytes)}"
                }
            }
            if nothing_known {
                span { class: "italic",
                    title: "This server keeps no row or size statistics for this object. Counting exactly still works.",
                    "No stored statistics"
                }
            }
            if let Some(err) = stats_error {
                span { class: "truncate italic text-amber-700 dark:text-amber-400", title: "{err}",
                    "Statistics unavailable"
                }
            }
            if let Some(err) = count_error {
                span { class: "truncate text-rose-700 dark:text-rose-400", title: "{err}",
                    "Count failed: {err}"
                }
            }
            button {
                class: "ml-auto shrink-0 rounded border border-slate-300 dark:border-slate-700 px-1.5 py-0.5 text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100 disabled:opacity-50",
                disabled: counting(),
                title: "Runs SELECT COUNT(*) against this object — a full scan on a large table",
                onclick: move |_| {
                    let target = table();
                    counting.set(true);
                    spawn(async move {
                        let result = state.count_table_rows(id, target.clone()).await;
                        counting.set(false);
                        counted.set(Some((target, result)));
                    });
                },
                if counting() {
                    "Counting…"
                } else {
                    "Count exactly"
                }
            }
        }
    }
}

/// The DDL overlay (FRE-108): one object's definition, read-only, with a copy
/// action. Dismissed by the ✕, a backdrop click, or Escape — handled here
/// rather than by the window listener, which ignores keys while the `<pre>`
/// container has focus.
#[component]
fn DdlOverlay(
    id: ConnectionId,
    table: TableRef,
    object: DdlObject,
    on_close: EventHandler<()>,
) -> Element {
    let state = use_context::<AppState>();
    let fetch_table = table.clone();
    let fetch_object = object.clone();
    let ddl = use_resource(move || {
        let table = fetch_table.clone();
        let object = fetch_object.clone();
        async move { state.load_ddl(id, table, object).await }
    });
    let title = match &object {
        DdlObject::Object => table.label(),
        DdlObject::Index(name) => format!("{} · index {name}", table.label()),
    };
    let loaded = ddl.read();
    rsx! {
        div {
            class: "fixed inset-0 z-40 flex items-center justify-center bg-black/40 p-4 outline-none",
            tabindex: "-1",
            onmounted: focus_on_mount,
            onkeydown: move |evt: KeyboardEvent| {
                if evt.code() == Code::Escape {
                    on_close.call(());
                }
            },
            onclick: move |_| on_close.call(()),
            div {
                class: "max-h-[80vh] w-full max-w-3xl overflow-auto rounded-lg border border-slate-300 dark:border-slate-700 bg-white dark:bg-slate-900 p-4 shadow-xl",
                onclick: move |evt| evt.stop_propagation(),
                div { class: "mb-2 flex items-center gap-2",
                    span { class: "text-xs font-semibold uppercase tracking-wide text-slate-500 dark:text-slate-400",
                        "DDL"
                    }
                    span { class: "min-w-0 flex-1 truncate font-mono text-xs text-slate-500", "{title}" }
                    match loaded.as_ref() {
                        // The provenance badge is the point of the whole
                        // feature: a rebuilt statement must never be mistaken
                        // for the server's own definition.
                        Some(Ok(ddl)) => match ddl.source {
                            DdlSource::Native => rsx! {
                                span { class: "shrink-0 rounded bg-emerald-100 dark:bg-emerald-900/50 px-1 text-xs text-emerald-700 dark:text-emerald-300",
                                    title: "The definition the server itself stores or generates",
                                    "server definition"
                                }
                            },
                            DdlSource::Reconstructed => rsx! {
                                span { class: "shrink-0 rounded bg-amber-100 dark:bg-amber-900/50 px-1 text-xs text-amber-700 dark:text-amber-300",
                                    title: "Rebuilt from catalog metadata — this backend has no DDL generator for this object",
                                    "reconstructed"
                                }
                            },
                        },
                        _ => rsx! {},
                    }
                    if let Some(Ok(ddl)) = loaded.as_ref() {
                        CopyDdlButton { text: ddl.text() }
                    }
                    button {
                        class: "shrink-0 rounded px-2 py-0.5 text-sm text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100",
                        aria_label: "Close",
                        onclick: move |_| on_close.call(()),
                        X { size: 16 }
                    }
                }
                match loaded.as_ref() {
                    None => rsx! {
                        DelayedLoading { label: "Loading definition…" }
                    },
                    Some(Err(err)) => rsx! {
                        Banner { kind: BannerKind::Error, message: err.clone() }
                    },
                    Some(Ok(ddl)) => rsx! {
                        pre { class: "whitespace-pre-wrap break-words font-mono text-xs text-slate-900 dark:text-slate-200",
                            "{ddl.text()}"
                        }
                    },
                }
            }
        }
    }
}

/// Copies the DDL exactly as shown — header and all. A reconstruction's
/// warning has to travel with the SQL, not stay behind in the window it was
/// copied from.
#[component]
fn CopyDdlButton(text: String) -> Element {
    rsx! {
        button {
            class: "shrink-0 rounded border border-slate-300 dark:border-slate-700 px-1.5 py-0.5 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100",
            onclick: move |_| copy_to_clipboard(&text),
            "Copy"
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
///
/// Shared with the grid's row detail panel (FRE-109), which names each field's
/// type beside it: the two views must not disagree about what a column is.
pub(super) fn display_type(column: &ColumnMeta) -> String {
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

    fn table_ref(schema: Option<&str>, name: &str) -> TableRef {
        TableRef {
            schema: schema.map(str::to_string),
            name: name.into(),
        }
    }

    #[test]
    fn an_exact_count_is_never_shown_beside_a_different_table() {
        let counted = (table_ref(Some("public"), "orders"), Ok(160));

        // The table it was counted for: the number is the whole reason the
        // button exists, so it must survive.
        assert_eq!(
            exact_for(Some(&counted), &table_ref(Some("public"), "orders")),
            Some(Ok(160))
        );

        // Any other table, and it is withheld — this is the guard. A count of
        // 160 shown beside `customers` would be a wrong number wearing the
        // "exact" badge, which is worse than no number at all.
        assert_eq!(
            exact_for(Some(&counted), &table_ref(Some("public"), "customers")),
            None
        );
        // Same name, different schema: two different tables.
        assert_eq!(
            exact_for(Some(&counted), &table_ref(Some("archive"), "orders")),
            None
        );
        // Same name, no schema at all (SQLite) — still not the same table.
        assert_eq!(exact_for(Some(&counted), &table_ref(None, "orders")), None);

        // Nothing counted yet.
        assert_eq!(exact_for(None, &table_ref(Some("public"), "orders")), None);

        // A *failed* count is carried the same way, so an error message cannot
        // leak onto the next table either.
        let failed = (table_ref(None, "t"), Err("boom".to_string()));
        assert_eq!(
            exact_for(Some(&failed), &table_ref(None, "t")),
            Some(Err("boom".to_string()))
        );
        assert_eq!(exact_for(Some(&failed), &table_ref(None, "u")), None);
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
