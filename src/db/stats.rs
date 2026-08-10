//! What a table costs: how many rows it holds and how much disk it occupies
//! (FRE-118).
//!
//! "How big is this table?" is one of the first questions asked of an
//! unfamiliar database, and the answer is only cheap if it comes from
//! statistics the server already keeps. So this is deliberately two different
//! things wearing one type:
//!
//! - a **cheap estimate**, read from the planner's own statistics in a single
//!   catalog query ([`super::DbPool::fetch_table_stats`]), loaded lazily when
//!   the schema pane opens a table;
//! - an **exact count**, which is a `COUNT(*)` scan and therefore only ever
//!   runs because the user asked for one
//!   ([`super::DbPool::count_table_rows`]).
//!
//! [`RowCount`] keeps the two apart in the type rather than in a boolean
//! beside it, so nothing can render an estimate without saying that it is one.
//! A number presented as exact when it isn't is worse than no number.
//!
//! **Absence is a first-class answer.** Every field is optional and defaults to
//! `None`: a server that keeps no statistics this can read (CockroachDB,
//! Materialize and RisingWave each keep none), a table nothing has analyzed
//! yet, a view with no storage of its own. `None` must render as *nothing*, and
//! never as a zero — "0 rows" is a claim, and it would be the wrong one.

/// A table's storage statistics, as far as the server will say (FRE-118).
///
/// The two fields are independent, and every combination really occurs: SQLite
/// answers rows and not size, a never-analyzed Postgres table answers size and
/// not rows, and three of the Postgres-wire engines answer neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TableStats {
    /// How many rows the table holds — see [`RowCount`]. `None` when the
    /// server keeps no estimate for it.
    pub rows: Option<RowCount>,
    /// Bytes the object occupies on disk, including its indexes and any
    /// out-of-line storage. `None` when the server cannot say.
    ///
    /// Not an estimate in [`RowCount`]'s sense: every backend that answers this
    /// answers from its own allocation accounting (Postgres sums the relation's
    /// forks, SQL Server its reserved pages), which is what the object really
    /// occupies rather than a sampled guess. It can still exceed the live data
    /// — dead tuples and free pages are occupied space too.
    pub bytes: Option<u64>,
}

impl TableStats {
    /// Whether the server answered nothing at all, so the UI can say so once
    /// rather than render an empty line.
    pub fn is_empty(&self) -> bool {
        self.rows.is_none() && self.bytes.is_none()
    }
}

/// Interprets a raw size in bytes from a backend's catalog, dropping the
/// answers that are not measurements.
///
/// **Zero is dropped**, which is the whole content of this function. Nothing
/// occupies literally nothing, so a zero is a server saying it has not
/// accounted for the object yet: YugabyteDB answers `pg_total_relation_size`
/// with 0 for a table it was handed a hundred rows a moment ago
/// (`tests/db_yugabyte.rs`), and SQL Server reserves no pages for a table
/// nothing has written. Rendering "0 B" beside a populated table would be
/// precisely the wrong-number-presented-as-fact this feature is arranged to
/// avoid, and what is given up when the object really is empty is nil — an
/// empty table's size is not what anyone opened the pane for.
///
/// A negative goes the same way rather than wrapping into a nonsense `u64`.
pub(crate) fn size_bytes(raw: Option<i64>) -> Option<u64> {
    raw.filter(|bytes| *bytes > 0).map(|bytes| bytes as u64)
}

/// How many rows a table holds, and how much that number is worth.
///
/// The distinction is the whole point of the type: an estimate is free and
/// stale, an exact count is expensive and true, and the two must never render
/// the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowCount {
    /// The server's own estimate, kept for the query planner and refreshed by
    /// `ANALYZE`/autovacuum or maintained incrementally. Costs one catalog read
    /// and can be arbitrarily stale — a table loaded a second ago may still
    /// estimate zero.
    Estimated(u64),
    /// A `COUNT(*)` that actually ran, because the user asked for one.
    Exact(u64),
}

impl RowCount {
    pub fn value(self) -> u64 {
        match self {
            RowCount::Estimated(n) | RowCount::Exact(n) => n,
        }
    }

    pub fn is_estimate(self) -> bool {
        matches!(self, RowCount::Estimated(_))
    }

    /// The badge text shown beside the number. Both cases are labelled: the
    /// estimate because it must be, and the exact count because an unbadged
    /// number sitting where a badged one used to be reads as unlabelled rather
    /// than as exact.
    pub fn label(self) -> &'static str {
        match self {
            RowCount::Estimated(_) => "estimate",
            RowCount::Exact(_) => "exact",
        }
    }

    /// The tooltip explaining what the number is worth.
    pub fn tooltip(self) -> &'static str {
        match self {
            RowCount::Estimated(_) => {
                "The server's own row estimate, kept for the query planner. \
                 Cheap to read and possibly stale — use Count exactly for the real number."
            }
            RowCount::Exact(_) => "Counted with SELECT COUNT(*) just now.",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_estimate_never_passes_for_an_exact_count() {
        assert!(RowCount::Estimated(10).is_estimate());
        assert_eq!(RowCount::Estimated(10).label(), "estimate");
        assert!(!RowCount::Exact(10).is_estimate());
        assert_eq!(RowCount::Exact(10).label(), "exact");
        // The same number, and still not the same value: nothing that renders
        // one can accidentally render the other.
        assert_ne!(RowCount::Estimated(10), RowCount::Exact(10));
        assert_eq!(RowCount::Estimated(10).value(), RowCount::Exact(10).value());
        assert_ne!(
            RowCount::Estimated(10).tooltip(),
            RowCount::Exact(10).tooltip()
        );
    }

    #[test]
    fn nothing_known_is_distinguishable_from_a_counted_zero() {
        assert!(TableStats::default().is_empty());
        // A zero the user counted is a fact, not an absence — the "no
        // statistics" line must not swallow it.
        assert!(!TableStats {
            rows: Some(RowCount::Exact(0)),
            ..Default::default()
        }
        .is_empty());
        assert!(!TableStats {
            bytes: Some(4096),
            ..Default::default()
        }
        .is_empty());
    }

    #[test]
    fn a_zero_size_is_an_unaccounted_object_rather_than_an_empty_one() {
        assert_eq!(size_bytes(Some(8192)), Some(8192));
        assert_eq!(size_bytes(Some(0)), None);
        assert_eq!(size_bytes(Some(-1)), None);
        assert_eq!(size_bytes(None), None);
    }
}
