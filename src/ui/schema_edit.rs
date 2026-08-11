//! The schema-edit dialog (FRE-122): fill in one operation, read the SQL it
//! generates, and run it.
//!
//! The statement is **shown before it runs and is editable** — the same
//! principle staged row edits follow, and the reason this is a dialog rather
//! than a menu item that fires an `ALTER`. What runs is the text in the box,
//! not the operation the button named, and everything downstream treats it that
//! way: [`AppState::start_schema_edit`] re-checks the text against the
//! connection's capabilities, and [`after_edit`] refuses to move the selection
//! once the text has been touched.
//!
//! Confirmation is proportional to the damage, which takes three forms:
//!
//! - operations that lose nothing (create/drop index, add column, rename) run
//!   on a plain press;
//! - operations that destroy data ([`SchemaOp::destroys_data`]) require the
//!   table's name to be typed — the point being to make you read *which* table
//!   is about to be emptied;
//! - a connection marked *confirm writes* (FRE-111) adds its own press, naming
//!   the connection, on top of either — because the question that state asks is
//!   which database you are pointed at, and that is a different question from
//!   which table.
//!
//! [`run_action`] is where those three combine, as a pure function over what
//! the dialog holds, so the rule is testable rather than implied by the order
//! of a component's `if`s.

use dioxus::prelude::*;
use dioxus_icons::lucide::X;

use crate::db::{
    op_problem, schema_edit_refusal, schema_op_sql, Capabilities, ConnectionId, Dialect, SchemaOp,
    TableMeta,
};

use super::notice::{Banner, BannerKind};
use super::state::{after_edit, AppState, SchemaEditRequest, TableRef};

const INPUT_CLASS: &str = "rounded border border-slate-300 dark:border-slate-700 bg-white \
                           dark:bg-slate-800 px-2 py-1 text-xs text-slate-900 dark:text-slate-100 \
                           focus:outline-none focus:ring-1 focus:ring-sky-500";

/// What pressing Run should do, given everything the dialog holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RunAction {
    /// The connection or the object forbids this operation: nothing to press,
    /// and the sentence says why.
    Refused(&'static str),
    /// The form is not filled in yet.
    Incomplete(&'static str),
    /// A destructive operation whose confirmation has not been typed.
    TypeTheName,
    /// A *confirm writes* connection that has not confirmed this exact
    /// statement yet.
    Confirm,
    /// Run it.
    Run,
}

/// Resolves a Run press.
///
/// The order is the substance. A refusal comes first, because being asked to
/// confirm something that can never run is a prompt with no right answer — the
/// same reason [`start_sql`](crate::ui::state) checks capabilities ahead of its
/// write banner. The typed name comes before the connection confirmation
/// because it is about the narrower thing (this table, not this database), so
/// meeting it first leaves the broader question last, next to the button.
///
/// `confirmed` is the statement a previous press already confirmed. Comparing
/// it against the current `sql` is what makes the confirmation about *this*
/// statement: edit the box after confirming and the prompt returns, rather than
/// laundering whatever the box happens to hold by the time Run is pressed
/// again. That rule is [`import_action`](crate::ui::import)'s, and it is the
/// same rule for the same reason.
pub(super) fn run_action(press: Press) -> RunAction {
    if let Some(reason) = press.refusal {
        return RunAction::Refused(reason);
    }
    if let Some(problem) = press.problem {
        return RunAction::Incomplete(problem);
    }
    // The box is editable, so it can be emptied. Without this the press runs a
    // script with no statements, which reports nothing at all — a button that
    // silently does nothing is the worst of the three states it could be in.
    if press.sql.trim().is_empty() {
        return RunAction::Incomplete(NOTHING_TO_RUN);
    }
    if press.destroys_data && press.typed.trim() != press.table_name {
        return RunAction::TypeTheName;
    }
    if press.confirms && press.confirmed != Some(press.sql) {
        return RunAction::Confirm;
    }
    RunAction::Run
}

/// What an emptied SQL box reports.
pub(super) const NOTHING_TO_RUN: &str = "There is no statement to run.";

/// Everything a Run press is decided from — see [`run_action`], whose
/// arguments these are.
pub(super) struct Press<'a> {
    /// Why the operation cannot run here at all, from
    /// [`schema_edit_refusal`].
    pub refusal: Option<&'static str>,
    /// What the form is still missing, from [`op_problem`].
    pub problem: Option<&'static str>,
    pub destroys_data: bool,
    /// The table's name, and what was typed to confirm it.
    pub table_name: &'a str,
    pub typed: &'a str,
    /// Whether the connection is marked *confirm writes*, and the statement a
    /// previous press confirmed.
    pub confirms: bool,
    pub confirmed: Option<&'a str>,
    /// The statement as it stands in the box.
    pub sql: &'a str,
}

