//! Query plans (FRE-119): the `EXPLAIN` statement a connection understands,
//! and PostgreSQL's `EXPLAIN (FORMAT JSON)` output parsed into a tree.
//!
//! Two halves, deliberately separated:
//!
//! - [`explain_statement`] builds the statement to run. It only ever *adds* an
//!   `EXPLAIN` prefix — never `ANALYZE`, never anything else — and leaves a
//!   statement that already starts with `EXPLAIN` exactly as the user wrote
//!   it. The result goes back through the ordinary script path, so the
//!   capability gate ([`script_refusal`](super::script_refusal)) and the
//!   write-confirmation gate ([`needs_confirmation`](super::needs_confirmation))
//!   see it exactly as they would if the same text had been typed and Run
//!   pressed. That is the whole safety design: there is no second execution
//!   path to keep in step with the first. `EXPLAIN ANALYZE` really does run
//!   the statement, so the plan view must not be — and is not — a way around
//!   the prompt.
//! - [`PlanDisplay`] turns a finished `EXPLAIN` result into something to
//!   render: a parsed tree for stock PostgreSQL's JSON, or the raw text for
//!   everything else. Every failure degrades to raw text rather than
//!   erroring — a plan nobody can parse is still a plan the user can read.

use super::script::is_explain;
use super::value::QueryResult;

/// Why a connection offers no plan view. SQL Server has no `EXPLAIN`
/// statement: its estimated plan comes from `SET SHOWPLAN_XML ON`, a session
/// setting that must be alone in its batch and makes the *next* batch return
/// a plan instead of running. hubro's script path hands each statement to the
/// pool separately, so the setting and the statement it is meant to cover can
/// land on different connections — and the failure mode of getting that wrong
/// is executing a statement the user asked to have explained. Offering
/// nothing is the honest answer until a backend can hold one connection for
/// the pair (a follow-up, not this issue).
pub const NO_EXPLAIN: &str = "This connection has no EXPLAIN statement.";

/// How a connection produces a query plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExplainSupport {
    /// Prefix placed before a statement that isn't already an `EXPLAIN`.
    pub prefix: &'static str,
    /// Whether that prefix asks for PostgreSQL JSON, so the output can be
    /// parsed into a tree. `false` means "show whatever comes back".
    pub structured: bool,
}

impl ExplainSupport {
    /// Stock PostgreSQL — and the extensions that are genuinely PostgreSQL
    /// underneath (TimescaleDB, Citus), which
    /// [`PgFlavor::Postgres`](super::PgFlavor::Postgres) already covers.
    pub const PG_JSON: ExplainSupport = ExplainSupport {
        prefix: "EXPLAIN (FORMAT JSON)",
        structured: true,
    };

    /// The portable spelling, for the Postgres-wire engines that are not
    /// PostgreSQL. `FORMAT JSON` is a PostgreSQL option and the
    /// reimplementations vary on it; plain `EXPLAIN` is what they all
    /// document. Structured rendering for those is a per-engine verification
    /// nobody has done, and claiming it without one is how a plan view starts
    /// showing a tree that isn't what the server said.
    pub const PG_TEXT: ExplainSupport = ExplainSupport {
        prefix: "EXPLAIN",
        structured: false,
    };

    /// SQLite. Its `EXPLAIN QUERY PLAN` returns rows rather than a document,
    /// which the raw pane renders as the table it is.
    pub const SQLITE: ExplainSupport = ExplainSupport {
        prefix: "EXPLAIN QUERY PLAN",
        structured: false,
    };
}

/// The statement to run to explain `sql`.
///
/// A statement the user already wrote as an `EXPLAIN` is returned untouched:
/// re-prefixing it would be a syntax error, and — more to the point — the
/// options they chose are theirs, including an `ANALYZE` that executes. The
/// output of such a statement still reaches [`PlanDisplay`], which renders a
/// tree when it happens to be JSON and the raw text when it isn't.
///
/// **This never adds `ANALYZE`.** A prefix-only rewrite cannot turn a
/// statement that would not have run into one that does, and every statement
/// it produces is gated by the same two checks a typed statement is — see the
/// module docs, and `explaining_never_loosens_the_gate` in
/// [`super::script`]'s tests, which pins that as a property rather than a
/// promise.
pub fn explain_statement(sql: &str, support: ExplainSupport) -> String {
    let sql = sql.trim();
    if is_explain(sql) {
        return sql.to_string();
    }
    format!("{} {sql}", support.prefix)
}

