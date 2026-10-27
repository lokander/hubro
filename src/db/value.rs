use crate::util::human_bytes;

/// A single cell value, independent of the backend that produced it.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Short human-readable form for grid cells. Blobs render as a size tag
    /// rather than raw bytes; NULL is spelled out so it can be styled apart
    /// from the empty string.
    pub fn display(&self) -> String {
        match self {
            Value::Null => "NULL".to_string(),
            Value::Integer(i) => i.to_string(),
            Value::Real(r) => r.to_string(),
            Value::Text(t) => t.clone(),
            Value::Blob(b) => format!("<blob {}>", human_bytes(b.len() as u64)),
        }
    }
}

/// Truncates a decoded cell to at most `cap` bytes so a streamed result
/// (the free-form query path, [`super::DbPool::query_capped`]) can bound the
/// memory each retained cell costs even for a `SELECT *` returning huge
/// values. Text is cut on a char boundary; blobs are cut to `cap` bytes;
/// everything else is returned untouched. This is a display-only truncation
/// on a read-only result — nothing staged or exported round-trips through it.
pub(crate) fn cap_value(value: Value, cap: usize) -> Value {
    match value {
        Value::Text(t) if t.len() > cap => {
            let mut end = cap;
            while end > 0 && !t.is_char_boundary(end) {
                end -= 1;
            }
            Value::Text(t[..end].to_string())
        }
        Value::Blob(b) if b.len() > cap => Value::Blob(b[..cap].to_vec()),
        other => other,
    }
}

/// One decoded cell as text, `None` for SQL NULL (and for a column index the
/// row doesn't have).
///
/// These four accessors read a decoded row positionally, which is how every
/// backend consumes the catalog queries it runs through its own query path:
/// the SQL fixes the column order, so an index is the natural key and a
/// missing/NULL cell must degrade rather than error — a catalog column that
/// unexpectedly reads NULL should cost one caveat, not the whole DDL. They
/// live here because they operate on [`Value`], not on any driver's row type.
pub(crate) fn row_opt_text(row: &[Value], idx: usize) -> Option<String> {
    match row.get(idx) {
        Some(Value::Null) | None => None,
        Some(Value::Text(t)) => Some(t.clone()),
        Some(other) => Some(other.display()),
    }
}

/// [`row_opt_text`] with NULL flattened to the empty string.
pub(crate) fn row_text(row: &[Value], idx: usize) -> String {
    row_opt_text(row, idx).unwrap_or_default()
}

/// One decoded cell as an integer, `None` when it is NULL or not an integer.
pub(crate) fn row_opt_int(row: &[Value], idx: usize) -> Option<i64> {
    match row.get(idx) {
        Some(Value::Integer(n)) => Some(*n),
        _ => None,
    }
}

/// [`row_opt_int`] with NULL flattened to zero.
pub(crate) fn row_int(row: &[Value], idx: usize) -> i64 {
    row_opt_int(row, idx).unwrap_or(0)
}

/// One decoded cell as a boolean. Boolean catalog columns (SQL Server's `bit`)
/// decode as Integer 0/1.
pub(crate) fn row_flag(row: &[Value], idx: usize) -> bool {
    row_int(row, idx) != 0
}

/// The [`Value::Real`] a 4-byte float (`real`, `float4`, `float(24)`) cell
/// reads as: the `f64` nearest the shortest decimal that is that `f32`, so
/// `0.1f32` shows as `0.1` — what the server prints — rather than as the
/// `0.10000000149011612` that widening it with `as f64` printed in the grid,
/// the export and the clipboard.
///
/// The value bound back is that decimal too, which is why each backend that
/// produces one compares it *as* a `real`: a `real` key column carries a cast
/// to `real` on PostgreSQL and SQL Server alike (`staged::cast_targets`), and
/// CockroachDB, whose `real` is a 64-bit float, holds the decimal the row was
/// written with — `0.1`, not the widened `f32`. Rust's `f32` `Display` is the
/// shortest spelling that reads back as the same `f32`, and its NaN and
/// infinities parse back as themselves.
pub(crate) fn real_from_f32(narrow: f32) -> f64 {
    narrow.to_string().parse().unwrap_or(f64::from(narrow))
}

/// Trims trailing zeros from a chrono-formatted fractional second: `%.f`
/// pads to 3/6/9 digits ("09.500"), both Postgres and the SQL Server display
/// print minimal ("09.5"). The input must end with the seconds field; the
/// fraction dot is the only dot.
pub(crate) fn trim_fraction(mut s: String) -> String {
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s
}

/// A result column as reported by the driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
}

