use dioxus::prelude::*;
use dioxus_icons::lucide::{Play, Plus, X};
use serde::Deserialize;
use serde_json::json;

use crate::db::{
    needs_confirmation, ConnectionId, Dialect, ExportFormat, PlanDisplay, PlanNode, PlanTree,
    QueryResult, Rollback, StatementOutcome, TableMeta, Value, EXPENSIVE_SHARE, MARKED_READ_ONLY,
    MAX_QUERY_ROWS, NO_DDL, NO_EXPLAIN, NO_MUTATE, NO_QUERY,
};

use super::history_panel::HistoryPanel;
use super::js::js_string;
use super::notice::{Banner, BannerKind, EmptyState, LoadingLine};
use super::saved_panel::SavedQueriesPanel;
use super::state::{
    AppState, ExportPane, ExportStatus, RunStatus, SchemaLoad, SharedStatement, SqlBuffer,
};

/// Cap on rendered result rows (per statement). The full result still sits
/// in memory until FRE-33 introduces streaming/limits at the query layer,
/// but the DOM must not receive a million rows.
const MAX_RENDERED_ROWS: usize = 500;

/// Messages the CodeMirror bundle sends over the eval channel.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum EditorMessage {
    Run { sql: String },
    Doc { doc: String },
}

/// Which side panel the SQL pane is showing. At most one at a time: they are
/// the same width and the same shape, and both open would leave no editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditorPanel {
    None,
    History,
    Saved,
}

/// The DOM id of one connection tab's CodeMirror host.
///
/// One instance per tab, not per buffer: switching query tabs pushes the new
/// buffer's text into it with `setDoc` (what the history panel's Load has
/// always done) rather than mounting a second editor. A per-buffer instance
/// looked cleaner and was wrong — Dioxus diffs the two hosts into the *same*
/// DOM node, so the outgoing editor stayed in place and the incoming buffer
/// was never shown.
fn editor_element_id(id: ConnectionId) -> String {
    format!("sql-editor-{id:?}").replace(['(', ')'], "-")
}

/// The name on a query tab: the saved query it was opened from, or its
/// position for a scratch buffer (FRE-113). Position rather than "Untitled"
/// so several scratch buffers stay tellable apart.
fn buffer_label(buffer: &SqlBuffer, index: usize) -> String {
    match &buffer.title {
        Some(title) => title.clone(),
        None => format!("Query {}", index + 1),
    }
}

