//! Matching and ranking for the schema sidebar's filter box (FRE-107).
//!
//! Deliberately free of Dioxus types: everything here is a pure function over
//! the already-introspected [`TableMeta`] list, so the sidebar's behaviour is
//! unit-testable without a renderer, and so the filter can never issue a
//! query. The UI in `sidebar.rs` owns the input text; this module owns what
//! that text *means*.
//!
//! The matching is deliberately tiered rather than a general fuzzy score:
//! a scroll-hunt is replaced by a filtered tree only if the user can predict
//! what typing three characters will do. Exact beats prefix beats
//! word-boundary beats plain substring beats subsequence, and only then do
//! position and length break ties — so `us` puts `users` above `census` above
//! `unit_specs`, every time.

use crate::db::TableMeta;

/// Prefixes that switch the box from table search to column search. Several
/// spellings because the short one is the documented form but the long ones
/// are what people type when they've forgotten it.
const COLUMN_PREFIXES: [&str; 4] = [":col", ":cols", ":column", ":columns"];

/// The canonical column-mode prefix, with its trailing space — what the
/// sidebar's toggle button writes into the box.
pub const COLUMN_PREFIX: &str = ":col ";

/// What the one filter input is searching. Both modes share
/// [`score`]; the mode only changes what gets scored and how the hits are
/// grouped for rendering (FRE-107).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterMode {
    /// The default: table and view names only.
    Tables,
    /// Column names, grouped under their owning table.
    Columns,
}

/// The filter box's text, parsed into a mode and the text to match on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub mode: FilterMode,
    /// The search text with the mode prefix and surrounding whitespace
    /// stripped. Empty means "no filter" — [`filter_tables`] then returns
    /// every table untouched, so `:col` on its own shows the plain tree with
    /// the mode armed rather than dumping every column in the database.
    pub needle: String,
}

/// Splits the raw input into a mode and a needle. The mode prefix has to be a
/// whole leading token — a table literally named `:column` is not a thing, but
/// `:columns_v2` matching in table mode is the less surprising reading.
pub fn parse_query(raw: &str) -> Query {
    let trimmed = raw.trim();
    let (head, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((head, rest)) => (head, rest),
        None => (trimmed, ""),
    };
    if COLUMN_PREFIXES.contains(&head.to_ascii_lowercase().as_str()) {
        Query {
            mode: FilterMode::Columns,
            needle: rest.trim().to_string(),
        }
    } else {
        Query {
            mode: FilterMode::Tables,
            needle: trimmed.to_string(),
        }
    }
}

/// Adds or removes the column-mode prefix, preserving whatever has already
/// been typed. Backs the `:col` toggle button, which keeps the box's text as
/// the single source of truth for the mode — the toggle and the typed prefix
/// can therefore never disagree.
pub fn toggle_column_mode(raw: &str) -> String {
    let query = parse_query(raw);
    match query.mode {
        FilterMode::Columns => query.needle,
        FilterMode::Tables => format!("{COLUMN_PREFIX}{}", query.needle),
    }
}

/// How well a needle matched, from best to worst. The tier dominates every
/// other tie-breaker: a word-boundary hit deep in a long name outranks a
/// mid-word hit near the front, because that is how a human reads the name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
    /// The whole name, ignoring case.
    Exact,
    /// The name starts with the needle.
    Prefix,
    /// The needle starts a word inside the name — after `_`, `.`, `-` or any
    /// other non-alphanumeric separator. This is what makes `roles` rank
    /// `user_roles` above `enrolments`.
    WordPrefix,
    /// The needle appears somewhere in the middle of a word.
    Substring,
    /// The needle's characters appear in order but not adjacently
    /// (`usrol` → `user_roles`).
    Subsequence,
}

/// A ranking key: smaller is better, and the derived `Ord` compares the
/// fields in declaration order, which *is* the ranking rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct MatchScore {
    tier: Tier,
    /// Character offset the match starts at — earlier wins.
    start: usize,
    /// Characters the match spans. Only interesting for subsequences, where
    /// a tight run reads more like the name than a scattered one.
    span: usize,
    /// Length of the whole name — with everything else equal, the shorter
    /// name is the more likely target.
    len: usize,
}

/// Whether `c` ends a word, i.e. whether a match starting right after it
/// counts as [`Tier::WordPrefix`]. Covers `_`, `.`, `-` and anything else
/// non-alphanumeric without enumerating separators per dialect.
fn is_boundary(c: char) -> bool {
    !c.is_alphanumeric()
}