/// Rows and columns from an arbitrary query, ready for the data grid.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct QueryResult {
    pub columns: Vec<ColumnInfo>,
    pub rows: Vec<Vec<Value>>,
}

/// What became of the rest of a read the row cap cut short — what the editor
/// says beside its "first N rows" notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RestOfRead {
    /// Run to its end (or the read fitted under the cap).
    #[default]
    Ran,
    /// Stopped where nothing it did could have lasted anyway: a marked
    /// connection's read (the engine refuses its writes, and its transaction
    /// is rolled back regardless), or a read ended with its connection inside
    /// a transaction the run rolls back, or one discarded with its connection.
    Stopped,
    /// Cancelled on a writable connection because its transaction had
    /// written nothing to a table — which undoes everything else the
    /// statement did, rows before the cap included.
    CancelledUnwritten,
    /// Still running when the run ended: read on in the background, and its
    /// connection closed after — the last read of a run on an engine that
    /// cannot say whether the statement has written
    /// (`postgres::knows_whether_written`). The run reports before it ends,
    /// and an error past the cap undoes everything the statement did, rows
    /// before the cap included; how it ended follows later, as a
    /// [`LateRest`] on a [`PendingRest`]. So this claims nothing about its
    /// writes: measured on YugabyteDB, a `SELECT f(g)` whose function inserts
    /// a row per call and fails at call 15,000 was reported done, with the
    /// error reaching only the background read and none of its rows kept.
    Unconfirmed,
}

impl RestOfRead {
    /// Whether the server stopped the statement at the cap rather than
    /// running it to its end. An [`Unconfirmed`](RestOfRead::Unconfirmed)
    /// rest was not stopped: it runs on.
    pub fn stopped(self) -> bool {
        matches!(self, RestOfRead::Stopped | RestOfRead::CancelledUnwritten)
    }
}

/// How the rest of a read reported [`RestOfRead::Unconfirmed`] ended, once
/// the background read of it has finished — the outcome the run could not
/// wait for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LateRest {
    /// Read to its end: the statement finished, and what it wrote stands.
    Finished,
    /// The server ended the statement with this error past the cap. The
    /// statement was undone with it, its writes included.
    Failed(String),
    /// No answer came: the connection was lost while the rest was read, or
    /// the read was dropped before it reported. Whether the statement
    /// finished is not known.
    Unknown,
}

/// The outcome of a rest read after the run ([`RestOfRead::Unconfirmed`]),
/// to be awaited by whoever shows the run. Never makes the run itself wait.
#[derive(Debug)]
pub struct PendingRest(pub(crate) tokio::sync::oneshot::Receiver<LateRest>);

impl PendingRest {
    /// Waits for the background read to end. A read dropped without
    /// reporting is [`LateRest::Unknown`], so this never waits forever on a
    /// task that is gone.
    pub async fn outcome(self) -> LateRest {
        self.0.await.unwrap_or(LateRest::Unknown)
    }
}

/// A bounded read: the rows kept, whether rows existed past the cap, and
/// what became of the rest.
#[derive(Debug)]
pub(crate) struct Capped {
    /// Result sets the statement returned before `result`, each whole: only
    /// the last one kept can be cut short. Empty on every engine but SQL
    /// Server, where one statement can return several — `SELECT 1 AS a
    /// SELECT 2 AS b` (`sqlserver::pool::read_sets`).
    pub(crate) earlier: Vec<QueryResult>,
    /// The last result set kept, the one `truncated` and `rest` are about.
    pub(crate) result: QueryResult,
    pub(crate) truncated: bool,
    pub(crate) rest: RestOfRead,
}