/// What the last schema edit on this connection did, or is doing (FRE-122).
///
/// Rendered above the pane body rather than inside it. The dialog closes the
/// moment its statement succeeds, so the report has to outlive it — and a drop
/// deselects the table it dropped, which unmounts the body. Placing the line
/// there would have made the one operation with no undo the one that reported
/// nothing.
#[component]
pub fn SchemaEditLine(id: ConnectionId) -> Element {
    let state = use_context::<AppState>();
    let Some(status) = state.schema_edit_status(id) else {
        return rsx! {};
    };
    let (text, color) = status.line();
    rsx! {
        div { class: "flex items-center gap-2 border-b border-slate-200 dark:border-slate-800 px-3 py-1 text-xs",
            span { class: "{color}", "{text}" }
            button {
                class: "rounded px-1 text-slate-500 hover:bg-slate-200 dark:hover:bg-slate-800",
                aria_label: "Dismiss",
                onclick: move |_| state.clear_schema_edit_status(id),
                X { size: 12 }
            }
        }
    }
}

/// One schema operation, from an empty form to a statement that has run.
#[component]
pub fn SchemaEditDialog(
    id: ConnectionId,
    /// The object the operation applies to. Its columns and indexes are what
    /// the forms offer, and its kind is half of whether anything is offered
    /// at all.
    ///
    /// A [`ReadSignal`], with `dialect` and `caps` below, because the
    /// memos in this component read all three: a plain prop is captured once
    /// into a memo's closure and never re-read, so the generated SQL and the
    /// refusal would go on describing the metadata the dialog opened with.
    /// `caps` is the one that would mislead — write protection can be changed
    /// on a live connection (`ConnectionRegistry::set_protection`), and a stale
    /// gate would leave the button offering what the user just forbade. The
    /// write itself is still refused (`start_schema_edit` re-resolves
    /// capabilities and `run_script` checks the text), so this is about the
    /// button not lying rather than about the guard.
    table: ReadSignal<TableMeta>,
    /// The operation, seeded by whichever button opened the dialog — a per-index
    /// Drop arrives with its index name, a per-column Rename with its column.
    op: SchemaOp,
    dialect: ReadSignal<Dialect>,
    /// The connection's effective capabilities (FRE-87/111).
    caps: ReadSignal<Capabilities>,
    /// The connection's name when it is marked *confirm writes*, `None`
    /// otherwise.
    confirm_connection: Option<String>,
    on_close: EventHandler<()>,
) -> Element {
    let state = use_context::<AppState>();
    let op = use_signal(|| op);
    // The box: `None` while it still mirrors the form. Any keystroke takes it
    // over, and from then on the form no longer writes over what was typed —
    // silently rewriting an edited statement because a checkbox moved would be
    // its own small data-loss.
    let mut edited = use_signal(|| Option::<String>::None);
    let mut typed_name = use_signal(String::new);
    let mut confirmed = use_signal(|| Option::<String>::None);
    // A *finished* previous edit's line belongs to the pane, not to this
    // dialog: without clearing it, reopening after a success would close
    // immediately on the stale Done. A `Running` one is left alone — clearing
    // it would re-enable Run while a statement is still in flight, and the
    // second press would send a second one.
    use_hook(move || {
        if !state.schema_edit_status(id).is_some_and(|s| s.is_running()) {
            state.clear_schema_edit_status(id);
        }
    });

    let generated = use_memo(move || schema_op_sql(dialect(), &table.read(), &op()));
    let sql = use_memo(move || edited().unwrap_or_else(&*generated));

    let refusal = use_memo(move || schema_edit_refusal(caps(), dialect(), &table.read(), &op()));
    let problem = use_memo(move || op_problem(&op()));
    let note = use_memo(move || op().note(dialect()));

    let status = state.schema_edit_status(id);
    let running = status.as_ref().is_some_and(|s| s.is_running());
    // Done closes the dialog — the pane's line reports what happened, and
    // keeping a dialog open around a statement that has already run invites
    // running it twice.
    //
    // **The status is read inside the closure**, not captured as a bool from
    // the render above. `use_effect` subscribes to the signals its closure
    // reads; a captured value subscribes to nothing, so the effect would run
    // once at mount — when there is deliberately no status — and never again.
    // The dialog appeared to close anyway, because a successful edit reloads
    // the schema and the pane unmounts this whole subtree while it loads. That
    // is somebody else's side effect, not a way to dismiss a dialog.
    use_effect(move || {
        let finished = state
            .schema_edit_status(id)
            .is_some_and(|s| !s.is_running() && s.error().is_none());
        if finished {
            on_close.call(());
        }
    });
    let failure = status.as_ref().and_then(|s| s.error().map(str::to_string));

    let confirms = confirm_connection.is_some();
    let action = run_action(Press {
        refusal: refusal(),
        problem: problem(),
        destroys_data: op().destroys_data(),
        table_name: &table.read().name,
        typed: &typed_name(),
        confirms,
        confirmed: confirmed().as_deref(),
        sql: &sql(),
    });
    let armed = action == RunAction::Run && confirms;

    let run = move |_| {
        let sql = sql();
        if action != RunAction::Run {
            if action == RunAction::Confirm {
                confirmed.set(Some(sql));
            }
            return;
        }
        let meta = table.read().clone();
        let target = TableRef {
            schema: meta.schema.clone(),
            name: meta.name.clone(),
        };
        let op = op();
        // Whether the statement *differs* from the generated one, not whether
        // the box was touched: retyping a character and undoing it leaves an
        // override that says nothing about what runs. This is what decides
        // whether the selection may follow a rename.
        let edited = sql != generated();
        state.start_schema_edit(
            id,
            SchemaEditRequest {
                after: after_edit(&op, edited, &target),
                table: target,
                running_label: running_label(&op, &meta),
                done_label: done_label(&op, &meta),
                sql,
            },
        );
    };

    rsx! {
        div {
            class: "fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-black/40 p-4 outline-none",
            tabindex: "-1",
            onmounted: super::js::focus_on_mount,
            onkeydown: move |evt: KeyboardEvent| {
                if evt.code() == Code::Escape {
                    evt.stop_propagation();
                    on_close.call(());
                }
            },
            div {
                class: "w-full max-w-2xl rounded-lg bg-white dark:bg-slate-900 p-4 shadow-xl",
                onclick: move |evt| evt.stop_propagation(),
                div { class: "mb-3 flex items-baseline justify-between gap-2",
                    h2 { class: "text-sm font-semibold text-slate-900 dark:text-slate-100",
                        "{op().label()}"
                    }
                    span { class: "truncate font-mono text-xs text-slate-500 dark:text-slate-400",
                        "{table_label(&table.read())}"
                    }
                }

                OpForm { table: table(), op }

                if let Some(note) = note() {
                    div { class: "mb-2",
                        Banner { kind: BannerKind::Warning, message: note.to_string() }
                    }
                }

                // The statement, as it will run. Editable on purpose: the
                // generator covers the operations dialects agree on, and the
                // box is what covers everything else about them.
                label { class: "mb-1 flex items-baseline gap-2 text-xs text-slate-500 dark:text-slate-400",
                    "SQL to run"
                    if edited().is_some() {
                        button {
                            class: "rounded border border-slate-300 dark:border-slate-700 px-1.5 text-xs hover:bg-slate-200 dark:hover:bg-slate-800",
                            onclick: move |_| edited.set(None),
                            "Reset to generated"
                        }
                    }
                }
                textarea {
                    class: "mb-2 h-24 w-full resize-y rounded border border-slate-300 dark:border-slate-700 bg-slate-50 dark:bg-slate-800 p-2 font-mono text-xs text-slate-900 dark:text-slate-100 focus:outline-none focus:ring-1 focus:ring-sky-500",
                    spellcheck: false,
                    value: "{sql()}",
                    oninput: move |evt| edited.set(Some(evt.value())),
                }

                if let Some(err) = failure {
                    div { class: "mb-2",
                        Banner { kind: BannerKind::Error, message: err }
                    }
                }

                // Typing the name is the confirmation for the two operations
                // that lose data. It names the table rather than asking "are
                // you sure?", because reading which table it is is the point.
                if op().destroys_data() && refusal().is_none() {
                    label { class: "mb-2 flex flex-wrap items-center gap-2 text-xs text-slate-600 dark:text-slate-300",
                        "Type {table.read().name} to confirm"
                        input {
                            class: INPUT_CLASS,
                            value: "{typed_name()}",
                            oninput: move |evt| typed_name.set(evt.value()),
                        }
                    }
                }

                div { class: "flex items-center justify-between gap-2",
                    div { class: "min-w-0 flex-1 text-xs",
                        match &action {
                            RunAction::Refused(reason) => rsx! {
                                span { class: "text-amber-700 dark:text-amber-300", "{reason}" }
                            },
                            RunAction::Incomplete(problem) => rsx! {
                                span { class: "text-slate-500 dark:text-slate-400", "{problem}" }
                            },
                            RunAction::TypeTheName => rsx! {
                                span { class: "text-slate-500 dark:text-slate-400",
                                    "This cannot be undone."
                                }
                            },
                            _ => rsx! {
                                if let (true, Some(name)) = (armed, confirm_connection.clone()) {
                                    span { class: "text-amber-700 dark:text-amber-300",
                                        "Run this against {name}?"
                                    }
                                }
                            },
                        }
                    }
                    button {
                        class: "rounded border border-slate-300 dark:border-slate-700 px-2 py-1 text-xs text-slate-600 dark:text-slate-300 hover:bg-slate-200 dark:hover:bg-slate-800",
                        onclick: move |_| on_close.call(()),
                        "Cancel"
                    }
                    button {
                        class: if op().destroys_data() {
                            "rounded bg-rose-600 px-3 py-1 text-xs font-medium text-white hover:bg-rose-500 disabled:opacity-50"
                        } else {
                            "rounded bg-sky-600 px-3 py-1 text-xs font-medium text-white hover:bg-sky-500 disabled:opacity-50"
                        },
                        disabled: running
                            || matches!(action, RunAction::Refused(_) | RunAction::Incomplete(_) | RunAction::TypeTheName),
                        onclick: run,
                        if running {
                            "Running…"
                        } else if armed {
                            "Yes, run it"
                        } else {
                            "Run"
                        }
                    }
                }
            }
        }
    }
}