/// Free-form SQL pane: a strip of query tabs over a CodeMirror 6 editor
/// (bundled locally in assets/codemirror.js — the app works offline), results
/// below. Ctrl+Enter runs the buffer, or just the selection when one exists.
#[component]
pub fn SqlEditor(id: ConnectionId) -> Element {
    let state = use_context::<AppState>();
    let dialect = state
        .registry
        .read()
        .get(id)
        .map(|c| c.pool.dialect())
        .unwrap_or(Dialect::Sqlite);
    // Whether this connection can produce a plan at all, and whether that
    // plan will be structured (FRE-119). `None` disables the Explain button
    // rather than hiding it: "SQL Server has no EXPLAIN" is worth saying once
    // more than it is worth leaving the user to wonder.
    let explain_support = state
        .registry
        .read()
        .get(id)
        .and_then(|c| c.pool.explain_support());
    let structured_plans = explain_support.is_some_and(|support| support.structured);
    let editor_element = editor_element_id(id);
    // The query tabs and which one is showing (FRE-113). Both are memos: they
    // read `tab_ui`, which changes on every keystroke, and a memo's PartialEq
    // gate is what keeps the pane from re-rendering for text nobody's tab
    // strip can see (FRE-129).
    let tabs = use_memo(move || {
        state
            .sql_buffers(id)
            .iter()
            .enumerate()
            .map(|(index, buffer)| (buffer.id, buffer_label(buffer, index)))
            .collect::<Vec<(u64, String)>>()
    });
    // What the editor should be showing: the active buffer *and* the
    // generation of the last text put into a buffer from outside the editor.
    // The buffer id alone is not enough — opening a saved query into an
    // untitled blank buffer reuses it, so the id does not change, and an
    // id-gated effect would rename the tab while leaving the editor empty.
    let doc_target = use_memo(move || state.sql_doc_target(id));
    let active = doc_target().0;

    // Show the active buffer's text. Runs on a real switch or open only —
    // the memo gates it — and reads the text with `peek`, so a keystroke
    // never pushes the document back into the editor under the caret.
    //
    // Known gap (not introduced here): the bundle flushes the document to
    // Rust on a 250 ms trailing timer and clears that timer on any change,
    // including the one `setDoc` dispatches. Text typed in the last 250 ms
    // before a switch is therefore lost. Closing it means flushing before
    // `setDoc`, i.e. rebuilding the checked-in `assets/codemirror.js`.
    let element_for_switch = editor_element.clone();
    use_effect(move || {
        let (buffer, _generation) = doc_target();
        let text = state
            .tab_ui
            .peek()
            .get(&id)
            .map_or(String::new(), |ui| ui.sql.text(buffer).to_string());
        document::eval(&format!(
            r#"DVEditor.setDoc("{element_for_switch}", {});"#,
            js_string(&text)
        ));
    });

    // Cheap despite running every render: the statements are Arc-shared
    // [`SharedStatement`]s, so this clones pointers and the small status —
    // never the result rows (FRE-134).
    //
    // Filtered to the active buffer: one connection still runs one script at
    // a time, but its results belong to the buffer that ran them.
    let run = state.sql_runs.read().get(&(id, active)).cloned();
    let running = matches!(run.as_ref().map(|r| &r.status), Some(RunStatus::Running));
    // What this connection won't run, stated before the user writes it
    // (FRE-87). `run_sql` refuses the same cases, so the note explains a
    // refusal that would otherwise arrive only after pressing Ctrl+Enter.
    //
    // Effective capabilities, so a connection the user marked read-only
    // (FRE-111) says so here rather than only when the run is refused.
    let capability_note: Option<&'static str> = match state.connection_caps(id) {
        Some(caps) if !caps.read_query => Some(NO_QUERY),
        Some(caps) if !caps.mutate => Some(if state.marked_read_only(id) {
            MARKED_READ_ONLY
        } else {
            NO_MUTATE
        }),
        Some(caps) if !caps.ddl => Some(NO_DDL),
        _ => None,
    };
    let pending_writes = state.pending_sql.read().get(&(id, active)).map(|pending| {
        pending
            .statements
            .iter()
            .filter(|s| needs_confirmation(s, dialect))
            .count()
    });
    // Under Confirm (FRE-111) the banner names the connection: the whole
    // point of the state is to make you read *which* database you are about
    // to change, which "Run anyway?" alone never made you do.
    let confirm_target: Option<String> = state
        .confirms_writes(id)
        .then(|| state.registry.read().get(id).map(|c| c.name.clone()))
        .flatten();
    let panel = use_signal(|| EditorPanel::None);

    rsx! {
        div { class: "flex h-full min-h-0",
        div { class: "flex min-w-0 flex-1 flex-col",
            div { class: "flex items-center justify-between border-b border-slate-200 dark:border-slate-800 px-3 py-1.5",
                div { class: "flex min-w-0 items-center gap-2",
                    span { class: "text-xs text-slate-500",
                        "Ctrl+Enter runs the buffer — or just the selection."
                    }
                    if let Some(note) = capability_note {
                        span {
                            class: "truncate rounded border border-amber-300 dark:border-amber-900/50 px-1.5 py-0.5 text-xs text-amber-700 dark:text-amber-300",
                            title: "{note}",
                            "{note}"
                        }
                    }
                }
                div { class: "flex items-center gap-2",
                    if running {
                        button {
                            class: "rounded border border-rose-300 dark:border-rose-800 px-2 py-0.5 text-xs text-rose-700 dark:text-rose-300 hover:bg-rose-100 dark:hover:bg-rose-950/50",
                            onclick: move |_| state.cancel_sql(id, active),
                            "Cancel"
                        }
                    }
                    button {
                        class: if explain_support.is_some() {
                            "rounded border border-slate-300 dark:border-slate-700 px-2 py-0.5 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100"
                        } else {
                            "rounded border border-slate-200 dark:border-slate-800 px-2 py-0.5 text-xs text-slate-400 dark:text-slate-600 cursor-not-allowed"
                        },
                        disabled: explain_support.is_none(),
                        title: match explain_support {
                            // Named rather than implied: EXPLAIN shows the
                            // plan without running the statement, but an
                            // EXPLAIN ANALYZE the user wrote themselves does
                            // run it — and is passed through as written.
                            Some(_) => "Show the query plan for this buffer, without running it",
                            None => NO_EXPLAIN,
                        },
                        onclick: move |_| state.run_explain(id, active),
                        "Explain"
                    }
                    PanelButton { panel, target: EditorPanel::Saved, label: "Saved" }
                    PanelButton { panel, target: EditorPanel::History, label: "History" }
                }
            }
            // The query tabs. Opening a saved query adds one here instead of
            // replacing what is in the editor (FRE-113).
            div { class: "flex items-center gap-1 overflow-x-auto border-b border-slate-200 dark:border-slate-800 px-2 py-1",
                for (buffer_id, label) in tabs().into_iter() {
                    div {
                        key: "{buffer_id}",
                        class: if buffer_id == active {
                            "flex shrink-0 items-center gap-1 rounded bg-slate-200 dark:bg-slate-700 px-2 py-0.5 text-xs text-slate-900 dark:text-slate-100"
                        } else {
                            "flex shrink-0 items-center gap-1 rounded px-2 py-0.5 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-100 dark:hover:bg-slate-800"
                        },
                        button {
                            class: "max-w-40 truncate",
                            title: "{label}",
                            onclick: move |_| state.select_sql_buffer(id, buffer_id),
                            "{label}"
                        }
                        if tabs.read().len() > 1 {
                            button {
                                class: "rounded px-0.5 text-slate-500 hover:bg-slate-300 dark:hover:bg-slate-600 hover:text-slate-900 dark:hover:text-slate-100",
                                aria_label: "Close query tab",
                                onclick: move |_| state.close_sql_buffer(id, buffer_id),
                                X { size: 10 }
                            }
                        }
                    }
                }
                button {
                    class: "shrink-0 rounded px-1.5 py-0.5 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-100 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100",
                    title: "New query tab",
                    aria_label: "New query tab",
                    onclick: move |_| {
                        state.new_sql_buffer(id);
                    },
                    Plus { size: 12 }
                }
            }
            EditorSurface { id, dialect }
            div { class: "min-h-0 flex-1 overflow-auto",
                if let Some(write_count) = pending_writes {
                    div { class: "flex items-center gap-3 border-b border-amber-300 dark:border-amber-900/50 bg-amber-100 dark:bg-amber-950/30 px-4 py-2",
                        span { class: "text-sm text-amber-700 dark:text-amber-300",
                            if let Some(target) = confirm_target.clone() {
                                if write_count == 1 {
                                    "Run 1 write statement against \"{target}\"?"
                                } else {
                                    "Run {write_count} write statements against \"{target}\"?"
                                }
                            } else if write_count == 1 {
                                "This script contains 1 write statement. Run anyway?"
                            } else {
                                "This script contains {write_count} write statements. Run anyway?"
                            }
                        }
                        button {
                            class: "rounded bg-amber-600 px-2.5 py-0.5 text-xs font-semibold text-slate-950 hover:bg-amber-500",
                            onclick: move |_| state.confirm_pending_sql(id, active),
                            "Run"
                        }
                        button {
                            class: "rounded border border-slate-400 dark:border-slate-600 px-2.5 py-0.5 text-xs text-slate-900 dark:text-slate-300 hover:bg-slate-200 dark:hover:bg-slate-800",
                            onclick: move |_| state.dismiss_pending_sql(id, active),
                            "Cancel"
                        }
                    }
                }
                match run {
                    None => rsx! {
                        EmptyState {
                            icon: rsx! { Play { size: 40 } },
                            title: "Results appear here",
                            hint: "Write SQL above and press Ctrl+Enter to run it.",
                        }
                    },
                    Some(run) => rsx! {
                        for (index, statement) in run.statements.iter().enumerate() {
                            if run.explain {
                                PlanSection {
                                    key: "{index}",
                                    index: index + 1,
                                    structured: structured_plans,
                                    result: statement.clone(),
                                }
                            } else {
                                StatementSection {
                                    key: "{index}",
                                    id,
                                    index: index + 1,
                                    result: statement.clone(),
                                }
                            }
                        }
                        RunStatusLine { status: run.status.clone(), statement_count: run.statements.len() }
                    },
                }
            }
        }
        match panel() {
            EditorPanel::None => rsx! {},
            EditorPanel::History => rsx! {
                HistoryPanel { id, buffer: active, editor_element: editor_element.clone() }
            },
            EditorPanel::Saved => rsx! {
                SavedQueriesPanel { id, buffer: active }
            },
        }
        }
    }
}