/// Scores `needle` against `haystack`, case-insensitively. `None` means no
/// match at all — not even as a subsequence.
///
/// An empty needle matches everything perfectly, so callers that treat "no
/// text typed" as "no filter" get the same answer either way.
pub fn score(haystack: &str, needle: &str) -> Option<MatchScore> {
    score_with(haystack, needle, true)
}

/// Same, but with the subsequence fallback optional.
///
/// Qualified names (`public.users`) are scored with it *off* — see
/// [`score_table`]. Left on, the schema half donates its characters to every
/// table under it: on a `public` schema, `us` would match every single table
/// as a subsequence (`p-u-blic.…-s`), which is exactly the flat wall of names
/// the filter exists to avoid.
fn score_with(haystack: &str, needle: &str, subsequences: bool) -> Option<MatchScore> {
    // Compared as lowercased `char` vectors rather than byte slices: `str`
    // indices would desync the moment a name contains a multi-byte character
    // (Postgres identifiers are not ASCII-only), and the offsets here feed
    // the ranking. The allocation is fine at sidebar scale — a few hundred
    // tables, re-scored per keystroke.
    let hay: Vec<char> = haystack.chars().flat_map(char::to_lowercase).collect();
    let ned: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();
    let len = hay.len();
    if ned.is_empty() {
        return Some(MatchScore {
            tier: Tier::Exact,
            start: 0,
            span: 0,
            len,
        });
    }
    if ned.len() > len {
        return None;
    }
    if hay == ned {
        return Some(MatchScore {
            tier: Tier::Exact,
            start: 0,
            span: ned.len(),
            len,
        });
    }
    // Every contiguous occurrence, keeping the best-ranked one: the first
    // occurrence is not always the best, since a later one may start a word.
    let mut best: Option<MatchScore> = None;
    for start in 0..=(len - ned.len()) {
        if hay[start..start + ned.len()] != ned[..] {
            continue;
        }
        let tier = if start == 0 {
            Tier::Prefix
        } else if is_boundary(hay[start - 1]) {
            Tier::WordPrefix
        } else {
            Tier::Substring
        };
        let candidate = MatchScore {
            tier,
            start,
            span: ned.len(),
            len,
        };
        best = Some(best.map_or(candidate, |b| b.min(candidate)));
    }
    if best.is_some() || !subsequences {
        return best;
    }
    subsequence(&hay, &ned).map(|(start, end)| MatchScore {
        tier: Tier::Subsequence,
        start,
        span: end - start,
        len,
    })
}

/// Greedy left-to-right subsequence match, returning the half-open character
/// range it consumed. Greedy rather than optimal-span on purpose: it always
/// anchors on the earliest possible first character, which is both cheap and
/// the behaviour a user can predict from the name they're looking at.
fn subsequence(hay: &[char], ned: &[char]) -> Option<(usize, usize)> {
    let mut next = 0;
    let mut start = None;
    let mut end = 0;
    for (i, c) in hay.iter().enumerate() {
        if next < ned.len() && *c == ned[next] {
            start.get_or_insert(i);
            next += 1;
            end = i + 1;
        }
    }
    if next == ned.len() {
        Some((start.unwrap_or(0), end))
    } else {
        None
    }
}

/// The better of two optional scores (`None` being "no match").
fn best_of(a: Option<MatchScore>, b: Option<MatchScore>) -> Option<MatchScore> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (hit, None) | (None, hit) => hit,
    }
}

/// Scores a table against the needle on both its bare name and its
/// schema-qualified `schema.name` form, keeping the better of the two.
///
/// Both forms matter and neither subsumes the other: typing `public` has to
/// narrow to that schema (only the qualified form matches), while typing
/// `users` must not rank `public.users` below some unrelated table just
/// because the schema padded the name. The qualified form is longer, so the
/// bare name wins any tie by [`MatchScore::len`].
///
/// The qualified form is matched contiguously only: a schema is a *scope*, and
/// narrowing to one is a prefix operation, not a fuzzy one (see
/// [`score_with`]).
pub fn score_table(schema: Option<&str>, name: &str, needle: &str) -> Option<MatchScore> {
    let bare = score(name, needle);
    let qualified =
        schema.and_then(|schema| score_with(&format!("{schema}.{name}"), needle, false));
    best_of(bare, qualified)
}

