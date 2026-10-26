//! The CodeMirror host: the JavaScript that mounts the editor and redoes
//! what a slow mount lost, the messages it sends back, and the completion
//! namespace built from the introspected schema.

use super::*;

/// Messages the CodeMirror bundle sends over the eval channel.
///
/// The two that carry text name the **buffer** they belong to, rather than
/// leaving the handler to ask which buffer is active when the message lands.
/// Those stopped being the same question once a tab switch stopped losing
/// the last keystrokes: the editor now flushes its pending text on the way
/// *out* of a buffer, so a message describing the outgoing one arrives, by
/// design, after the switch. `Doc` carries the
/// document generation for the same reason — see
/// [`SqlBuffers::set_text`](super::state::SqlBuffers::set_text).
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum EditorMessage {
    Run {
        sql: String,
        buffer: u64,
    },
    Doc {
        doc: String,
        buffer: u64,
        generation: u64,
    },
    /// The bundle never turned up, or `create` threw — see
    /// [`editor_mount_js`]. Carries the text to show in the pane, because the
    /// alternative is a blank one.
    Failed {
        message: String,
    },
    /// The editor exists now. Sent once, right after a successful `create`,
    /// so the pushes that happened *during* the wait can be redone — see the
    /// handler for what was being lost.
    Ready,
}

/// How long the mount waits for `assets/codemirror.js` before giving up.
///
/// Generous on purpose: the cost of waiting is nothing (the editor appears as
/// soon as the bundle does), while the cost of giving up early is a pane that
/// says the editor is broken when it was merely slow. Under software rendering
/// on a loaded machine the observed gap was well under a second.
const EDITOR_LOAD_TIMEOUT_MS: u32 = 10_000;

/// Gap between checks for the bundle's global. Short enough that the editor
/// still appears instantly to a human.
const EDITOR_LOAD_POLL_MS: u32 = 25;

/// Collapsing the budget turns this fix inside out — the pane would give up
/// after a poll or two and claim the editor failed, in exactly the slow-mount
/// case it exists for. Enforced here rather than in a test, since both values
/// are known at compile time.
const _: () = assert!(
    EDITOR_LOAD_TIMEOUT_MS >= 100 * EDITOR_LOAD_POLL_MS,
    "the load budget must allow many polls"
);

/// The bundle's name for a dialect (its `create`/`updateSchema` argument;
/// anything unrecognized falls back to SQLite there).
///
/// Derived from the enum wherever it is needed rather than passed alongside
/// it: a `dialect_name: &str` next to a `dialect: Dialect` is two arguments
/// that must agree with nothing enforcing it, and a mismatch would hand the
/// editor one engine's completions under another's grammar.
fn dialect_name(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::Postgres => "postgres",
        Dialect::Sqlite => "sqlite",
        Dialect::SqlServer => "mssql",
    }
}

/// What the pane says when the bundle never arrived. Interpolated through
/// [`js_string`], so it is safe to reword freely.
const EDITOR_UNAVAILABLE: &str = "The SQL editor failed to load. Reopening this tab will try \
     again; if it keeps happening, the editor bundle (assets/codemirror.js) is missing or failed \
     to parse.";

/// What it says when the bundle *did* arrive and `create` threw. A separate
/// message because [`EDITOR_UNAVAILABLE`] names a cause that is provably not
/// this one — the bundle loaded — and a status line that states something
/// untrue is worse than a vaguer one (the same reasoning that qualifies the
/// "rolled back" line in [`RunStatusLine`]). The thrown error is appended,
/// since that is where the real information is.
const EDITOR_CREATE_FAILED: &str =
    "The SQL editor could not start. Reopening this tab will try again.";

