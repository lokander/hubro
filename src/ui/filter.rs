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
//! span, position and length break ties — so `ord` puts `orders` above
//! `records` above `overdraft`, every time.

use crate::db::TableMeta;

/// Prefixes that switch the box from table search to column search. Several
/// spellings because the short one is the documented form but the long ones
/// are what people type when they've forgotten it.
const COLUMN_PREFIXES: [&str; 4] = [":col", ":cols", ":column", ":columns"];

/// The canonical column-mode prefix, with its trailing space — what the
/// sidebar's toggle button writes into the box.
const COLUMN_PREFIX: &str = ":col ";

/// Shortest needle the subsequence fallback is offered for.
///
/// Below this it is all noise and no signal: on a realistic 50-table schema
/// `us` keeps 22 tables with subsequences on and 6 with them off, and the 16
/// extra are `refunds`, `coupons`, `job_runs` — names no one was reaching for.
/// The motivating example (`usrol` → `user_roles`) is five characters, so
/// nothing the feature is for is lost.
const MIN_SUBSEQUENCE_LEN: usize = 3;

/// What the one filter input is searching. Both modes share the same matcher;
/// the mode only changes what gets scored and how the hits are grouped for
/// rendering (FRE-107).
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
///
/// `span` sits ahead of `start` deliberately, and it only ever decides
/// anything in the subsequence tier: every contiguous tier sets `span` to the
/// needle's length, so it is constant across those candidates and the
/// comparison falls straight through to `start`. In the subsequence tier the
/// span *is* the quality of the match, and ordering by `start` first put
/// `audit_events` (`a-U-dit_event-S`, span 11) above `job_runs` (span 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct MatchScore {
    tier: Tier,
    /// Characters the match spans — a tight run reads more like the name than
    /// a scattered one.
    span: usize,
    /// Character offset the match starts at — earlier wins.
    start: usize,
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

/// Lowercases a name into the `char` vector the matcher works on.
///
/// `char`s rather than bytes because `str` indices would desync the moment a
/// name contains a multi-byte character (Postgres identifiers are not
/// ASCII-only) and the offsets here feed the ranking. `to_lowercase` rather
/// than `to_ascii_lowercase` because the non-ASCII case is exactly the one
/// that needs folding: `ÅRSRAPPORT` has to match `årsrapport`.
fn lowercased(name: &str) -> Vec<char> {
    name.chars().flat_map(char::to_lowercase).collect()
}

/// Writes the lowercased `prefix.name` form into `buf`.
///
/// Takes both halves already lowercased and reuses the caller's buffer: column
/// mode scores every column of every table on each keystroke (~10k on the
/// 300-table databases this feature exists for), and a `format!` per candidate
/// was three quarters of the cost.
fn qualify_into(buf: &mut Vec<char>, prefix: &[char], name: &[char]) {
    buf.clear();
    buf.reserve(prefix.len() + name.len() + 1);
    buf.extend_from_slice(prefix);
    buf.push('.');
    buf.extend_from_slice(name);
}