/// A capped read from a path with no rest of its own to report: SQLite's and
/// SQL Server's reads that are not a script's last statement. `Ran` holds for
/// both, measured through `run_script`:
///
/// - **SQLite** never reads the rest (dropping the statement stops it), but
///   nothing past the cap has anything to do: a read cannot write, and a
///   write that returns rows makes every change in its first step (SQLite's
///   documented `RETURNING` behaviour). A `WITH … INSERT … RETURNING` of
///   20,000 rows cut at 10,000 kept all 20,000.
/// - **SQL Server**'s held connection keeps the response, and its rest is
///   read to its end before the read returns (`sqlserver::pool::read_rest`):
///   an `INSERT` in the rest of a capped batch ran (counted by the next
///   statement). An error in the rest fails the read, so a rest reported is
///   one that ran without one. It used to be left for tiberius to read before
///   the connection's next request, which discards what it reads: a `SELECT
///   1/0` in the rest of a capped batch left the script `Ok`.
///
/// A last read on either engine reports its own rest
/// (`sqlserver::finish_script_conn_with_read`, `ScriptConn::finish_with_read`).
impl From<(QueryResult, bool)> for Capped {
    fn from((result, truncated): (QueryResult, bool)) -> Self {
        Capped {
            earlier: Vec::new(),
            result,
            truncated,
            rest: RestOfRead::Ran,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_displays_as_marker() {
        assert_eq!(Value::Null.display(), "NULL");
        assert!(Value::Null.is_null());
    }

    #[test]
    fn scalars_display_plainly() {
        assert_eq!(Value::Integer(-42).display(), "-42");
        assert_eq!(Value::Real(1.5).display(), "1.5");
        assert_eq!(Value::Text("hi".into()).display(), "hi");
    }

    #[test]
    fn blobs_display_as_size_tag() {
        assert_eq!(Value::Blob(vec![0; 2048]).display(), "<blob 2.0 KB>");
        assert!(!Value::Blob(vec![]).is_null());
    }

    #[test]
    fn cap_value_truncates_text_and_blobs_but_not_scalars() {
        // Under the cap: untouched.
        assert_eq!(
            cap_value(Value::Text("hi".into()), 8),
            Value::Text("hi".into())
        );
        assert_eq!(
            cap_value(Value::Blob(vec![1, 2]), 8),
            Value::Blob(vec![1, 2])
        );
        // Over the cap: trimmed.
        assert_eq!(
            cap_value(Value::Text("abcdef".into()), 3),
            Value::Text("abc".into())
        );
        assert_eq!(
            cap_value(Value::Blob(vec![9; 100]), 4),
            Value::Blob(vec![9; 4])
        );
        // Scalars are never touched.
        assert_eq!(cap_value(Value::Integer(123456), 2), Value::Integer(123456));
        assert_eq!(cap_value(Value::Null, 0), Value::Null);
    }

    #[test]
    fn cap_value_cuts_text_on_a_char_boundary() {
        // "héllo": cutting at byte 2 would split 'é' (2 bytes); back off to 1.
        let capped = cap_value(Value::Text("héllo".into()), 2);
        assert_eq!(capped, Value::Text("h".into()));
    }

    #[test]
    fn row_accessors_degrade_on_null_and_missing_cells() {
        let row = [
            Value::Text("hi".into()),
            Value::Null,
            Value::Integer(0),
            Value::Integer(7),
        ];
        assert_eq!(row_opt_text(&row, 0), Some("hi".to_string()));
        assert_eq!(row_text(&row, 0), "hi");
        // Non-text cells render through `display`, so a numeric catalog column
        // read as text still says something useful.
        assert_eq!(row_text(&row, 3), "7");
        // NULL and past-the-end are the same "nothing there" for every arm.
        for idx in [1, 99] {
            assert_eq!(row_opt_text(&row, idx), None);
            assert_eq!(row_text(&row, idx), "");
            assert_eq!(row_opt_int(&row, idx), None);
            assert_eq!(row_int(&row, idx), 0);
            assert!(!row_flag(&row, idx));
        }
        assert_eq!(row_opt_int(&row, 2), Some(0));
        assert!(!row_flag(&row, 2));
        assert!(row_flag(&row, 3));
    }

    #[test]
    fn a_four_byte_float_reads_as_its_shortest_decimal() {
        assert_eq!(real_from_f32(0.1), 0.1);
        assert_eq!(Value::Real(real_from_f32(0.1)).display(), "0.1");
        assert_eq!(real_from_f32(1e20), 1e20);
        assert_eq!(real_from_f32(3.402_823_5e38), 3.402_823_5e38);
        assert_eq!(real_from_f32(-2.5), -2.5);
        // The decimal still reads back as the very same f32.
        for narrow in [
            0.1f32,
            1e20,
            f32::MAX,
            f32::MIN_POSITIVE,
            1e-45,
            16_777_217.0,
        ] {
            assert_eq!(real_from_f32(narrow) as f32, narrow, "{narrow}");
        }
        assert!(real_from_f32(f32::NAN).is_nan());
        assert_eq!(real_from_f32(f32::INFINITY), f64::INFINITY);
        assert_eq!(real_from_f32(f32::NEG_INFINITY), f64::NEG_INFINITY);
    }

    #[test]
    fn trim_fraction_strips_padding_zeros() {
        assert_eq!(trim_fraction("12:34:56.500".into()), "12:34:56.5");
        assert_eq!(trim_fraction("12:34:56.000".into()), "12:34:56");
        assert_eq!(trim_fraction("12:34:56".into()), "12:34:56");
    }
}