/// The JS that mounts one CodeMirror instance.
///
/// `DVEditor` is loaded by a separate `<script>` ([`super::CODEMIRROR_JS`]),
/// and the mount effect can win the race against it — reliably so when a
/// session restores straight into the SQL pane on a slow machine. Calling
/// `DVEditor.create` regardless threw a `ReferenceError` into a channel nobody
/// reads, and the pane then stayed **blank until it was remounted**, which
/// reads as "no query here" rather than "the editor didn't load".
///
/// So the create is gated on the global actually being there, and every way
/// this can still fail reports itself over the same channel instead of leaving
/// a blank pane: a timeout, and a `create` that throws once the bundle is
/// present.
///
/// Two consequences of *waiting* are handled here rather than left implicit:
///
/// - **A remount during the wait leaves a stale poller.** Switching away from
///   the SQL pane and back inside the load window starts a second one for the
///   same element, and whichever calls `create` last wins — measurably the
///   stale one. It would build the editor from the older document snapshot, so
///   each poller claims a generation and the outdated one stands down.
/// - **Pushes made during the wait are lost**, because `setDoc` and
///   `updateSchema` throw while the global is missing. That is why a
///   [`EditorMessage::Ready`] is sent on success: the Rust side re-pushes the
///   current document and schema, which is what keeps a tab switch mid-wait
///   from mounting the *previous* buffer's text.
///
/// `__dvRun`/`__dvDoc` are single globals, reassigned by whichever editor
/// mounted last, so both check that the message is for **this** element before
/// sending. That check earns its keep now that teardown *flushes* where it
/// used to only cancel, so a closing editor can emit — and one whose
/// connection tab is gone would otherwise be filed against the connection
/// whose channel the global currently points at. Buffer ids and generations
/// both start small and are per connection, so such a message would plausibly
/// match and quietly overwrite a real buffer. A mismatch is dropped rather
/// than rerouted: the owning channel is closing anyway, and losing one tail is
/// the bug this fix is about, while corrupting another connection's buffer
/// would be a worse one.
fn editor_mount_js(
    element: &str,
    dialect: Dialect,
    initial_json: &str,
    schema_json: &str,
    buffer: u64,
    generation: u64,
) -> String {
    let dialect_name = dialect_name(dialect);
    let unavailable = js_string(EDITOR_UNAVAILABLE);
    let create_failed = js_string(EDITOR_CREATE_FAILED);
    format!(
        r#"
        window.__dvRun = (p) => {{
            if (p.id !== "{element}") return;
            dioxus.send(JSON.stringify({{ kind: "run", sql: p.sql, buffer: p.buffer }}));
        }};
        window.__dvDoc = (p) => {{
            if (p.id !== "{element}") return;
            dioxus.send(JSON.stringify({{ kind: "doc", doc: p.doc, buffer: p.buffer, generation: p.generation }}));
        }};
        (async () => {{
            window.__dvGen = window.__dvGen || {{}};
            const mine = (window.__dvGen["{element}"] = (window.__dvGen["{element}"] || 0) + 1);
            const deadline = Date.now() + {EDITOR_LOAD_TIMEOUT_MS};
            while (typeof DVEditor === "undefined" || typeof DVEditor.create !== "function") {{
                if (Date.now() > deadline) {{
                    dioxus.send(JSON.stringify({{ kind: "failed", message: {unavailable} }}));
                    return;
                }}
                await new Promise((resolve) => setTimeout(resolve, {EDITOR_LOAD_POLL_MS}));
            }}
            if (window.__dvGen["{element}"] !== mine) {{
                return;
            }}
            try {{
                DVEditor.create("{element}", "{element}", "{dialect_name}", {initial_json}, {schema_json}, {buffer}, {generation});
            }} catch (err) {{
                dioxus.send(JSON.stringify({{ kind: "failed", message: {create_failed} + " (" + err + ")" }}));
                return;
            }}
            dioxus.send(JSON.stringify({{ kind: "ready" }}));
        }})();
        "#
    )
}