/// Scores a column, qualified by its owning table rather than by its schema —
/// `:col users.id` is the natural way to say "the id on users".
///
/// Unlike [`score_table`], the qualified form is only tried when the needle
/// actually contains a `.`. The asymmetry is deliberate: a schema is part of a
/// table's identity and narrowing to one is a stated goal, but in column mode
/// the table name is a *scope*, and an implicit one would make `:col stripe`
/// dump every column of a table called `stripe_events` — turning a column
/// search into table search with extra steps.
fn score_column(table: &str, column: &str, needle: &str) -> Option<MatchScore> {
    let bare = score(column, needle);
    let qualified = needle
        .contains('.')
        .then(|| score_with(&format!("{table}.{column}"), needle, false))
        .flatten();
    best_of(bare, qualified)
}

/// One table that survived the filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableHit {
    /// Index into the slice handed to [`filter_tables`].
    pub index: usize,
    /// Indices into that table's `columns` that matched, in declaration
    /// order. Always empty in [`FilterMode::Tables`].
    pub columns: Vec<usize>,
}

/// Filters and ranks `tables` for `query`, best match first.
///
/// An empty needle is not a match-nothing filter but a match-everything one:
/// the result is every table in the order introspection returned them, so the
/// sidebar renders its unfiltered tree through exactly the same code path.
pub fn filter_tables(tables: &[TableMeta], query: &Query) -> Vec<TableHit> {
    if query.needle.is_empty() {
        return (0..tables.len())
            .map(|index| TableHit {
                index,
                columns: Vec::new(),
            })
            .collect();
    }
    let mut hits: Vec<(MatchScore, TableHit)> = Vec::new();
    for (index, table) in tables.iter().enumerate() {
        match query.mode {
            FilterMode::Tables => {
                if let Some(score) =
                    score_table(table.schema.as_deref(), &table.name, &query.needle)
                {
                    hits.push((
                        score,
                        TableHit {
                            index,
                            columns: Vec::new(),
                        },
                    ));
                }
            }
            FilterMode::Columns => {
                // A table's rank in column mode is its best column's rank, so
                // the table holding the closest-named column floats to the
                // top. The columns themselves stay in declaration order: they
                // read as a slice of the table's structure, and re-ranking
                // them would scramble that for no gain.
                let mut columns = Vec::new();
                let mut best = None;
                for (position, column) in table.columns.iter().enumerate() {
                    if let Some(score) = score_column(&table.name, &column.name, &query.needle) {
                        columns.push(position);
                        best = best_of(best, Some(score));
                    }
                }
                if let Some(score) = best {
                    hits.push((score, TableHit { index, columns }));
                }
            }
        }
    }
    // Ties break on the qualified name so the order is stable across renders
    // and independent of introspection order.
    hits.sort_by(|(a_score, a), (b_score, b)| {
        let (a_table, b_table) = (&tables[a.index], &tables[b.index]);
        a_score
            .cmp(b_score)
            .then_with(|| a_table.schema.cmp(&b_table.schema))
            .then_with(|| a_table.name.cmp(&b_table.name))
    });
    hits.into_iter().map(|(_, hit)| hit).collect()
}