/// What the editor should show for one finished `EXPLAIN` statement.
#[derive(Debug, Clone, PartialEq)]
pub enum PlanDisplay {
    /// A parsed PostgreSQL plan. Boxed because a tree is an order of
    /// magnitude wider than the string beside it, and the raw variant is the
    /// common one — every backend but stock PostgreSQL lands there.
    Tree(Box<PlanTree>),
    /// The server's output as text, in a monospace pane. Every backend that
    /// isn't stock PostgreSQL lands here, and so does stock PostgreSQL when
    /// the output isn't the JSON we asked for — a user-written `EXPLAIN
    /// ANALYZE`, a future output format, or anything the parser doesn't
    /// recognize.
    Raw(String),
}

impl PlanDisplay {
    /// Renders one `EXPLAIN` result. `structured` is
    /// [`ExplainSupport::structured`] for the connection that ran it; a
    /// structured result that doesn't parse degrades to [`Self::Raw`] rather
    /// than reporting an error, because the text is still the plan.
    pub fn from_result(structured: bool, result: &QueryResult) -> PlanDisplay {
        if structured {
            if let Some(tree) = first_cell(result).and_then(|cell| PlanTree::parse(&cell)) {
                return PlanDisplay::Tree(Box::new(tree));
            }
        }
        PlanDisplay::Raw(raw_text(result))
    }
}

/// The first cell of a result, when it has one — where PostgreSQL puts the
/// whole JSON document.
fn first_cell(result: &QueryResult) -> Option<String> {
    result.rows.first()?.first().map(|value| value.display())
}

/// A result as plain text: one line per row, cells separated by ` | `, with a
/// header line only when there is more than one column (PostgreSQL's text
/// `EXPLAIN` is one nameless column of plan lines, and a `QUERY PLAN` header
/// above it would be noise; SQLite's four-column `EXPLAIN QUERY PLAN` needs
/// its names to be readable).
fn raw_text(result: &QueryResult) -> String {
    let mut lines: Vec<String> = Vec::with_capacity(result.rows.len() + 1);
    if result.columns.len() > 1 {
        lines.push(
            result
                .columns
                .iter()
                .map(|c| c.name.clone())
                .collect::<Vec<_>>()
                .join(" | "),
        );
    }
    for row in &result.rows {
        lines.push(
            row.iter()
                .map(|value| value.display())
                .collect::<Vec<_>>()
                .join(" | "),
        );
    }
    lines.join("\n")
}

/// A parsed PostgreSQL plan: the root node, plus the two timings `EXPLAIN
/// ANALYZE` adds.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanTree {
    pub root: PlanNode,
    /// The root's `Total Cost` — the denominator of every node's
    /// [`PlanNode::cost_share`].
    pub total_cost: f64,
    pub planning_ms: Option<f64>,
    pub execution_ms: Option<f64>,
}

/// One node of a plan.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanNode {
    /// `Node Type` — "Seq Scan", "Hash Join", …
    pub node_type: String,
    /// What the node reads, already worded as a suffix: `on users`, `using
    /// users_pkey on users`. `None` for nodes that read no relation.
    pub target: Option<String>,
    pub startup_cost: Option<f64>,
    pub total_cost: Option<f64>,
    /// The cost this node adds on its own: its `Total Cost` minus its
    /// children's, floored at zero. This is what "expensive" is measured on —
    /// a node's own `Total Cost` includes everything beneath it, so ranking by
    /// it would always crown the root.
    ///
    /// Floored because the subtraction is an approximation the planner does
    /// not promise: a node under a nested loop is costed per iteration while
    /// its parent's total counts every one, so a child total can exceed the
    /// parent's. Every plan reader worth the name (`explain.depesz.com`,
    /// pgAdmin) computes exclusive cost the same way and clamps it the same
    /// way; a negative "cost added" would be a worse answer than zero.
    pub self_cost: f64,
    /// [`Self::self_cost`] as a fraction of the whole plan's cost, `0.0` when
    /// the plan's total cost is zero or missing.
    pub cost_share: f64,
    /// Whether this is one of the nodes the plan spends its cost in — see
    /// [`EXPENSIVE_SHARE`].
    pub expensive: bool,
    /// The planner's row estimate.
    pub plan_rows: Option<f64>,
    /// Rows actually produced per loop, and the time to produce them —
    /// present only under `ANALYZE`.
    pub actual_rows: Option<f64>,
    pub actual_ms: Option<f64>,
    pub loops: Option<f64>,
    pub children: Vec<PlanNode>,
}