/// Scores an already-lowercased needle against an already-lowercased
/// haystack. `None` means no match at all.
///
/// An empty needle matches everything perfectly, so callers that treat "no
/// text typed" as "no filter" get the same answer either way.
///
/// `subsequences` turns the fuzzy fallback off. Qualified names
/// (`public.users`) are scored with it off — see [`score_table_prepared`].
fn score_prepared(hay: &[char], ned: &[char], subsequences: bool) -> Option<MatchScore> {
    let len = hay.len();
    if ned.is_empty() {
        return Some(MatchScore {
            tier: Tier::Exact,
            span: 0,
            start: 0,
            len,
        });
    }
    if ned.len() > len {
        return None;
    }
    if hay == ned {
        return Some(MatchScore {
            tier: Tier::Exact,
            span: ned.len(),
            start: 0,
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
            span: ned.len(),
            start,
            len,
        };
        best = Some(best.map_or(candidate, |b| b.min(candidate)));
    }
    if best.is_some() || !subsequences || ned.len() < MIN_SUBSEQUENCE_LEN {
        return best;
    }
    subsequence(hay, ned).map(|(start, end)| MatchScore {
        tier: Tier::Subsequence,
        span: end - start,
        start,
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

/// The strongest tier a *qualified* name may claim.
///
/// A qualifier — the schema in front of a table, the table in front of a
/// column — is a **scope**, and narrowing to a scope is something you do by
/// naming it, not by sharing letters with it. Anything weaker than a word
/// boundary means the needle landed mid-word inside the qualifier, which is
/// not a scope narrow: on Postgres, `ub`, `bl`, `ic`, `ubli`… are each a
/// substring of `public` and would otherwise hand back that entire schema —
/// the full unfiltered table list, at a tier *above* subsequence, with no cue
/// as to why.
///
/// It cannot be tightened to `Prefix`: a schema's second word must still
/// narrow, and `payroll` against `hr_payroll.runs` is a [`Tier::WordPrefix`].
const MAX_QUALIFIED_TIER: Tier = Tier::WordPrefix;

/// Scores an already-lowercased table name against the needle on both its
/// bare form and its schema-qualified `schema.name` form, keeping the better
/// of the two. `buf` is scratch, reused across calls.
///
/// Both forms matter and neither subsumes the other: typing `public` has to
/// narrow to that schema (only the qualified form matches), while typing
/// `users` must not rank `public.users` below some unrelated table just
/// because the schema padded the name. The qualified form is longer, so the
/// bare name wins any tie by [`MatchScore::len`].
///
/// The qualified form is matched contiguously *and* no weaker than
/// [`MAX_QUALIFIED_TIER`].
fn score_table_prepared(
    schema: Option<&[char]>,
    name: &[char],
    needle: &[char],
    buf: &mut Vec<char>,
) -> Option<MatchScore> {
    let bare = score_prepared(name, needle, true);
    let qualified = match schema {
        Some(schema) => {
            qualify_into(buf, schema, name);
            score_prepared(buf, needle, false).filter(|hit| hit.tier <= MAX_QUALIFIED_TIER)
        }
        None => None,
    };
    best_of(bare, qualified)
}

/// Scores an already-lowercased column name, qualified by its owning table
/// rather than by its schema — `:col users.id` is the natural way to say "the
/// id on users". `buf` is scratch, reused across calls.
///
/// Unlike [`score_table_prepared`], the qualified form is only tried when the
/// needle actually contains a `.`. The asymmetry is deliberate: a schema is
/// part of a table's identity and narrowing to one is a stated goal, but in
/// column mode the table name is a *scope* and nothing else, so it has to be
/// asked for. Left implicit, `:col stripe` would dump every column of a table
/// called `stripe_events` — a column search with extra steps.
fn score_column_prepared(
    table: &[char],
    column: &[char],
    needle: &[char],
    dotted: bool,
    buf: &mut Vec<char>,
) -> Option<MatchScore> {
    let bare = score_prepared(column, needle, true);
    let qualified = if dotted {
        qualify_into(buf, table, column);
        score_prepared(buf, needle, false).filter(|hit| hit.tier <= MAX_QUALIFIED_TIER)
    } else {
        None
    };
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
    // Lowercased once, then reused for every candidate. Rebuilding it per
    // haystack (~10k times per keystroke in column mode on a 300-table
    // database) was the single biggest cost in the matcher.
    let needle = lowercased(&query.needle);
    let dotted = query.needle.contains('.');
    // Scratch for the qualified forms, likewise reused rather than reallocated
    // per candidate.
    let mut buf: Vec<char> = Vec::new();
    let mut hits: Vec<(MatchScore, TableHit)> = Vec::new();
    for (index, table) in tables.iter().enumerate() {
        let name = lowercased(&table.name);
        match query.mode {
            FilterMode::Tables => {
                let schema = table.schema.as_deref().map(lowercased);
                if let Some(score) =
                    score_table_prepared(schema.as_deref(), &name, &needle, &mut buf)
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
                    let column_name = lowercased(&column.name);
                    if let Some(score) =
                        score_column_prepared(&name, &column_name, &needle, dotted, &mut buf)
                    {
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

    /// The tier a bare (unqualified) name matches at, subsequences allowed.
    fn tier(haystack: &str, needle: &str) -> Option<Tier> {
        score_prepared(&lowercased(haystack), &lowercased(needle), true).map(|s| s.tier)
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
            table(None, "records", &["id"]),
            table(None, "overdraft", &["id"]),
            table(None, "orders", &["id"]),
        ];
        // Prefix, then the mid-word substring (rec-ORD-s), then the scattered
        // subsequence last (O-ve-R-D-raft).
        assert_eq!(ranked(&tables, "ord"), ["orders", "records", "overdraft"]);
    }

    #[test]
    fn short_needles_never_fall_back_to_a_subsequence() {
        // Two characters is all noise: on a real schema it roughly quadruples
        // the result set with names nobody was reaching for. Three is where
        // the fallback starts earning its place, and `usrol` — the case the
        // feature exists for — is five.
        assert_eq!(tier("unit_specs", "us"), None);
        assert_eq!(tier("unit_specs", "uns"), Some(Tier::Subsequence));
        // Contiguous matches are unaffected at any length.
        assert_eq!(tier("census", "us"), Some(Tier::Substring));
    }

    #[test]
    fn the_tightest_subsequence_wins_regardless_of_where_it_starts() {
        // Within the subsequence tier the span *is* the quality of the match,
        // so it has to outrank the start offset — otherwise a loose scatter
        // that happens to begin early beats a tight run that begins late.
        let tables = [
            table(None, "alpha_beta_charlie", &["id"]),
            table(None, "zz_a_b_c", &["id"]),
        ];
        assert_eq!(ranked(&tables, "abc"), ["zz_a_b_c", "alpha_beta_charlie"]);
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
    fn a_schema_never_lends_its_letters_to_a_substring_match_either() {
        // The other half of the same hole, found in review: closing the fuzzy
        // route left the Substring tier live on the qualified name, so every
        // fragment of "public" still handed back the whole schema — and at a
        // tier *above* subsequence, so it wasn't even buried.
        let tables = [
            table(Some("public"), "tags", &["id"]),
            table(Some("public"), "orders", &["id"]),
            table(Some("public"), "line_items", &["id"]),
        ];
        for needle in ["ub", "bl", "ubli", "blic"] {
            assert!(
                ranked(&tables, needle).is_empty(),
                "{needle:?} matched {:?}",
                ranked(&tables, needle)
            );
        }
        // A needle that really is in a table name still finds it.
        assert_eq!(ranked(&tables, "li"), ["line_items"]);
    }

    #[test]
    fn a_schemas_second_word_still_narrows() {
        // Why the qualified cap is WordPrefix and not Prefix: "payroll" names
        // the scope just as plainly as "hr_payroll" does.
        let tables = [
            table(Some("hr_payroll"), "runs", &["id"]),
            table(Some("public"), "widgets", &["id"]),
        ];
        assert_eq!(ranked(&tables, "payroll"), ["runs"]);
        assert_eq!(ranked(&tables, "hr_payroll"), ["runs"]);
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

    #[test]
    fn a_table_never_lends_its_letters_to_a_column_scope() {
        // The same hole one level down: the table in front of a column is a
        // scope too, so a mid-word hit inside it must not scope the search.
        let tables = [
            table(None, "invoices", &["id", "total"]),
            table(None, "customers", &["id", "email"]),
        ];
        let scoped = |raw: &str| -> Vec<String> {
            filter_tables(&tables, &parse_query(raw))
                .iter()
                .map(|hit| tables[hit.index].name.clone())
                .collect()
        };
        // "voices" is a substring of "invoices" and names nothing.
        assert!(
            scoped(":col voices.id").is_empty(),
            "{:?}",
            scoped(":col voices.id")
        );
        assert!(
            scoped(":col stomers.em").is_empty(),
            "{:?}",
            scoped(":col stomers.em")
        );
        // Naming the table, or one of its words, still scopes.
        assert_eq!(scoped(":col invoices.id"), ["invoices"]);
        assert_eq!(scoped(":col customers.em"), ["customers"]);
    }

    #[test]
    fn only_the_subsequence_tier_has_a_span_of_its_own() {
        // `MatchScore` orders `span` before `start`, which is only correct
        // because every contiguous tier sets `span` to the needle length —
        // making it constant within a tier, so it can't outrank `start`
        // there. A future tier that computed its own span would silently
        // change the ranking of every tier above it, and nothing else in the
        // suite would notice.
        let names = [
            "users",
            "user_roles",
            "public.users",
            "AuditLog",
            "unit_specs",
            "\u{e5}rsrapport",
            "a.b.c",
            "x",
            "enrolments",
            "zz_a_b_c",
        ];
        let needles = [
            "u",
            "us",
            "user",
            "abc",
            "rol",
            "\u{e5}",
            "b",
            "x",
            "unit_specs",
            "zzz",
        ];
        for name in names {
            for needle in needles {
                let ned = lowercased(needle);
                for subsequences in [true, false] {
                    let Some(hit) = score_prepared(&lowercased(name), &ned, subsequences) else {
                        continue;
                    };
                    if hit.tier == Tier::Subsequence {
                        continue;
                    }
                    assert_eq!(
                        hit.span,
                        ned.len(),
                        "{name:?} / {needle:?} matched at {:?} with span {} != needle length {}",
                        hit.tier,
                        hit.span,
                        ned.len()
                    );
                }
            }
        }
    }
}