/// The operation's own controls. Every field edits [`SchemaOp`] in place, so
/// the generated SQL below follows as it is typed.
#[component]
fn OpForm(table: TableMeta, op: Signal<SchemaOp>) -> Element {
    let current = op();
    rsx! {
        div { class: "mb-3 flex flex-col gap-2 text-xs text-slate-600 dark:text-slate-300",
            match current {
                SchemaOp::CreateIndex { name, columns, unique } => rsx! {
                    label { class: "flex items-center gap-2",
                        "Name"
                        input {
                            class: INPUT_CLASS,
                            value: "{name}",
                            oninput: move |evt| {
                                if let SchemaOp::CreateIndex { name, .. } = &mut *op.write() {
                                    *name = evt.value();
                                }
                            },
                        }
                    }
                    label { class: "flex items-center gap-2",
                        input {
                            r#type: "checkbox",
                            checked: unique,
                            onchange: move |evt| {
                                if let SchemaOp::CreateIndex { unique, .. } = &mut *op.write() {
                                    *unique = evt.checked();
                                }
                            },
                        }
                        "Unique"
                    }
                    div { class: "flex flex-wrap items-center gap-2",
                        span { "Columns" }
                        for column in table.columns.clone() {
                            label { key: "{column.name}", class: "flex items-center gap-1",
                                input {
                                    r#type: "checkbox",
                                    checked: columns.contains(&column.name),
                                    onchange: {
                                        let name = column.name.clone();
                                        move |evt: Event<FormData>| {
                                            if let SchemaOp::CreateIndex { columns, .. } = &mut *op.write() {
                                                // Order follows the order they
                                                // were picked: an index's column
                                                // order is not cosmetic.
                                                if evt.checked() {
                                                    columns.push(name.clone());
                                                } else {
                                                    columns.retain(|c| c != &name);
                                                }
                                            }
                                        }
                                    },
                                }
                                span { class: "font-mono", "{column.name}" }
                            }
                        }
                    }
                },
                SchemaOp::DropIndex { name } => rsx! {
                    div { "Index " span { class: "font-mono", "{name}" } }
                },
                SchemaOp::AddColumn { name, type_name } => rsx! {
                    label { class: "flex items-center gap-2",
                        "Name"
                        input {
                            class: INPUT_CLASS,
                            value: "{name}",
                            oninput: move |evt| {
                                if let SchemaOp::AddColumn { name, .. } = &mut *op.write() {
                                    *name = evt.value();
                                }
                            },
                        }
                    }
                    label { class: "flex items-center gap-2",
                        "Type"
                        input {
                            class: INPUT_CLASS,
                            value: "{type_name}",
                            oninput: move |evt| {
                                if let SchemaOp::AddColumn { type_name, .. } = &mut *op.write() {
                                    *type_name = evt.value();
                                }
                            },
                        }
                    }
                    p { class: "text-slate-500 dark:text-slate-400",
                        "The column is added nullable, with no default — the one form every \
                         backend accepts, and the only one that cannot fail on an existing row."
                    }
                },
                SchemaOp::RenameTable { new_name } => rsx! {
                    label { class: "flex items-center gap-2",
                        "New name"
                        input {
                            class: INPUT_CLASS,
                            value: "{new_name}",
                            oninput: move |evt| {
                                if let SchemaOp::RenameTable { new_name } = &mut *op.write() {
                                    *new_name = evt.value();
                                }
                            },
                        }
                    }
                },
                SchemaOp::RenameColumn { column, new_name } => rsx! {
                    div { "Column " span { class: "font-mono", "{column}" } }
                    label { class: "flex items-center gap-2",
                        "New name"
                        input {
                            class: INPUT_CLASS,
                            value: "{new_name}",
                            oninput: move |evt| {
                                if let SchemaOp::RenameColumn { new_name, .. } = &mut *op.write() {
                                    *new_name = evt.value();
                                }
                            },
                        }
                    }
                },
                SchemaOp::DropTable => rsx! {
                    p { "The table and everything in it are removed." }
                },
                SchemaOp::Truncate => rsx! {
                    p { "Every row is removed. The table itself stays." }
                },
            }
        }
    }
}