/// The share of a plan's total cost a node must add on its own to count as
/// expensive.
///
/// One threshold rather than a gradient, because the highlight answers one
/// question — *where does this plan spend its cost?* — and a gradient answers
/// it in a way you have to squint at. A fifth of the plan in a single node is
/// where looking is worth it: at most four nodes can cross it, so the
/// highlight stays a pointer rather than a wash.
///
/// A consequence worth stating rather than special-casing: a one-node plan
/// highlights its only node, which holds 100% of the cost. That is the honest
/// answer to the question being asked, not a bug — the cost is all there.
pub const EXPENSIVE_SHARE: f64 = 0.2;

impl PlanTree {
    /// Parses `EXPLAIN (FORMAT JSON)` output, or `None` when it is not a plan
    /// document this understands — a different format, a different server, a
    /// truncated cell, anything. Never panics and never errors: the caller
    /// falls back to showing the text.
    ///
    /// Nesting is bounded by `serde_json`'s own recursion limit (128 levels,
    /// two per plan level), which rejects a pathological document before this
    /// recurses into it.
    pub fn parse(json: &str) -> Option<PlanTree> {
        let parsed: serde_json::Value = serde_json::from_str(json).ok()?;
        // Postgres wraps the document in a one-element array; accept a bare
        // object too, since that is what a caller who unwrapped it would pass.
        let entry = match &parsed {
            serde_json::Value::Array(items) => items.first()?,
            other => other,
        };
        let mut root = node_from(entry.get("Plan")?)?;
        let total_cost = root.total_cost.unwrap_or(0.0);
        mark_expensive(&mut root, total_cost);
        Some(PlanTree {
            root,
            total_cost,
            planning_ms: number(entry.get("Planning Time")),
            execution_ms: number(entry.get("Execution Time")),
        })
    }

    /// The nodes in display order (depth-first, parents before children),
    /// each with its indent depth.
    ///
    /// Borrows rather than clones: a node owns its subtree, so handing out
    /// owned copies would duplicate the whole plan once per level.
    pub fn rows(&self) -> Vec<(usize, &PlanNode)> {
        let mut out = Vec::new();
        push_rows(&self.root, 0, &mut out);
        out
    }
}

fn push_rows<'a>(node: &'a PlanNode, depth: usize, out: &mut Vec<(usize, &'a PlanNode)>) {
    out.push((depth, node));
    for child in &node.children {
        push_rows(child, depth + 1, out);
    }
}

impl PlanNode {
    /// The node's one-line name: its type plus what it reads.
    pub fn label(&self) -> String {
        match &self.target {
            Some(target) => format!("{} {target}", self.node_type),
            None => self.node_type.clone(),
        }
    }
}

/// Builds a node (and its subtree) from one `Plan` object. `None` when the
/// value isn't an object with a `Node Type` string — the one field every
/// PostgreSQL plan node has, and so the marker that this is a plan at all.
fn node_from(value: &serde_json::Value) -> Option<PlanNode> {
    let node_type = value.get("Node Type")?.as_str()?.to_string();
    let children: Vec<PlanNode> = match value.get("Plans") {
        Some(serde_json::Value::Array(items)) => {
            items.iter().filter_map(node_from).collect::<Vec<_>>()
        }
        _ => Vec::new(),
    };
    let total_cost = number(value.get("Total Cost"));
    // Exclusive cost: what this node adds over its children. Floored at zero
    // — see `PlanNode::self_cost`.
    let children_cost: f64 = children.iter().filter_map(|c| c.total_cost).sum();
    let self_cost = (total_cost.unwrap_or(0.0) - children_cost).max(0.0);
    Some(PlanNode {
        node_type,
        target: target_of(value),
        startup_cost: number(value.get("Startup Cost")),
        total_cost,
        self_cost,
        // Filled in by `mark_expensive` once the whole plan's total is known.
        cost_share: 0.0,
        expensive: false,
        plan_rows: number(value.get("Plan Rows")),
        actual_rows: number(value.get("Actual Rows")),
        actual_ms: number(value.get("Actual Total Time")),
        loops: number(value.get("Actual Loops")),
        children,
    })
}