/// The JS that redoes the pushes lost while the bundle was loading, run once
/// on [`EditorMessage::Ready`].
///
/// The document is always pushed: the editor was created from a snapshot taken
/// before the wait, and if a tab switch happened in between it is holding the
/// *previous* buffer's text — whose first keystroke would flush over the
/// current buffer.
///
/// The schema is pushed **only when there is one** (`None` while a reload is in
/// flight or after one failed), mirroring the schema effect's own rule. Pushing
/// an empty namespace instead would strip the completions `create` was handed,
/// and a reload that subsequently failed would leave them stripped for the
/// session.
/// Takes the schema entry itself rather than a prepared namespace, so the
/// "only when `Ready`" rule has no second home in the caller to drift from —
/// there is simply no decision left there to get wrong.
fn editor_redo_js(
    element: &str,
    dialect: Dialect,
    doc_json: &str,
    buffer: u64,
    generation: u64,
    schema: Option<&SchemaLoad>,
) -> String {
    let mut js = format!(r#"DVEditor.setDoc("{element}", {doc_json}, {buffer}, {generation});"#);
    if let Some(SchemaLoad::Ready(tables)) = schema {
        let dialect_name = dialect_name(dialect);
        let schema_json = completion_schema(tables, dialect);
        js.push_str(&format!(
            r#"DVEditor.updateSchema("{element}", "{dialect_name}", {schema_json});"#
        ));
    }
    js
}

/// The DOM id of one connection tab's CodeMirror host.
///
/// One instance per tab, not per buffer: switching query tabs pushes the new
/// buffer's text into it with `setDoc` (what the history panel's Load has
/// always done) rather than mounting a second editor. A per-buffer instance
/// looked cleaner and was wrong — Dioxus diffs the two hosts into the *same*
/// DOM node, so the outgoing editor stayed in place and the incoming buffer
/// was never shown.
pub(super) fn editor_element_id(id: ConnectionId) -> String {
    format!("sql-editor-{id:?}").replace(['(', ')'], "-")
}

/// The CodeMirror host: mounts the editor, pumps its messages, keeps its
/// completion namespace in sync, and destroys it on unmount.
///
/// Its own component so the parent can re-render (a new query tab, a finished
/// run) without remounting the editor. Which buffer its messages belong to is
/// resolved when each message arrives — the instance outlives every buffer
/// switch, so a captured id would go stale on the first one.
#[component]
pub(super) fn EditorSurface(id: ConnectionId, dialect: Dialect) -> Element {
    let state = use_context::<AppState>();
    let element = editor_element_id(id);
    // Set only if the bundle never arrives. CodeMirror owns this
    // element's contents in every other case, so this stays `None` and the
    // host div renders no children of its own.
    let mut load_failure = use_signal(|| None::<String>);

    // Mount CodeMirror once and pump its messages. The eval channel stays
    // open for the component's lifetime; unmounting destroys the JS view.
    let element_for_effect = element.clone();
    use_effect(move || {
        let element = element_for_effect.clone();
        // Peeked, not read: this effect must run on mount and never again,
        // or every keystroke would recreate the editor.
        let (buffer, generation, initial) = state.peek_sql_document(id);
        // Whatever the introspection has produced so far; the refresh
        // effect below pushes updates once (re)loads finish.
        let schema_json = match state.schemas.peek().get(&id) {
            Some(SchemaLoad::Ready(tables)) => completion_schema(tables, dialect).to_string(),
            _ => "{}".to_string(),
        };
        let initial_json = js_string(&initial);
        spawn(async move {
            let js = editor_mount_js(
                &element,
                dialect,
                &initial_json,
                &schema_json,
                buffer,
                generation,
            );
            let mut channel = document::eval(&js);
            // The channel closes (Err) when the component unmounts.
            while let Ok(raw) = channel.recv::<String>().await {
                match serde_json::from_str::<EditorMessage>(&raw) {
                    // Both text-carrying messages are filed under the buffer
                    // the *editor* names, not under whichever is active when
                    // they arrive. The editor flushes on the way out of a
                    // buffer, so those differ exactly when it
                    // matters: asking here would file the outgoing buffer's
                    // tail — or its Ctrl+Enter — under the incoming one.
                    Ok(EditorMessage::Run { sql, buffer }) => {
                        let trimmed = sql.trim();
                        if !trimmed.is_empty() {
                            state.run_sql(id, buffer, trimmed.to_string());
                        }
                    }
                    Ok(EditorMessage::Doc {
                        doc,
                        buffer,
                        generation,
                    }) => state.set_sql_text(id, buffer, generation, doc),
                    Ok(EditorMessage::Failed { message }) => load_failure.set(Some(message)),
                    // The editor exists now, but it was built from the
                    // snapshot this effect took *before* the wait. Anything
                    // pushed in between — a tab switch, a saved query opened,
                    // an introspection that finished — threw against the
                    // missing global and was lost. Re-push both so the pane
                    // cannot show the previous buffer's text (whose first
                    // keystroke would then flush it over the current buffer)
                    // or sit without completions for the rest of the session.
                    Ok(EditorMessage::Ready) => {
                        // The *current* target, not the one `create` was
                        // handed: that is the point of the redo, and it is
                        // also the token the editor must hold from now on, or
                        // its next flush would be filed under a buffer it
                        // stopped showing during the wait.
                        let (buffer, generation, doc) = state.peek_sql_document(id);
                        // The schema entry goes in as-is: whether it is fit to
                        // push is `editor_redo_js`'s rule, not a second copy
                        // of it here. The guard is a local so nothing is held
                        // across the eval.
                        let schemas = state.schemas.peek();
                        let js = editor_redo_js(
                            &element,
                            dialect,
                            &js_string(&doc),
                            buffer,
                            generation,
                            schemas.get(&id),
                        );
                        drop(schemas);
                        document::eval(&js);
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
            r#"DVEditor.updateSchema("{element_for_schema}", "{}", {schema_json});"#,
            dialect_name(dialect)
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
            // Dioxus keeps a comment placeholder here while this is `None`, so
            // the element is not literally childless — but the placeholder is
            // swapped by node id rather than by child index, so CodeMirror's
            // own subtree cannot misdirect it. The message only ever appears
            // when CodeMirror never mounted.
            if let Some(message) = load_failure() {
                div { class: "p-3 text-rose-700 dark:text-rose-300", "{message}" }
            }
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
        // of them doesn't bury the user's own tables in the ranking.
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

    /// The mount script as the effect builds it. Buffer 7, generation 3:
    /// values that are neither the first buffer's id nor zero, so an argument
    /// dropped on the way to `create` cannot coincide with a plausible
    /// default and pass anyway.
    fn mount_js() -> String {
        editor_mount_js(
            "sql-editor-1-",
            Dialect::Postgres,
            "\"SELECT 1\"",
            "{}",
            7,
            3,
        )
    }

    #[test]
    fn the_editor_is_told_which_document_it_is_holding() {
        // The editor stamps every message with this pair, and the
        // handler files the text by it rather than by whichever buffer is
        // active when the message lands. Without it reaching `create`, the
        // first flush after a mount is stamped `undefined` and the message
        // fails to deserialize — the tail is lost exactly as before, only
        // more quietly.
        let js = mount_js();
        assert!(
            js.contains(r#""postgres", "SELECT 1", {}, 7, 3);"#),
            "create must be handed the document target alongside the text: {js}"
        );
        // And the wire must carry it back. The mount script is what copies the
        // bundle's payload onto the message, so a rename on either side shows
        // up here.
        assert!(
            js.contains("buffer: p.buffer") && js.contains("generation: p.generation"),
            "the doc message must carry the buffer and generation it was \
             typed under, or the handler is back to guessing: {js}"
        );
        assert!(
            js.contains(r#"kind: "run", sql: p.sql, buffer: p.buffer"#),
            "a run must name its buffer too — Ctrl+Enter can be the last thing \
             before a switch, and its results belong to the tab it was pressed \
             in: {js}"
        );

        // The Rust side must understand both, including the fields.
        let parsed: EditorMessage =
            serde_json::from_str(r#"{"kind":"doc","doc":"SELECT 1","buffer":7,"generation":3}"#)
                .unwrap();
        match parsed {
            EditorMessage::Doc {
                doc,
                buffer,
                generation,
            } => {
                assert_eq!(doc, "SELECT 1");
                assert_eq!((buffer, generation), (7, 3));
            }
            other => panic!("expected Doc, got {other:?}"),
        }
        let parsed: EditorMessage =
            serde_json::from_str(r#"{"kind":"run","sql":"SELECT 1","buffer":7}"#).unwrap();
        match parsed {
            EditorMessage::Run { sql, buffer } => {
                assert_eq!((sql.as_str(), buffer), ("SELECT 1", 7));
            }
            other => panic!("expected Run, got {other:?}"),
        }
        // A message without the stamp is rejected rather than defaulted: a
        // silent `0` would file every flush under a buffer id that never
        // exists (ids start at 1), losing the text with no sign of it.
        assert!(serde_json::from_str::<EditorMessage>(r#"{"kind":"doc","doc":"x"}"#).is_err());
    }

    #[test]
    fn a_closing_editor_cannot_send_down_another_connections_channel() {
        // `__dvRun`/`__dvDoc` are single globals that the last mount owns. A
        // teardown flush (teardown used to only cancel, losing the tail) can
        // therefore run against a channel belonging to a *different*
        // connection, and buffer ids and generations are per connection and
        // both start small, so the message would plausibly match a real buffer
        // and overwrite it. Dropped rather than rerouted: the owning channel
        // is closing anyway, and one lost tail beats one corrupted buffer.
        let js = mount_js();
        for global in ["window.__dvRun", "window.__dvDoc"] {
            let (_, body) = js
                .split_once(global)
                .unwrap_or_else(|| panic!("{global} must still be installed"));
            let guard = body
                .find(r#"if (p.id !== "sql-editor-1-") return;"#)
                .unwrap_or_else(|| {
                    panic!("{global} sends without checking the message is its own: {body}")
                });
            let send = body
                .find("dioxus.send")
                .unwrap_or_else(|| panic!("{global} must still send: {body}"));
            assert!(
                guard < send,
                "{global} checks the id after sending, which is not a check: {body}"
            );
        }
    }

    /// The editor's JavaScript: the readable source, and the minified build of
    /// it that actually ships.
    const ENTRY: &str = include_str!("../../../assets/codemirror-entry.js");
    const BUNDLE: &str = include_str!("../../../assets/codemirror.js");

    #[test]
    fn every_way_out_of_the_editor_flushes_rather_than_cancels() {
        // The whole tab-switch fix (a switch used to drop the last 250 ms of
        // typing), checked against the source that is readable enough to
        // check it in. Three exits, and the two orderings that make two of
        // them correct rather than merely present.
        let set_doc = entry_fn("setDoc");
        let flush = set_doc
            .find("flush(id)")
            .expect("setDoc must flush what the outgoing document still owes");
        let retoken = set_doc
            .find("tokens.set(id,")
            .expect("setDoc must take the incoming document's token");
        assert!(
            flush < retoken,
            "setDoc flushes *after* re-stamping, so the outgoing buffer's tail \
             is filed under the incoming buffer — a silent misfile, which is \
             worse than the silent loss it replaced: {set_doc}"
        );

        let destroy = entry_fn("destroy");
        let flush = destroy
            .find("flush(id)")
            .expect("destroy must flush before tearing the view down");
        let teardown = destroy
            .find("view.destroy()")
            .expect("destroy must still destroy the view");
        assert!(
            flush < teardown,
            "destroy reads the document off the view, so flushing after the \
             teardown reads nothing: {destroy}"
        );

        // And blur, which is the one that does the work — it fires on the
        // mousedown that begins a switch, long before the teardown.
        let create = entry_fn("create");
        let blur = create
            .find("blur:")
            .expect("the editor must flush when it loses focus");
        assert!(
            create[blur..]
                .split_once('}')
                .is_some_and(|(handler, _)| handler.contains("flush(id)")),
            "the blur handler does not flush: {}",
            &create[blur..blur + 120.min(create.len() - blur)]
        );

        // `flush` must take the timer *before* the guard that can abandon the
        // send, which is what makes teardown's lack of a cancel safe rather
        // than merely tidy: consume it below the guard and an armed timer can
        // outlive the view. Nothing bad follows today — in `destroy` the guard
        // passes anyway — but the reasoning above is only sound while this
        // holds, so it is checked rather than remembered.
        let flush_fn = entry_fn("flush");
        let consume = flush_fn
            .find("debounces.delete(id)")
            .expect("flush must take the pending timer");
        let abandon = flush_fn
            .find("if (!view")
            .expect("flush must still bail when there is nothing to send to");
        assert!(
            consume < abandon,
            "flush can return with the timer still armed, so teardown would \
             leave one behind: {flush_fn}"
        );

        // And teardown must not cancel *at all*. `flush` takes the timer
        // whenever there is one, so a `clearTimeout` here could only ever be a
        // no-op — but "cancel on the way out" is exactly the behaviour that
        // lost the tail, and one placed above the flush rather than below it
        // is the whole bug back. Excluding it outright is cheaper to hold than
        // an ordering, and gives up nothing.
        assert!(
            !destroy.contains("clearTimeout"),
            "teardown cancels the pending send rather than leaving it to \
             flush(), which is how the tail was being lost: {destroy}"
        );
    }

    /// One top-level function's text in `codemirror-entry.js`, from its
    /// signature to the start of the next one. Not a parser — the file is
    /// rustfmt-adjacent prettier output with every top-level `function` in
    /// column zero, and a test that mis-slices fails loudly rather than
    /// silently passing.
    fn entry_fn(name: &str) -> &'static str {
        let start = ENTRY
            .find(&format!("function {name}("))
            .unwrap_or_else(|| panic!("codemirror-entry.js no longer defines {name}()"));
        let rest = &ENTRY[start..];
        let end = rest[1..]
            .find("\nfunction ")
            .or_else(|| rest[1..].find("\nexport function "))
            .map_or(rest.len(), |offset| offset + 1);
        &rest[..end]
    }

    #[test]
    fn the_committed_bundle_is_not_a_build_from_before_flush_on_exit() {
        // `assets/codemirror.js` is a committed build artifact, rebuilt by
        // hand (see `assets/codemirror-README.md`), so an edit to the entry
        // file that lands without the rebuild changes nothing the app runs —
        // and the test above, which reads only the entry file, would not
        // notice.
        //
        // The name is the exact limit of the claim. What this pins is
        // narrower than "the bundle is a build of the entry file", which
        // nothing short of running esbuild can establish: it catches a bundle
        // reverted or never rebuilt *at all*, by requiring the stamped wire
        // format and the blur handler to be present in it. An entry-file edit
        // confined to the `setDoc`/`destroy` flushes would still slip past.
        // Matched on object-literal keys and a DOM event name — the things a
        // minifier cannot rename — so the mangled locals around them stay
        // free to change with esbuild.
        assert!(
            BUNDLE.contains("blur:") && BUNDLE.contains("domEventHandlers"),
            "the shipped bundle has no blur handler — rebuild it from \
             codemirror-entry.js (assets/codemirror-README.md)"
        );
        let doc_payload = payload(BUNDLE, "window.__dvDoc({");
        assert!(
            doc_payload.contains("buffer:") && doc_payload.contains("generation:"),
            "the shipped bundle sends documents unstamped, so a flush of the \
             outgoing buffer is indistinguishable from a reply about the \
             incoming one — rebuild it: {doc_payload}"
        );
        let run_payload = payload(BUNDLE, "window.__dvRun({");
        assert!(run_payload.contains("buffer:"), "{run_payload}");
    }

    /// The object literal a bundle call site passes, from `opening` to the
    /// `})` that closes it.
    fn payload<'a>(bundle: &'a str, opening: &str) -> &'a str {
        let (_, rest) = bundle
            .split_once(opening)
            .unwrap_or_else(|| panic!("the bundle no longer calls {opening}"));
        rest.split_once("})")
            .expect("an unterminated call would not have parsed")
            .0
    }

    #[test]
    fn the_mount_waits_for_the_bundle_before_creating_an_editor() {
        let js = mount_js();
        // The property that matters is an ordering, not a presence: `create`
        // has to sit *after* the guard. Asserting only that the guard exists
        // would stay green if a stray `DVEditor.create` were left above it,
        // which is the bug exactly.
        let guard = js
            .find("typeof DVEditor.create !== \"function\"")
            .expect("the mount must test for the bundle's global");
        let create = js
            .find("DVEditor.create(\"sql-editor-1-\"")
            .expect("the mount must still create an editor");
        assert!(
            guard < create,
            "DVEditor.create runs before the bundle is known to be loaded — on a \
             slow mount that throws into a channel nobody reads and leaves a \
             blank pane until the tab is switched away and back"
        );
        // And the wait must be able to end, or a missing bundle spins forever.
        assert!(js.contains("Date.now() > deadline"));

        // The budget itself has to reach the script. Dropping it (`const
        // deadline = Date.now();`) or collapsing the constant turns the fix
        // inside out: the pane gives up after a single poll and claims the
        // editor failed, in exactly the slow-mount case this exists for. So
        // the deadline expression is pinned, and the two constants are pinned
        // in relation to each other rather than to their own values.
        assert!(
            js.contains(&format!("Date.now() + {EDITOR_LOAD_TIMEOUT_MS}")),
            "the timeout budget never reaches the script, so the wait ends \
             immediately and a slow bundle is reported as a failure"
        );
        // (The budget/poll relation itself is a compile-time assertion beside
        // the constants — both values are known then.)
        assert!(js.contains(&format!("setTimeout(resolve, {EDITOR_LOAD_POLL_MS})")));
    }

    #[test]
    fn a_stale_poller_from_a_remount_stands_down() {
        // Switching away from the SQL pane and back inside the load window
        // starts a second poller for the same element, and whichever calls
        // `create` last wins — measurably the stale one, which would build the
        // editor from the older document snapshot.
        let js = mount_js();
        let claim = js
            .find(r#"window.__dvGen["sql-editor-1-"] = "#)
            .expect("each mount must claim a generation for its element");
        let check = js
            .find(r#"window.__dvGen["sql-editor-1-"] !== mine"#)
            .expect("a poller must check its claim is still current");
        let create = js
            .find("DVEditor.create(\"sql-editor-1-\"")
            .expect("checked above");
        // The wait itself is an anchor, not just an ordering of claim/check:
        // a recheck hoisted to immediately after the claim keeps
        // `claim < check < create` true while doing nothing, since the whole
        // point is to recheck once the await has let a remount happen.
        let wait = js
            .find("while (typeof DVEditor")
            .expect("the mount waits for the bundle");
        assert!(
            claim < wait && wait < check && check < create,
            "the generation must be claimed before the wait and rechecked \
             *after* it, or a remount's stale poller still wins the create"
        );
    }

    #[test]
    fn a_create_that_throws_is_reported_rather_than_leaving_a_blank_pane() {
        // The wait covers "the global never appeared". A bundle that loads and
        // then throws inside `create` is the other half of the same symptom,
        // and without the catch it is not merely silent — it becomes an
        // unhandled rejection, since the IIFE is never awaited.
        let js = mount_js();
        let guarded = js
            .find("try {")
            .expect("the create must be guarded against throwing");
        let create = js
            .find("DVEditor.create(\"sql-editor-1-\"")
            .expect("checked above");
        let catch = js.find("catch (err)").expect("and the throw handled");
        assert!(
            guarded < create && create < catch,
            "a create that throws still leaves a blank pane with no reason given"
        );
        assert!(
            js[catch..].contains(r#"kind: "failed""#),
            "the catch must report, not swallow"
        );
        // It must name a cause that is true on this path. The bundle demonstrably
        // loaded, so the timeout's wording would be a status line stating
        // something false.
        assert!(
            js[catch..].contains(&js_string(EDITOR_CREATE_FAILED)),
            "a create that threw must not be reported as a missing bundle"
        );
        assert!(
            !js[catch..].contains(&js_string(EDITOR_UNAVAILABLE)),
            "the two failure paths must not share the missing-bundle wording"
        );
        // And it must return, or a failed mount also announces ready.
        let send_end = js[catch..]
            .find("}));")
            .map(|offset| catch + offset + "}));".len())
            .expect("the catch reports as a statement");
        assert!(
            js[send_end..].trim_start().starts_with("return;"),
            "the catch must return rather than fall through to ready"
        );
    }

    #[test]
    fn a_successful_mount_announces_itself_so_lost_pushes_can_be_redone() {
        // `setDoc`/`updateSchema` throw while the global is missing, so a tab
        // switch or a finished introspection during the wait is lost — and a
        // lost `setDoc` is worse than a lost schema: the editor then holds the
        // *previous* buffer's text, whose first keystroke flushes over the
        // current buffer.
        let js = mount_js();
        let create = js
            .find("DVEditor.create(\"sql-editor-1-\"")
            .expect("checked above");
        let ready = js
            .find(r#"kind: "ready""#)
            .expect("a successful mount must announce itself");
        assert!(
            create < ready,
            "the ready signal must follow the create it reports"
        );
        // It must not fire on the failure paths. Checked as "the statement
        // immediately after the send is a return", not as "a return appears
        // somewhere before ready" — the latter is satisfied by the generation
        // recheck's return and the catch's, so it passes even when the timeout
        // path falls straight through.
        let failed = js.find(r#"kind: "failed""#).expect("checked above");
        let send_end = js[failed..]
            .find("}));")
            .map(|offset| failed + offset + "}));".len())
            .expect("the timeout report is a statement");
        assert!(
            js[send_end..].trim_start().starts_with("return;"),
            "the timeout path must return immediately after reporting — \
             otherwise it falls through to ready (a failed mount reporting \
             success), and the loop sends a fresh failure every poll for the \
             life of the window"
        );

        // And the Rust side must understand it.
        let parsed: EditorMessage = serde_json::from_str(r#"{"kind":"ready"}"#).unwrap();
        assert!(matches!(parsed, EditorMessage::Ready));
    }

    #[test]
    fn the_redo_always_restores_the_document_and_never_strips_completions() {
        // The document carries the text, so it is pushed unconditionally: the
        // editor was created from a pre-wait snapshot, and after a tab switch
        // it holds the *previous* buffer, whose first keystroke would flush
        // over the current one.
        let ready = SchemaLoad::Ready(vec![table(None, "artists", &["id", "name"])]);
        let js = editor_redo_js(
            "sql-editor-1-",
            Dialect::Sqlite,
            "\"SELECT 1\"",
            7,
            3,
            Some(&ready),
        );
        assert!(
            js.contains(r#"DVEditor.setDoc("sql-editor-1-", "SELECT 1", 7, 3)"#),
            "the redo must restore the document — without it this whole \
             message is decoration and a tab switch during the wait still \
             loses text: {js}"
        );
        assert!(js.contains(r#"DVEditor.updateSchema("sql-editor-1-", "sqlite", {"artists""#));

        // Anything that is not `Ready` must produce NO updateSchema at all.
        // Pushing an empty namespace would *strip* the completions `create`
        // was handed, and a reload that then failed would leave them stripped
        // for the session — the outcome this redo exists to prevent, arrived
        // at from the other direction.
        //
        // Every non-Ready state is exercised, not just the absent entry: the
        // realistic trigger is `Loading` (a reload starting during the wait).
        let failed = SchemaLoad::Failed("boom".to_string());
        for schema in [None, Some(&SchemaLoad::Loading), Some(&failed)] {
            let js = editor_redo_js(
                "sql-editor-1-",
                Dialect::Sqlite,
                "\"SELECT 1\"",
                7,
                3,
                schema,
            );
            assert!(
                js.contains("setDoc"),
                "the document is pushed regardless of the schema"
            );
            assert!(
                !js.contains("updateSchema"),
                "a reload in flight or failed must leave the existing \
                 completions alone, exactly as the schema effect does: {js}"
            );
        }
    }

    #[test]
    fn a_bundle_that_never_loads_reports_itself_instead_of_showing_nothing() {
        let js = mount_js();
        assert!(
            js.contains(r#"kind: "failed""#),
            "a timeout must send something back — a blank pane reads as \"no \
             query here\" rather than \"the editor didn't load\""
        );
        // What this pins is that the message reaches the script intact — the
        // whole of it, as a closed literal.
        //
        // It does **not** pin the escaping, and cannot: today's wording
        // contains nothing `js_string` would alter, so a raw
        // `format!("\"{}\"", …)` produces identical bytes and passes too.
        // The escaping is a property of `js_string` and is pinned where it can
        // actually fail, in `js::tests` — the risk here would only become real
        // if the message ever gained a quote or a backslash, which is the
        // reason it goes through the helper at all.
        assert!(
            js.contains(&js_string(EDITOR_UNAVAILABLE)),
            "the failure message must reach the script as a closed JS literal"
        );

        // And the Rust side must understand what that path sends.
        let parsed: EditorMessage =
            serde_json::from_str(r#"{"kind":"failed","message":"boom"}"#).unwrap();
        match parsed {
            EditorMessage::Failed { message } => assert_eq!(message, "boom"),
            other => panic!("expected Failed, got {other:?}"),
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
        // — but demoted, so a thousand of them don't bury `readings`. The
        // user's own tables keep the plain array form, which is what lets
        // lang-sql quote-apply them for us.
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