/// `schema.table`, or just the name where there is no schema.
fn table_label(table: &TableMeta) -> String {
    match &table.schema {
        Some(schema) => format!("{schema}.{}", table.name),
        None => table.name.clone(),
    }
}

/// The status line while the statement runs.
fn running_label(op: &SchemaOp, table: &TableMeta) -> String {
    let name = &table.name;
    match op {
        SchemaOp::CreateIndex { .. } => format!("Creating an index on {name}"),
        SchemaOp::DropIndex { name } => format!("Dropping index {name}"),
        SchemaOp::AddColumn { .. } => format!("Adding a column to {name}"),
        SchemaOp::RenameTable { .. } => format!("Renaming {name}"),
        SchemaOp::RenameColumn { .. } => format!("Renaming a column of {name}"),
        SchemaOp::DropTable => format!("Dropping table {name}"),
        SchemaOp::Truncate => format!("Emptying {name}"),
    }
}

/// The same line once it has succeeded — past tense, and naming what changed
/// rather than reporting a bare "Done".
fn done_label(op: &SchemaOp, table: &TableMeta) -> String {
    let name = &table.name;
    match op {
        SchemaOp::CreateIndex { name: index, .. } => format!("Created index {index}"),
        SchemaOp::DropIndex { name: index } => format!("Dropped index {index}"),
        SchemaOp::AddColumn { name: column, .. } => format!("Added column {column} to {name}"),
        SchemaOp::RenameTable { new_name } => format!("Renamed {name} to {new_name}"),
        SchemaOp::RenameColumn { column, new_name } => {
            format!("Renamed {column} to {new_name}")
        }
        SchemaOp::DropTable => format!("Dropped table {name}"),
        SchemaOp::Truncate => format!("Emptied {name}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{NOT_A_TABLE, NO_DDL};

    const SQL: &str = "DROP TABLE \"t\";";

    /// A press with nothing in the way: not destructive, not protected. Named
    /// fields are overridden per test, so each one reads as the single thing
    /// it varies.
    fn press() -> Press<'static> {
        Press {
            refusal: None,
            problem: None,
            destroys_data: false,
            table_name: "t",
            typed: "",
            confirms: false,
            confirmed: None,
            sql: SQL,
        }
    }

    #[test]
    fn an_ordinary_operation_runs_on_one_press() {
        assert_eq!(run_action(press()), RunAction::Run);
    }

    #[test]
    fn an_emptied_box_says_so_instead_of_running_nothing() {
        for sql in ["", "   ", "\n\t "] {
            assert_eq!(
                run_action(Press { sql, ..press() }),
                RunAction::Incomplete(NOTHING_TO_RUN),
                "{sql:?}"
            );
        }
        // …and a refusal still outranks it: the connection's answer is the
        // more useful one, and it does not depend on what is in the box.
        assert_eq!(
            run_action(Press {
                sql: "",
                refusal: Some(NO_DDL),
                ..press()
            }),
            RunAction::Refused(NO_DDL)
        );
    }

    #[test]
    fn a_refusal_outranks_everything_else() {
        // Including the confirmation: being asked to confirm a statement that
        // can never run is a prompt with no right answer.
        assert_eq!(
            run_action(Press {
                refusal: Some(NO_DDL),
                destroys_data: true,
                typed: "t",
                confirms: true,
                ..press()
            }),
            RunAction::Refused(NO_DDL)
        );
        assert_eq!(
            run_action(Press {
                refusal: Some(NOT_A_TABLE),
                problem: Some("The column needs a name."),
                ..press()
            }),
            RunAction::Refused(NOT_A_TABLE)
        );
    }

    #[test]
    fn an_incomplete_form_is_reported_before_any_confirmation() {
        assert_eq!(
            run_action(Press {
                problem: Some("The index needs a name."),
                confirms: true,
                ..press()
            }),
            RunAction::Incomplete("The index needs a name.")
        );
    }

    #[test]
    fn a_destructive_operation_waits_for_the_tables_name() {
        let dropping = Press {
            destroys_data: true,
            table_name: "orders",
            ..press()
        };
        assert_eq!(run_action(Press { ..dropping }), RunAction::TypeTheName);
        // A near miss is a miss: this is the guard against dropping the table
        // beside the one you meant.
        for typed in ["order", "orders2", "Orders", "ORDERS"] {
            assert_eq!(
                run_action(Press { typed, ..dropping }),
                RunAction::TypeTheName,
                "{typed:?}"
            );
        }
        // Surrounding whitespace is not a difference anyone means.
        assert_eq!(
            run_action(Press {
                typed: "  orders  ",
                ..dropping
            }),
            RunAction::Run
        );
    }

    #[test]
    fn a_confirm_connection_asks_once_more_after_the_name_is_typed() {
        // Both gates, in order: the table's name first, then the connection.
        // They ask different questions — which table, and which database — so
        // meeting one must not stand in for the other.
        let protected = Press {
            destroys_data: true,
            table_name: "orders",
            confirms: true,
            ..press()
        };
        assert_eq!(run_action(Press { ..protected }), RunAction::TypeTheName);
        assert_eq!(
            run_action(Press {
                typed: "orders",
                ..protected
            }),
            RunAction::Confirm
        );
        assert_eq!(
            run_action(Press {
                typed: "orders",
                confirmed: Some(SQL),
                ..protected
            }),
            RunAction::Run
        );
    }

    #[test]
    fn confirming_authorizes_that_statement_and_no_other() {
        // The rule `import_action` follows: a confirmation that survived an
        // edit would launder whatever the box holds by the second press —
        // with a DDL statement behind it.
        assert_eq!(
            run_action(Press {
                confirms: true,
                confirmed: Some("DROP TABLE \"t\";"),
                sql: "DROP TABLE \"other\";",
                ..press()
            }),
            RunAction::Confirm
        );
        // An unprotected connection never asks, whatever was confirmed before.
        assert_eq!(
            run_action(Press {
                sql: "DROP TABLE \"x\";",
                ..press()
            }),
            RunAction::Run
        );
    }

    #[test]
    fn labels_name_what_changed_rather_than_reporting_done() {
        let table = TableMeta {
            schema: Some("app".into()),
            name: "orders".into(),
            kind: crate::db::TableKind::Table,
            columns: vec![],
            indexes: vec![],
            foreign_keys: vec![],
            restriction: None,
            internal: None,
            kind_label: None,
        };
        assert_eq!(
            running_label(&SchemaOp::DropTable, &table),
            "Dropping table orders"
        );
        assert_eq!(
            done_label(&SchemaOp::DropTable, &table),
            "Dropped table orders"
        );
        assert_eq!(
            done_label(
                &SchemaOp::RenameTable {
                    new_name: "invoices".into()
                },
                &table
            ),
            "Renamed orders to invoices"
        );
        assert_eq!(table_label(&table), "app.orders");
    }
}