/// What a node reads, as a label suffix. Index name first when there is one,
/// since "Index Scan using users_pkey on users" is how PostgreSQL's own text
/// output words it; an aliased relation gets the alias in parentheses, which
/// is the only way to tell two scans of the same table apart in a self-join.
fn target_of(value: &serde_json::Value) -> Option<String> {
    let index = value.get("Index Name").and_then(|v| v.as_str());
    let relation = value.get("Relation Name").and_then(|v| v.as_str());
    let alias = value.get("Alias").and_then(|v| v.as_str());
    let mut label = match (index, relation) {
        (Some(index), Some(relation)) => format!("using {index} on {relation}"),
        (Some(index), None) => format!("using {index}"),
        (None, Some(relation)) => format!("on {relation}"),
        (None, None) => return None,
    };
    if let (Some(relation), Some(alias)) = (relation, alias) {
        if alias != relation {
            label.push_str(&format!(" ({alias})"));
        }
    }
    Some(label)
}

/// A JSON number as `f64`, ignoring anything that isn't one. Strings are not
/// accepted: PostgreSQL emits plan costs as JSON numbers, and coercing a
/// string here would only invent a cost for output that isn't a plan.
fn number(value: Option<&serde_json::Value>) -> Option<f64> {
    value?.as_f64()
}