/// Regroups ranked hits under their schemas for rendering, so the result
/// stays a tree instead of collapsing into a flat list (FRE-107).
///
/// Groups appear in the order their first (i.e. best-ranked) hit does, which
/// puts the schema holding the strongest match at the top. A schema with no
/// surviving hits produces no group at all — that is what "non-matching
/// schemas collapsed" amounts to in a tree with no per-schema expansion
/// state.
///
/// Grouping is by identity rather than by consecutive runs, so a backend that
/// returns tables interleaved across schemas still gets one header per
/// schema.
pub fn group_by_schema(
    tables: &[TableMeta],
    hits: &[TableHit],
) -> Vec<(Option<String>, Vec<TableHit>)> {
    let mut groups: Vec<(Option<String>, Vec<TableHit>)> = Vec::new();
    for hit in hits {
        let schema = &tables[hit.index].schema;
        match groups.iter_mut().find(|(key, _)| key == schema) {
            Some((_, group)) => group.push(hit.clone()),
            None => groups.push((schema.clone(), vec![hit.clone()])),
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{ColumnMeta, Generated, TableKind, TypeDetail};

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
                    generated: Generated::Never,
                    type_detail: TypeDetail::Plain,
                })
                .collect(),
            indexes: vec![],
            foreign_keys: vec![],
            restriction: None,
        }
    }

    /// The names of the tables a query keeps, in ranked order.
    fn ranked(tables: &[TableMeta], raw: &str) -> Vec<String> {
        filter_tables(tables, &parse_query(raw))
            .iter()
            .map(|hit| tables[hit.index].name.clone())
            .collect()
    }

    fn tier(haystack: &str, needle: &str) -> Option<Tier> {
        score(haystack, needle).map(|s| s.tier)
    }

    #[test]
    fn substring_matches_anywhere_in_the_name() {
        assert_eq!(tier("user_roles", "user"), Some(Tier::Prefix));
        assert_eq!(tier("user_roles", "roles"), Some(Tier::WordPrefix));
        assert_eq!(tier("user_roles", "ser"), Some(Tier::Substring));
        assert_eq!(tier("user_roles", "user_roles"), Some(Tier::Exact));
        assert_eq!(tier("user_roles", "zebra"), None);
    }

    #[test]
    fn subsequence_matches_scattered_characters() {
        assert_eq!(tier("user_roles", "usrol"), Some(Tier::Subsequence));
        // Order still matters — a subsequence is not an anagram.
        assert_eq!(tier("user_roles", "lorsu"), None);
        // And a needle longer than the name can never match.
        assert_eq!(tier("id", "identifier"), None);
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert_eq!(tier("Users", "users"), Some(Tier::Exact));
        assert_eq!(tier("users", "USERS"), Some(Tier::Exact));
        assert_eq!(tier("AuditLog", "log"), Some(Tier::Substring));
        let tables = [table(Some("Public"), "Users", &["id"])];
        assert_eq!(ranked(&tables, "public.us"), ["Users"]);
    }

    #[test]
    fn prefix_matches_rank_before_later_ones() {
        let tables = [
            table(None, "census", &["id"]),
            table(None, "unit_specs", &["id"]),
            table(None, "users", &["id"]),
        ];
        // Prefix, then the mid-word substring, then the scattered
        // subsequence last.
        assert_eq!(ranked(&tables, "us"), ["users", "census", "unit_specs"]);
    }

    #[test]
    fn exact_matches_rank_before_prefixes() {
        let tables = [
            table(None, "user_roles", &["id"]),
            table(None, "users", &["id"]),
            table(None, "user", &["id"]),
        ];
        // Both prefixes; the shorter name wins that tie.
        assert_eq!(ranked(&tables, "user"), ["user", "users", "user_roles"]);
    }

    #[test]
    fn word_boundaries_outrank_mid_word_hits() {
        let tables = [
            table(None, "enrolments", &["id"]),
            table(None, "user_roles", &["id"]),
        ];
        assert_eq!(ranked(&tables, "rol"), ["user_roles", "enrolments"]);
    }

    #[test]
    fn schema_qualified_names_match() {
        let tables = [
            table(Some("public"), "users", &["id"]),
            table(Some("audit"), "events", &["id"]),
        ];
        // A bare schema name narrows to that schema…
        assert_eq!(ranked(&tables, "audit"), ["events"]);
        // …as does the qualified form, and a dot alone doesn't match either.
        assert_eq!(ranked(&tables, "public.us"), ["users"]);
        assert!(ranked(&tables, "public.zzz").is_empty());
    }

    #[test]
    fn a_bare_name_outranks_the_same_name_behind_a_schema() {
        // "users" is exact against the bare name and only a word-prefix
        // against "public.users", so the exact reading has to win.
        let tables = [
            table(Some("public"), "users_audit", &["id"]),
            table(Some("public"), "users", &["id"]),
        ];
        assert_eq!(ranked(&tables, "users"), ["users", "users_audit"]);
    }

    #[test]
    fn a_schema_never_lends_its_letters_to_a_fuzzy_match() {
        // Caught in the app: with subsequences allowed on the qualified name,
        // "us" matched *every* table in "public" (p-U-blic.enrolment-S), which
        // is the flat wall of names the filter is supposed to prevent.
        let tables = [
            table(Some("public"), "enrolments", &["id"]),
            table(Some("public"), "census", &["id"]),
        ];
        assert_eq!(ranked(&tables, "us"), ["census"]);
        // The bare name still matches fuzzily.
        assert_eq!(ranked(&tables, "enrlm"), ["enrolments"]);
    }

    #[test]
    fn an_empty_needle_keeps_every_table_in_order() {
        let tables = [
            table(Some("public"), "zebras", &["id"]),
            table(Some("audit"), "events", &["id"]),
        ];
        assert_eq!(ranked(&tables, ""), ["zebras", "events"]);
        assert_eq!(ranked(&tables, "   "), ["zebras", "events"]);
    }

    #[test]
    fn column_mode_is_entered_by_prefix() {
        assert_eq!(
            parse_query(":col stripe"),
            Query {
                mode: FilterMode::Columns,
                needle: "stripe".into(),
            }
        );
        // Alternate spellings, casing, and extra whitespace all land the same.
        for raw in [":COLUMNS   stripe", ":cols stripe", ":column stripe"] {
            assert_eq!(parse_query(raw).mode, FilterMode::Columns);
            assert_eq!(parse_query(raw).needle, "stripe");
        }
        // The prefix alone arms the mode with nothing to search for.
        assert_eq!(
            parse_query(":col"),
            Query {
                mode: FilterMode::Columns,
                needle: String::new(),
            }
        );
        // Only a whole leading token counts as the prefix.
        assert_eq!(parse_query(":columns_v2").mode, FilterMode::Tables);
        assert_eq!(parse_query("stripe").mode, FilterMode::Tables);
        assert_eq!(parse_query("a :col b").needle, "a :col b");
    }

    #[test]
    fn the_column_toggle_round_trips_the_prefix() {
        assert_eq!(toggle_column_mode("stripe"), ":col stripe");
        assert_eq!(toggle_column_mode(":col stripe"), "stripe");
        assert_eq!(toggle_column_mode(""), ":col ");
        assert_eq!(toggle_column_mode(":col "), "");
    }

    #[test]
    fn column_mode_searches_columns_and_groups_them_under_their_table() {
        let tables = [
            table(None, "invoices", &["id", "stripe_customer_id", "total"]),
            table(None, "customers", &["id", "email"]),
            table(None, "payments", &["id", "stripe_charge_id", "stripe_fee"]),
        ];
        let hits = filter_tables(&tables, &parse_query(":col stripe"));
        let named: Vec<(&str, Vec<&str>)> = hits
            .iter()
            .map(|hit| {
                let table = &tables[hit.index];
                (
                    table.name.as_str(),
                    hit.columns
                        .iter()
                        .map(|&c| table.columns[c].name.as_str())
                        .collect(),
                )
            })
            .collect();
        assert_eq!(
            named,
            [
                ("payments", vec!["stripe_charge_id", "stripe_fee"]),
                ("invoices", vec!["stripe_customer_id"]),
            ]
        );
        // Ranked by each table's best column: every hit here is a prefix
        // match, so the shortest matching column name ("stripe_fee") carries
        // payments above invoices.

        // A table-qualified needle narrows to that table.
        let hits = filter_tables(&tables, &parse_query(":col invoices.id"));
        assert_eq!(hits.len(), 1);
        assert_eq!(tables[hits[0].index].name, "invoices");
    }

    #[test]
    fn column_mode_ignores_unqualified_table_names() {
        let tables = [table(None, "customers", &["id", "email"])];
        // The table name alone is not a column search…
        assert!(filter_tables(&tables, &parse_query(":col customers")).is_empty());
        // …but it is a scope once the needle says so with a dot.
        assert_eq!(
            filter_tables(&tables, &parse_query(":col customers.em")).len(),
            1
        );
        // The same needle in the default mode still finds the table.
        assert_eq!(ranked(&tables, "customers"), ["customers"]);
    }

    #[test]
    fn grouping_keeps_one_header_per_schema_in_ranked_order() {
        let tables = [
            table(Some("audit"), "user_events", &["id"]),
            table(Some("public"), "users", &["id"]),
            table(Some("audit"), "users_history", &["id"]),
            table(Some("other"), "widgets", &["id"]),
        ];
        let hits = filter_tables(&tables, &parse_query("users"));
        let groups = group_by_schema(&tables, &hits);
        let shape: Vec<(Option<&str>, Vec<&str>)> = groups
            .iter()
            .map(|(schema, group)| {
                (
                    schema.as_deref(),
                    group
                        .iter()
                        .map(|hit| tables[hit.index].name.as_str())
                        .collect(),
                )
            })
            .collect();
        // "public" leads on its exact hit; "audit" contributes both of its
        // tables under a single header; "other" has no hit and no header.
        assert_eq!(
            shape,
            [
                (Some("public"), vec!["users"]),
                (Some("audit"), vec!["users_history", "user_events"]),
            ]
        );
    }
}