/// One of the SQL toolbar's panel toggles. Clicking the open one closes it.
#[component]
fn PanelButton(panel: Signal<EditorPanel>, target: EditorPanel, label: String) -> Element {
    let mut panel = panel;
    rsx! {
        button {
            class: if panel() == target {
                "rounded bg-slate-300 dark:bg-slate-700 px-2 py-0.5 text-xs text-slate-900 dark:text-slate-100"
            } else {
                "rounded border border-slate-300 dark:border-slate-700 px-2 py-0.5 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100"
            },
            onclick: move |_| {
                let showing = panel() == target;
                panel.set(if showing { EditorPanel::None } else { target });
            },
            "{label}"
        }
    }
}

/// The CodeMirror host: mounts the editor, pumps its messages, keeps its
/// completion namespace in sync, and destroys it on unmount.
///
/// Its own component so the parent can re-render (a new query tab, a finished
/// run) without remounting the editor. Which buffer its messages belong to is
/// resolved when each message arrives — the instance outlives every buffer
/// switch, so a captured id would go stale on the first one.
#[component]
fn EditorSurface(id: ConnectionId, dialect: Dialect) -> Element {
    let state = use_context::<AppState>();
    let element = editor_element_id(id);
    let dialect_name = match dialect {
        Dialect::Postgres => "postgres",
        Dialect::Sqlite => "sqlite",
        Dialect::SqlServer => "mssql",
    };

    // Mount CodeMirror once and pump its messages. The eval channel stays
    // open for the component's lifetime; unmounting destroys the JS view.
    let element_for_effect = element.clone();
    use_effect(move || {
        let element = element_for_effect.clone();
        // Peeked, not read: this effect must run on mount and never again,
        // or every keystroke would recreate the editor.
        let initial = state
            .tab_ui
            .peek()
            .get(&id)
            .map_or(String::new(), |ui| ui.sql.text(ui.sql.active()).to_string());
        // Whatever the introspection has produced so far; the refresh
        // effect below pushes updates once (re)loads finish.
        let schema_json = match state.schemas.peek().get(&id) {
            Some(SchemaLoad::Ready(tables)) => completion_schema(tables, dialect).to_string(),
            _ => "{}".to_string(),
        };
        let initial_json = js_string(&initial);
        spawn(async move {
            let js = format!(
                r#"
                window.__dvRun = (p) => dioxus.send(JSON.stringify({{ kind: "run", sql: p.sql }}));
                window.__dvDoc = (p) => dioxus.send(JSON.stringify({{ kind: "doc", doc: p.doc }}));
                DVEditor.create("{element}", "{element}", "{dialect_name}", {initial_json}, {schema_json});
                "#
            );
            let mut channel = document::eval(&js);
            // The channel closes (Err) when the component unmounts.
            while let Ok(raw) = channel.recv::<String>().await {
                match serde_json::from_str::<EditorMessage>(&raw) {
                    Ok(EditorMessage::Run { sql }) => {
                        let trimmed = sql.trim();
                        if !trimmed.is_empty() {
                            let buffer = state.active_sql_buffer(id);
                            state.run_sql(id, buffer, trimmed.to_string());
                        }
                    }
                    Ok(EditorMessage::Doc { doc }) => {
                        let buffer = state.active_sql_buffer(id);
                        state.set_sql_text(id, buffer, doc);
                    }
                    Err(_) => {}
                }
            }
        });
    });

    // Keep completion data in sync with schema reloads: reading the signal
    // (not peeking) subscribes this effect, so it re-runs whenever
    // `load_schema` rewrites the entry. While a reload is in flight
    // (Loading) or failed, the editor keeps its previous completions.
    let element_for_schema = element.clone();
    use_effect(move || {
        let schema_json = match state.schemas.read().get(&id) {
            Some(SchemaLoad::Ready(tables)) => completion_schema(tables, dialect).to_string(),
            _ => return,
        };
        document::eval(&format!(
            r#"DVEditor.updateSchema("{element_for_schema}", "{dialect_name}", {schema_json});"#
        ));
    });

    let element_for_drop = element.clone();
    use_drop(move || {
        document::eval(&format!(r#"DVEditor.destroy("{element_for_drop}");"#));
    });

    rsx! {
        div {
            id: "{element}",
            class: "h-1/2 min-h-0 shrink-0 overflow-hidden border-b border-slate-300 dark:border-slate-700 text-sm",
        }
    }
}

/// Builds the completion namespace handed to lang-sql (its `SQLNamespace`
/// shape): an object mapping table names — `"table"` on SQLite,
/// `"schema.table"` on Postgres and SQL Server — to arrays of column names.
/// lang-sql splits keys on unescaped dots, so literal dots inside
/// identifiers are escaped as `\.`; it also quote-applies any completion
/// whose label needs quoting, so weird identifiers can be passed through as
/// plain strings.
fn completion_schema(tables: &[TableMeta], dialect: Dialect) -> serde_json::Value {
    let mut namespace = serde_json::Map::new();
    for table in tables {
        let key = match (&dialect, &table.schema) {
            (Dialect::Postgres | Dialect::SqlServer, Some(schema)) => {
                format!("{}.{}", escape_dots(schema), escape_dots(&table.name))
            }
            _ => escape_dots(&table.name),
        };
        let columns: Vec<serde_json::Value> = table
            .columns
            .iter()
            .map(|c| serde_json::Value::String(c.name.clone()))
            .collect();
        // Internal objects stay completable — the SQL tab is exactly where you
        // go to poke at a chunk — but demoted, so a database with a thousand
        // of them doesn't bury the user's own tables in the ranking (FRE-88).
        //
        // This bites hardest for internal objects in the *default* schema —
        // partition children, PostGIS's `spatial_ref_sys` — because lang-sql
        // hoists only that schema's list to the root namespace, which is
        // where an unqualified name completes from. Timescale's chunks live
        // in a schema of their own and so were never in that root list;
        // demoting them orders the list you get after typing the schema
        // name, which is worth having but is the smaller of the two wins.
        let entry = match table.internal {
            Some(_) => json!({
                "self": demoted_completion(&table.name, dialect),
                "children": columns,
            }),
            None => serde_json::Value::Array(columns),
        };
        namespace.insert(key, entry);
    }
    serde_json::Value::Object(namespace)
}

/// Escapes literal dots in an identifier for use in an `SQLNamespace` key.
fn escape_dots(name: &str) -> String {
    name.replace('.', "\\.")
}

/// How far internal objects are pushed down the completion list. lang-sql
/// hands `boost` to CodeMirror, whose scale runs -99..99.
const INTERNAL_BOOST: i64 = -99;

/// The `self` completion for an internal object's `{self, children}`
/// namespace entry.
///
/// That namespace form hands the completion to CodeMirror **verbatim**,
/// unlike the plain-string entries the rest of the namespace uses, which
/// lang-sql quote-applies on our behalf. So this reproduces what lang-sql
/// would have produced for the same name, adding only the boost: a bare
/// lowercase identifier (its `^[a-z_][a-z_\d]*$`) inserts as-is, anything
/// else gets an explicit quoted `apply`.
///
/// Reproducing rather than improving on it is the point — these completions
/// sit in the same list as lang-sql's own, and one entry quoting differently
/// from its neighbours would be a worse bug than either convention alone.
/// So the quote character is lang-sql's: the **first** character of the
/// dialect's `identifierQuotes` spec, which is what its `Ym` reads. That is
/// `"` for PostgreSQL, `"` for MSSQL (whose spec is `"[`, and the `[` never
/// wins), and a backtick for SQLite (whose spec is `` `" ``).
fn demoted_completion(name: &str, dialect: Dialect) -> serde_json::Value {
    let mut chars = name.chars();
    let bare = matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if bare {
        return json!({ "label": name, "type": "type", "boost": INTERNAL_BOOST });
    }
    let quote = match dialect {
        Dialect::Postgres | Dialect::SqlServer => '"',
        Dialect::Sqlite => '`',
    };
    json!({
        "label": name,
        "type": "type",
        "boost": INTERNAL_BOOST,
        "apply": format!("{quote}{name}{quote}"),
    })
}
/// The script-level footer: running indicator, total elapsed time (shown
/// once per run), or the failure block.
#[component]
fn RunStatusLine(status: RunStatus, statement_count: usize) -> Element {
    match status {
        RunStatus::Running => rsx! {
            LoadingLine { label: "Running…" }
        },
        RunStatus::Done { elapsed_ms } => rsx! {
            p { class: "px-4 py-2 text-xs text-slate-500",
                if statement_count == 1 {
                    "1 statement in {elapsed_ms} ms"
                } else {
                    "{statement_count} statements in {elapsed_ms} ms"
                }
            }
        },
        RunStatus::Failed {
            error,
            statement_index,
            preview,
            elapsed_ms,
            rollback,
        } => rsx! {
            StatementErrorBlock {
                statement_index,
                preview,
                message: error,
                note: match rollback {
                    Rollback::Full => {
                        format!("Script rolled back after {elapsed_ms} ms — no changes were applied.")
                    }
                    Rollback::None => format!(
                        "Script stopped after {elapsed_ms} ms; earlier statements were not rolled back.",
                    ),
                    // The claim this exists to stop hubro making (FRE-146):
                    // the rollback was real, but it did not reach the schema
                    // changes, so "no changes were applied" would be false in
                    // the one direction that matters.
                    Rollback::ExceptSchemaChanges => format!(
                        "Script rolled back after {elapsed_ms} ms, but this server does not undo \
                         schema changes on rollback — the schema changes it ran, and possibly \
                         data written before them, were kept. Check the schema before running \
                         it again.",
                    ),
                },
            }
        },
        RunStatus::Refused {
            reason,
            statement_index,
            preview,
        } => rsx! {
            // The same block as a failure, with the one thing a timing line
            // can't say: nothing reached the database, so there is no partial
            // state to reason about.
            StatementErrorBlock {
                statement_index,
                preview,
                message: reason,
                note: "Nothing was run — the script was refused before it reached the database."
                    .to_string(),
            }
        },
        RunStatus::Cancelled => rsx! {
            p { class: "border-t border-amber-300 dark:border-amber-900/50 px-4 py-3 text-sm text-amber-700 dark:text-amber-300",
                "Run cancelled — an in-flight statement may still complete on the server."
            }
        },
    }
}

/// The error block both failing run outcomes render: which statement, its
/// preview, the message, and a footer saying what it means for the database.
///
/// Shares the [`Banner`] error palette but not its markup — a banner carries
/// one message, and the point here is the structure a single string can't
/// hold.
#[component]
fn StatementErrorBlock(
    statement_index: usize,
    preview: String,
    message: String,
    /// What the failure means for the database — rolled back, stopped
    /// part-way, or never sent.
    note: String,
) -> Element {
    rsx! {
        div { class: "m-3 flex items-start gap-2 rounded border px-3 py-2 {BannerKind::Error.container_classes()}",
            span { class: "shrink-0 select-none leading-5", {BannerKind::Error.icon()} }
            div { class: "min-w-0 flex-1",
                p { class: "mb-1 truncate font-mono text-xs opacity-80",
                    "{statement_index + 1} · {preview}"
                }
                p { class: "font-mono text-sm break-words", "{message}" }
                p { class: "mt-1 text-xs text-slate-500 dark:text-slate-400", "{note}" }
            }
        }
    }
}

/// One executed statement's section: a header naming the statement plus its
/// row count or affected count, and the result table for reads. Read results
/// carry an Export CSV/JSON control that serializes the full held
/// [`QueryResult`] (not just the rendered rows) through a native save dialog.
///
/// Takes the Arc-shared [`SharedStatement`], whose pointer-identity
/// `PartialEq` lets prop diffing skip this whole subtree while the statement
/// is unchanged — which is always, since results are write-once (FRE-134).
#[component]
fn StatementSection(id: ConnectionId, index: usize, result: SharedStatement) -> Element {
    let state = use_context::<AppState>();
    let summary = match &result.outcome {
        StatementOutcome::Affected(1) => "1 row affected".to_string(),
        StatementOutcome::Affected(n) => format!("{n} rows affected"),
        StatementOutcome::Rows(r) if r.rows.len() == 1 => "1 row".to_string(),
        StatementOutcome::Rows(r) => format!("{} rows", r.rows.len()),
    };
    // Whether there is a held result to export (reads only). The rows
    // themselves are cloned per export click, not per render — the buttons
    // capture the shared statement and snapshot it when pressed.
    let exportable =
        matches!(&result.outcome, StatementOutcome::Rows(rows) if !rows.rows.is_empty());
    let export_status: Option<ExportStatus> = state
        .export_status
        .read()
        .get(&(id, ExportPane::Sql))
        .cloned();
    rsx! {
        div { class: "border-b border-slate-200 dark:border-slate-800",
            p { class: "flex items-baseline gap-2 bg-slate-100 dark:bg-slate-900/60 px-4 py-1.5 text-xs",
                span { class: "font-mono text-slate-500", "{index}" }
                span { class: "min-w-0 truncate font-mono text-slate-900 dark:text-slate-300", "{result.preview}" }
                span { class: "shrink-0 text-cyan-700 dark:text-cyan-400", "— {summary}" }
                if exportable {
                    div { class: "flex-1" }
                    if let Some(status) = export_status.as_ref() {
                        {
                            let (text, class) = status.line();
                            rsx! { span { class: "shrink-0 {class}", title: "{text}", "{text}" } }
                        }
                    }
                    for (format, label) in [
                        (ExportFormat::Csv, "CSV"),
                        (ExportFormat::Json, "JSON"),
                    ] {
                        button {
                            key: "{label}",
                            class: "shrink-0 rounded border border-slate-300 dark:border-slate-700 px-1.5 py-0.5 text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100",
                            title: "Export this result to {label}",
                            onclick: {
                                let statement = result.clone();
                                move |_| {
                                    if let StatementOutcome::Rows(rows) = &statement.outcome {
                                        spawn_result_export(state, id, rows.clone(), format);
                                    }
                                }
                            },
                            "Export {label}"
                        }
                    }
                }
            }
            // The result was capped at the fetch limit to bound memory
            // (distinct from the 500-row render cap inside ResultTable).
            if result.truncated {
                div { class: "px-4 pt-2",
                    Banner {
                        kind: BannerKind::Warning,
                        message: format!(
                            "Result truncated — showing the first {MAX_QUERY_ROWS} rows to keep memory bounded. \
                             Add a LIMIT or a WHERE clause to narrow it.",
                        ),
                    }
                }
            }
            match &result.outcome {
                StatementOutcome::Affected(_) => rsx! {},
                StatementOutcome::Rows(rows) if rows.rows.is_empty() => rsx! {
                    p { class: "px-4 py-2 text-sm text-slate-500", "The statement returned no rows." }
                },
                StatementOutcome::Rows(_) => rsx! {
                    // Handed the whole shared statement, not the QueryResult
                    // inside it: the Arc bump keeps the prop pointer-diffed
                    // where a `rows.clone()` would deep-copy every row.
                    ResultTable { statement: result.clone() }
                },
            }
        }
    }
}

/// Opens a native save dialog and, on a chosen path, writes the held
/// [`QueryResult`] to it in `format` via [`AppState::export_result`] (a
/// background task; the UI never blocks).
fn spawn_result_export(
    state: AppState,
    id: ConnectionId,
    result: QueryResult,
    format: ExportFormat,
) {
    let (filter_name, ext) = match format {
        ExportFormat::Csv => ("CSV", "csv"),
        ExportFormat::Json => ("JSON", "json"),
    };
    let suggested = format!("query-result.{ext}");
    spawn(async move {
        let picked = rfd::AsyncFileDialog::new()
            .set_title("Export query result")
            .set_file_name(suggested)
            .add_filter(filter_name, &[ext])
            .save_file()
            .await;
        if let Some(file) = picked {
            state.export_result(id, result, format, file.path().to_path_buf());
        }
    });
}

/// A result grid, capped at [`MAX_RENDERED_ROWS`] rendered rows.
///
/// Takes the shared statement rather than the [`QueryResult`] inside it: an
/// `Arc` cannot lend out a piece of itself, so passing the rows alone would
/// mean cloning them, and the pointer-eq `PartialEq` gating re-renders would
/// be lost with them (FRE-134).
#[component]
fn ResultTable(statement: SharedStatement) -> Element {
    let StatementOutcome::Rows(result) = &statement.outcome else {
        // Unreachable: StatementSection only mounts this for row outcomes.
        return rsx! {};
    };
    rsx! {
        if result.rows.len() > MAX_RENDERED_ROWS {
            p { class: "border-b border-amber-300 dark:border-amber-900/50 bg-amber-100 dark:bg-amber-950/30 px-4 py-1.5 text-xs text-amber-700 dark:text-amber-300",
                "Showing the first {MAX_RENDERED_ROWS} of {result.rows.len()} rows."
            }
        }
        table { class: "w-full border-collapse text-left",
            thead { class: "sticky top-0 bg-slate-100 dark:bg-slate-900",
                tr {
                    for column in result.columns.iter() {
                        th { class: "border-b border-slate-300 dark:border-slate-700 px-3 py-1.5 font-mono text-xs font-semibold text-slate-900 dark:text-slate-300",
                            "{column.name}"
                        }
                    }
                }
            }
            tbody {
                for row in result.rows.iter().take(MAX_RENDERED_ROWS) {
                    tr { class: "border-t border-slate-200 dark:border-slate-800/60 hover:bg-slate-100 dark:hover:bg-slate-800/30",
                        // Cells are a plain function, not a `#[component]`:
                        // a component instance per cell — each cloning its
                        // Value into an owned prop — was most of the render
                        // cost of a wide result (FRE-134).
                        for value in row.iter() {
                            {result_cell(value)}
                        }
                    }
                }
            }
        }
    }
}

/// One result cell. NULL renders as an italic marker — distinct from an empty
/// string — and blobs as their size tag, both via [`Value::display`].
fn result_cell(value: &Value) -> Element {
    let display = value.display();
    let class = match value {
        Value::Null => "px-3 py-1 font-mono text-xs italic text-slate-400 dark:text-slate-600",
        Value::Blob(_) => "px-3 py-1 font-mono text-xs text-violet-700 dark:text-violet-400",
        _ => "px-3 py-1 font-mono text-xs text-slate-900 dark:text-slate-200",
    };
    rsx! {
        td { class,
            div { class: "max-w-md truncate", title: "{display}", "{display}" }
        }
    }
}

/// One explained statement (FRE-119): the same header line a result section
/// has, and below it the plan — a tree for stock PostgreSQL's JSON, the
/// server's own output verbatim for every other backend (and for PostgreSQL
/// output that isn't the JSON we asked for, e.g. a user-written `EXPLAIN
/// ANALYZE`).
///
/// Takes the Arc-shared statement for the same reason [`StatementSection`]
/// does: the prop diffs by pointer, so the plan is parsed once per run rather
/// than once per render.
#[component]
fn PlanSection(index: usize, structured: bool, result: SharedStatement) -> Element {
    let display = match &result.outcome {
        StatementOutcome::Rows(rows) => PlanDisplay::from_result(structured, rows),
        // An EXPLAIN returns rows on every backend hubro speaks; this is the
        // shape the type allows rather than one seen in practice.
        StatementOutcome::Affected(n) => PlanDisplay::Raw(format!("{n} rows affected")),
    };
    rsx! {
        div { class: "border-b border-slate-200 dark:border-slate-800",
            p { class: "flex items-baseline gap-2 bg-slate-100 dark:bg-slate-900/60 px-4 py-1.5 text-xs",
                span { class: "font-mono text-slate-500", "{index}" }
                span { class: "min-w-0 truncate font-mono text-slate-900 dark:text-slate-300", "{result.preview}" }
                if let PlanDisplay::Tree(tree) = &display {
                    span { class: "shrink-0 text-cyan-700 dark:text-cyan-400",
                        "— total cost {format_cost(tree.total_cost)}"
                    }
                }
            }
            if result.truncated {
                div { class: "px-4 pt-2",
                    Banner {
                        kind: BannerKind::Warning,
                        message: format!(
                            "Plan truncated — showing the first {MAX_QUERY_ROWS} rows of output.",
                        ),
                    }
                }
            }
            match &display {
                PlanDisplay::Tree(tree) => rsx! { PlanTreeView { tree: (**tree).clone() } },
                PlanDisplay::Raw(text) if text.is_empty() => rsx! {
                    p { class: "px-4 py-2 text-sm text-slate-500", "The server returned no plan." }
                },
                PlanDisplay::Raw(text) => rsx! {
                    pre { class: "overflow-x-auto px-4 py-2 font-mono text-xs text-slate-900 dark:text-slate-200",
                        "{text}"
                    }
                },
            }
        }
    }
}

/// A parsed plan as an indented tree, expensive nodes highlighted.
#[component]
fn PlanTreeView(tree: PlanTree) -> Element {
    let expensive = tree.rows().iter().filter(|(_, n)| n.expensive).count();
    rsx! {
        div { class: "py-1",
            for (position, (depth, node)) in tree.rows().into_iter().enumerate() {
                div { key: "{position}", {plan_node_row(depth, node)} }
            }
        }
        p { class: "px-4 pb-2 text-xs text-slate-500 dark:text-slate-400",
            if expensive > 0 {
                // Says what the highlight means, in the terms it was decided
                // in — the share is read from the constant, so a change to the
                // rule cannot leave this sentence describing the old one.
                "Highlighted: each adds at least {(EXPENSIVE_SHARE * 100.0).round() as u32}% of the plan's total cost on its own."
            }
            if let Some(planning) = tree.planning_ms {
                " Planning {format_ms(planning)} ms."
            }
            if let Some(execution) = tree.execution_ms {
                " Execution {format_ms(execution)} ms — this plan was measured, so the statement ran."
            }
        }
    }
}

/// One node's line: its label, what it costs, and what it is expected (or was
/// measured) to return. A plain function rather than a component — like
/// [`result_cell`] — so a wide plan doesn't pay for a component instance and
/// an owned prop clone per node.
fn plan_node_row(depth: usize, node: &PlanNode) -> Element {
    let indent = format!("{}rem", depth as f64 * 1.25 + 1.0);
    let class = if node.expensive {
        "flex flex-wrap items-baseline gap-x-3 border-l-2 border-amber-500 bg-amber-100 dark:bg-amber-950/30 py-0.5 pr-4 text-xs"
    } else {
        "flex flex-wrap items-baseline gap-x-3 border-l-2 border-transparent py-0.5 pr-4 text-xs"
    };
    rsx! {
        div { class, padding_left: "{indent}",
            span { class: "font-mono font-semibold text-slate-900 dark:text-slate-200", "{node.label()}" }
            if let (Some(startup), Some(total)) = (node.startup_cost, node.total_cost) {
                span { class: "font-mono text-slate-500",
                    "cost {format_cost(startup)}..{format_cost(total)}"
                }
            } else if let Some(total) = node.total_cost {
                span { class: "font-mono text-slate-500", "cost {format_cost(total)}" }
            }
            if let Some(rows) = node.plan_rows {
                span { class: "font-mono text-slate-500", "rows {format_rows(rows)}" }
            }
            if let Some(actual) = node.actual_rows {
                span { class: "font-mono text-cyan-700 dark:text-cyan-400",
                    "actual {format_rows(actual)}"
                    if let Some(ms) = node.actual_ms {
                        " in {format_ms(ms)} ms"
                    }
                    if let Some(loops) = node.loops {
                        if loops > 1.0 {
                            " × {format_rows(loops)} loops"
                        }
                    }
                }
            }
            if node.expensive {
                span { class: "rounded bg-amber-600 px-1.5 text-xs font-semibold text-slate-950",
                    "{(node.cost_share * 100.0).round() as u32}% of cost"
                }
            }
        }
    }
}

/// A plan cost, in the two decimals PostgreSQL itself prints.
fn format_cost(cost: f64) -> String {
    format!("{cost:.2}")
}

/// A row count or loop count: whole numbers stay whole (the planner's
/// estimates are integers), fractions keep two decimals (an `ANALYZE` actual
/// row count is an average over loops).
fn format_rows(rows: f64) -> String {
    if rows.fract() == 0.0 && rows.abs() < 1e15 {
        format!("{rows:.0}")
    } else {
        format!("{rows:.2}")
    }
}

/// A millisecond timing, to three decimals like `EXPLAIN ANALYZE`.
fn format_ms(ms: f64) -> String {
    format!("{ms:.3}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Internal;
    use crate::db::{ColumnMeta, TableKind};
    use serde_json::json;

    fn table(schema: Option<&str>, name: &str, columns: &[&str]) -> TableMeta {
        TableMeta {
            schema: schema.map(Into::into),
            name: name.into(),
            kind: TableKind::Table,
            columns: columns
                .iter()
                .map(|c| ColumnMeta {
                    name: (*c).into(),
                    type_name: "TEXT".into(),
                    nullable: true,
                    primary_key_position: None,
                    default: None,
                    generated: crate::db::Generated::Never,
                    type_detail: crate::db::TypeDetail::Plain,
                })
                .collect(),
            indexes: vec![],
            foreign_keys: vec![],
            restriction: None,
            internal: None,
            kind_label: None,
        }
    }

    #[test]
    fn sqlite_schema_uses_flat_table_names() {
        let tables = [
            table(None, "artists", &["id", "name"]),
            table(None, "albums", &["id", "artist_id", "title"]),
        ];
        assert_eq!(
            completion_schema(&tables, Dialect::Sqlite),
            json!({
                "artists": ["id", "name"],
                "albums": ["id", "artist_id", "title"],
            })
        );
    }

    #[test]
    fn postgres_schema_qualifies_table_names() {
        let tables = [
            table(Some("public"), "users", &["id", "email"]),
            table(Some("audit"), "events", &["id", "at"]),
        ];
        assert_eq!(
            completion_schema(&tables, Dialect::Postgres),
            json!({
                "public.users": ["id", "email"],
                "audit.events": ["id", "at"],
            })
        );
    }

    #[test]
    fn sqlserver_schema_qualifies_table_names() {
        let tables = [
            table(Some("dbo"), "users", &["id", "email"]),
            table(None, "loners", &["id"]),
        ];
        assert_eq!(
            completion_schema(&tables, Dialect::SqlServer),
            json!({
                "dbo.users": ["id", "email"],
                "loners": ["id"],
            })
        );
    }

    #[test]
    fn postgres_table_without_schema_stays_flat() {
        let tables = [table(None, "loners", &["id"])];
        assert_eq!(
            completion_schema(&tables, Dialect::Postgres),
            json!({ "loners": ["id"] })
        );
    }

    #[test]
    fn dots_in_identifiers_are_escaped_for_the_namespace() {
        // lang-sql splits namespace keys on unescaped dots, so a literal dot
        // in a schema or table name must arrive as `\.`.
        let tables = [table(
            Some("odd.schema"),
            "weird.table",
            &["a b", "sel.ect"],
        )];
        assert_eq!(
            completion_schema(&tables, Dialect::Postgres),
            json!({ r"odd\.schema.weird\.table": ["a b", "sel.ect"] })
        );
    }

    #[test]
    fn empty_schema_serializes_to_an_empty_object() {
        assert_eq!(completion_schema(&[], Dialect::Sqlite), json!({}));
    }

    /// A table the backend declared internal.
    fn internal_table(schema: Option<&str>, name: &str, columns: &[&str]) -> TableMeta {
        TableMeta {
            internal: Some(Internal::Extension("timescaledb".into())),
            ..table(schema, name, columns)
        }
    }

    #[test]
    fn internal_tables_complete_but_rank_below_the_users_own() {
        // Still completable — the SQL tab is where you go to poke at a chunk
        // — but demoted, so a thousand of them don't bury `readings`
        // (FRE-88). The user's own tables keep the plain array form, which is
        // what lets lang-sql quote-apply them for us.
        let tables = [
            table(Some("public"), "readings", &["time"]),
            internal_table(Some("_timescaledb_internal"), "_hyper_1_1_chunk", &["time"]),
        ];
        assert_eq!(
            completion_schema(&tables, Dialect::Postgres),
            json!({
                "public.readings": ["time"],
                "_timescaledb_internal._hyper_1_1_chunk": {
                    "self": { "label": "_hyper_1_1_chunk", "type": "type", "boost": -99 },
                    "children": ["time"],
                },
            })
        );
    }

    #[test]
    fn a_demoted_name_needing_quotes_carries_its_own_apply() {
        // The `{self, children}` form hands the completion to CodeMirror
        // verbatim, so unlike a plain string entry it is not quote-applied
        // for us — the quoting has to travel with it.
        assert_eq!(
            demoted_completion("weird table", Dialect::Postgres),
            json!({
                "label": "weird table",
                "type": "type",
                "boost": -99,
                "apply": "\"weird table\"",
            })
        );
        // MSSQL's spec is `"[`, and lang-sql reads only the first character
        // — so it double-quotes too, brackets notwithstanding.
        assert_eq!(
            demoted_completion("weird table", Dialect::SqlServer)["apply"],
            json!("\"weird table\"")
        );
        // SQLite's spec is backtick-then-quote, so its first character wins.
        assert_eq!(
            demoted_completion("weird table", Dialect::Sqlite)["apply"],
            json!("`weird table`")
        );
        // Upper case is not a bare identifier under lang-sql's rule either.
        assert_eq!(
            demoted_completion("Chunk", Dialect::Postgres)["apply"],
            json!("\"Chunk\"")
        );
        // A plain identifier needs no `apply` at all.
        assert_eq!(
            demoted_completion("_hyper_1_1_chunk", Dialect::Postgres),
            json!({ "label": "_hyper_1_1_chunk", "type": "type", "boost": -99 })
        );
    }
}