/// Fills in every node's [`PlanNode::cost_share`] and
/// [`PlanNode::expensive`] against the plan's total cost.
///
/// A total of zero (or a plan with no costs at all) marks nothing: dividing
/// by it would make every node either infinite or NaN, and "everything is
/// expensive" is no more useful than "nothing is".
fn mark_expensive(node: &mut PlanNode, total: f64) {
    if total > 0.0 {
        node.cost_share = node.self_cost / total;
        node.expensive = node.cost_share >= EXPENSIVE_SHARE;
    }
    for child in &mut node.children {
        mark_expensive(child, total);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::value::{ColumnInfo, Value};

    fn result(columns: &[&str], rows: &[&[&str]]) -> QueryResult {
        QueryResult {
            columns: columns
                .iter()
                .map(|name| ColumnInfo {
                    name: (*name).to_string(),
                })
                .collect(),
            rows: rows
                .iter()
                .map(|row| row.iter().map(|c| Value::Text((*c).to_string())).collect())
                .collect(),
        }
    }

    #[test]
    fn a_statement_gets_its_dialects_explain_prefix() {
        assert_eq!(
            explain_statement("SELECT 1", ExplainSupport::PG_JSON),
            "EXPLAIN (FORMAT JSON) SELECT 1"
        );
        assert_eq!(
            explain_statement("SELECT 1", ExplainSupport::PG_TEXT),
            "EXPLAIN SELECT 1"
        );
        assert_eq!(
            explain_statement("  SELECT 1\n", ExplainSupport::SQLITE),
            "EXPLAIN QUERY PLAN SELECT 1"
        );
    }

    #[test]
    fn a_statement_that_is_already_an_explain_is_left_alone() {
        // Re-prefixing would be a syntax error, and the options the user
        // chose — including an ANALYZE that executes — are theirs.
        for sql in [
            "EXPLAIN SELECT 1",
            "explain analyze select 1",
            "EXPLAIN (ANALYZE, FORMAT JSON) SELECT 1",
            "-- a comment first\nEXPLAIN SELECT 1",
        ] {
            assert_eq!(
                explain_statement(sql, ExplainSupport::PG_JSON),
                sql.trim(),
                "{sql:?}"
            );
        }
    }

    /// A `{"Plan": …}` document wrapped the way Postgres wraps it.
    fn document(plan: serde_json::Value) -> String {
        serde_json::json!([{ "Plan": plan }]).to_string()
    }

    #[test]
    fn a_single_node_plan_parses_with_its_costs_and_rows() {
        let tree = PlanTree::parse(&document(serde_json::json!({
            "Node Type": "Seq Scan",
            "Relation Name": "users",
            "Alias": "u",
            "Startup Cost": 0.0,
            "Total Cost": 35.5,
            "Plan Rows": 2550,
            "Plan Width": 4,
        })))
        .unwrap();
        assert_eq!(tree.root.node_type, "Seq Scan");
        assert_eq!(tree.root.label(), "Seq Scan on users (u)");
        assert_eq!(tree.root.total_cost, Some(35.5));
        assert_eq!(tree.root.plan_rows, Some(2550.0));
        assert_eq!(tree.root.children, vec![]);
        assert_eq!(tree.total_cost, 35.5);
        // The whole plan is this node, so this node is where the cost is.
        assert_eq!(tree.root.self_cost, 35.5);
        assert!(tree.root.expensive);
        assert_eq!(tree.rows().len(), 1);
    }

    #[test]
    fn analyze_timings_are_read_when_present() {
        let json = serde_json::json!([{
            "Plan": {
                "Node Type": "Seq Scan",
                "Total Cost": 10.0,
                "Plan Rows": 100,
                "Actual Rows": 7,
                "Actual Total Time": 0.25,
                "Actual Loops": 3,
            },
            "Planning Time": 0.12,
            "Execution Time": 1.75,
        }])
        .to_string();
        let tree = PlanTree::parse(&json).unwrap();
        assert_eq!(tree.planning_ms, Some(0.12));
        assert_eq!(tree.execution_ms, Some(1.75));
        assert_eq!(tree.root.actual_rows, Some(7.0));
        assert_eq!(tree.root.actual_ms, Some(0.25));
        assert_eq!(tree.root.loops, Some(3.0));
        // A plain EXPLAIN leaves all three unset rather than guessing.
        let plain = PlanTree::parse(&document(serde_json::json!({
            "Node Type": "Seq Scan", "Total Cost": 10.0,
        })))
        .unwrap();
        assert_eq!(plain.planning_ms, None);
        assert_eq!(plain.root.actual_rows, None);
    }

    /// A node with the given total cost and children.
    fn node(kind: &str, total: f64, children: Vec<serde_json::Value>) -> serde_json::Value {
        let mut value = serde_json::json!({ "Node Type": kind, "Total Cost": total });
        if !children.is_empty() {
            value["Plans"] = serde_json::Value::Array(children);
        }
        value
    }

    #[test]
    fn a_nested_plan_keeps_its_shape_and_order() {
        let tree = PlanTree::parse(&document(node(
            "Hash Join",
            100.0,
            vec![
                node("Seq Scan", 30.0, vec![]),
                node("Hash", 50.0, vec![node("Index Scan", 20.0, vec![])]),
            ],
        )))
        .unwrap();
        let rows = tree.rows();
        let shape: Vec<(usize, &str)> = rows
            .iter()
            .map(|(depth, node)| (*depth, node.node_type.as_str()))
            .collect();
        assert_eq!(
            shape,
            [
                (0, "Hash Join"),
                (1, "Seq Scan"),
                (1, "Hash"),
                (2, "Index Scan"),
            ]
        );
        // Exclusive cost: the join adds 100 - (30 + 50) = 20 of its own.
        assert_eq!(rows[0].1.self_cost, 20.0);
        assert_eq!(rows[2].1.self_cost, 30.0);
    }

    #[test]
    fn a_deeply_nested_plan_parses_to_its_full_depth() {
        // 40 levels — deeper than any real plan, and deep enough that a
        // parser with a shallow cap or a flattener that lost depth would
        // show it. (serde_json's own 128-level recursion limit is the real
        // bound; a document past it is rejected before this parser sees it,
        // which is a degrade, not a panic.)
        let mut plan = node("Seq Scan", 1.0, vec![]);
        for level in 1..40 {
            plan = node("Nested Loop", 1.0 + level as f64, vec![plan]);
        }
        let tree = PlanTree::parse(&document(plan)).unwrap();
        let rows = tree.rows();
        assert_eq!(rows.len(), 40);
        assert_eq!(rows[0].0, 0);
        assert_eq!(rows[39].0, 39);
        assert_eq!(rows[39].1.node_type, "Seq Scan");
    }

    #[test]
    fn expensive_is_a_fifth_of_the_plans_cost_in_one_node() {
        // Root total 100: children add 20, 19.9 and 0.1 of their own, and the
        // root itself adds the remaining 60.
        let tree = PlanTree::parse(&document(node(
            "Append",
            100.0,
            vec![
                node("Seq Scan", 20.0, vec![]),
                node("Seq Scan", 19.9, vec![]),
                node("Seq Scan", 0.1, vec![]),
            ],
        )))
        .unwrap();
        let rows = tree.rows();
        let flags: Vec<bool> = rows.iter().map(|(_, node)| node.expensive).collect();
        // Exactly at the threshold counts; a hair under does not.
        assert_eq!(flags, [true, true, false, false]);
        assert_eq!(rows[1].1.cost_share, 0.2);
    }

    #[test]
    fn a_costless_plan_marks_nothing_expensive() {
        // No division by zero, no NaN share, and no "everything is expensive"
        // — which says exactly as much as "nothing is".
        for plan in [
            serde_json::json!({ "Node Type": "Result", "Total Cost": 0.0 }),
            serde_json::json!({ "Node Type": "Result" }),
        ] {
            let tree = PlanTree::parse(&document(plan)).unwrap();
            assert!(!tree.root.expensive);
            assert_eq!(tree.root.cost_share, 0.0);
            assert!(tree.root.cost_share.is_finite());
        }
    }

    #[test]
    fn a_child_costing_more_than_its_parent_floors_at_zero() {
        // The planner costs a nested loop's inner side per iteration while the
        // loop's own total counts every one, so a child total can exceed its
        // parent's. Clamped, not negative.
        let tree = PlanTree::parse(&document(node(
            "Nested Loop",
            10.0,
            vec![node("Index Scan", 40.0, vec![])],
        )))
        .unwrap();
        assert_eq!(tree.root.self_cost, 0.0);
        assert!(!tree.root.expensive);
    }

    #[test]
    fn unexpected_json_degrades_instead_of_panicking() {
        for json in [
            "",
            "not json at all",
            "{",
            "[]",
            "{}",
            "null",
            "42",
            r#"[{"NotAPlan": 1}]"#,
            // A Plan that isn't an object, and one without the Node Type
            // every plan node has.
            r#"[{"Plan": 7}]"#,
            r#"[{"Plan": {"Total Cost": 1.0}}]"#,
            r#"[{"Plan": {"Node Type": 7}}]"#,
            // Text-format EXPLAIN output, which is what a user-written
            // `EXPLAIN ANALYZE` returns.
            "Seq Scan on users  (cost=0.00..35.50 rows=2550 width=4)",
        ] {
            assert_eq!(PlanTree::parse(json), None, "{json:?}");
        }
    }

    #[test]
    fn odd_but_parseable_plans_keep_what_they_can() {
        // Children that aren't nodes are dropped; the node itself survives.
        let tree = PlanTree::parse(
            r#"[{"Plan": {"Node Type": "Result", "Total Cost": 5.0,
                 "Plans": [7, {"no": "type"}], "Plan Rows": "many"}}]"#,
        )
        .unwrap();
        assert_eq!(tree.root.children, vec![]);
        // A non-numeric cost is absent, not coerced to something invented.
        assert_eq!(tree.root.plan_rows, None);
        assert_eq!(tree.root.total_cost, Some(5.0));
    }

    #[test]
    fn a_structured_result_that_does_not_parse_falls_back_to_its_text() {
        let plan = document(serde_json::json!({
            "Node Type": "Seq Scan", "Total Cost": 1.0,
        }));
        assert!(matches!(
            PlanDisplay::from_result(true, &result(&["QUERY PLAN"], &[&[&plan]])),
            PlanDisplay::Tree(_)
        ));
        // Same result, read from a connection that never asked for JSON.
        assert!(matches!(
            PlanDisplay::from_result(false, &result(&["QUERY PLAN"], &[&[&plan]])),
            PlanDisplay::Raw(_)
        ));
        // Structured, but the server sent text.
        assert_eq!(
            PlanDisplay::from_result(
                true,
                &result(&["QUERY PLAN"], &[&["Seq Scan on users"], &["  Filter: …"]]),
            ),
            PlanDisplay::Raw("Seq Scan on users\n  Filter: …".to_string())
        );
        // Structured, but no rows at all.
        assert_eq!(
            PlanDisplay::from_result(true, &result(&["QUERY PLAN"], &[])),
            PlanDisplay::Raw(String::new())
        );
    }

    #[test]
    fn a_multi_column_raw_result_keeps_its_headers() {
        // SQLite's EXPLAIN QUERY PLAN is a four-column table; without the
        // names the numbers mean nothing.
        assert_eq!(
            PlanDisplay::from_result(
                false,
                &result(
                    &["id", "parent", "notused", "detail"],
                    &[&["2", "0", "0", "SCAN artists"]],
                ),
            ),
            PlanDisplay::Raw(
                "id | parent | notused | detail\n2 | 0 | 0 | SCAN artists".to_string()
            )
        );
    }
}
