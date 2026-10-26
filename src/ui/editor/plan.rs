//! An Explain's output: the plan as an indented tree with its expensive nodes
//! highlighted when the server returned structured JSON, the server's own
//! text otherwise.

use super::*;

/// One explained statement: the same header line a result section
/// has, and below it the plan — a tree for stock PostgreSQL's JSON, the
/// server's own output verbatim for every other backend (and for PostgreSQL
/// output that isn't the JSON we asked for, e.g. a user-written `EXPLAIN
/// ANALYZE`).
///
/// Takes the Arc-shared statement for the same reason [`StatementSection`]
/// does: the prop diffs by pointer, so the plan is parsed once per run rather
/// than once per render.
#[component]
pub(super) fn PlanSection(index: usize, structured: bool, result: SharedStatement) -> Element {
    let display = match &result.outcome {
        StatementOutcome::Rows(rows) => PlanDisplay::from_result(structured, rows),
        // An EXPLAIN returns rows on every backend hubro speaks; this is the
        // shape the type allows rather than one seen in practice.
        StatementOutcome::Affected(rows) => PlanDisplay::Raw(rows_affected_line(*rows)),
    };
    let cut_off =
        matches!(&result.outcome, StatementOutcome::Rows(rows) if plan_reached_cell_cap(rows));
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
            if cut_off {
                div { class: "px-4 pt-2",
                    Banner {
                        kind: BannerKind::Warning,
                        message: format!(
                            "Plan cut off — the server's output is larger than the {} MiB hubro \
                             fetches, so only its beginning is shown.",
                            PLAN_CELL_CAP / (1024 * 1024),
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
