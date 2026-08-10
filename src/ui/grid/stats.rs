//! Aggregate statistics for a cell selection (FRE-117): count, non-null count
//! and distinct count for anything, plus sum/avg/min/max over the numeric
//! cells.
//!
//! Two properties are the whole point of the feature, and both are enforced
//! here rather than promised in prose:
//!
//! 1. **It describes the selection, not the table.** Every readout
//!    [`SelectionStats::line`] produces opens with `Selection` and its cell
//!    count, and [`SelectionStats::scope_note`] spells out that the numbers
//!    cover the selected cells on the current page only. A sum over one page
//!    presented as *the* sum is a wrong answer, not a rough one.
//! 2. **No query, no full-table scan.** [`SelectionStats::compute`] takes the
//!    page already in memory ([`GridNav`]) and a [`Selection`], and visits
//!    only the cells inside the rectangle. It is a pure function with no
//!    access to a pool, a resource, or an await — pinned by
//!    `the_readout_cannot_reach_the_database` below, which reads this file's
//!    own source.
//!
//! Numeric-ness is decided by the *decoded value*, never by the column type
//! and never by parsing text: a cell counts as numeric exactly when it is
//! [`Value::Integer`] or [`Value::Real`]. `Value` has no numeric variant to
//! ask about, so this is the one deliberate definition — text that looks like
//! a number ("42", a numeric-typed column a backend hands back as text) is
//! text, because parsing it would make the readout depend on locale and
//! formatting rather than on what the database returned.

use std::collections::BTreeSet;

use super::*;

/// Sum of the integer cells, kept exact in `i128` so a page full of `i64`
/// values cannot wrap. `Overflow` is the refusal: a sum that no longer fits is
/// reported as such rather than silently wrapped or saturated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IntSum {
    Exact(i128),
    Overflow,
}

impl IntSum {
    fn add(self, value: i64) -> Self {
        match self {
            IntSum::Exact(total) => match total.checked_add(value as i128) {
                Some(next) => IntSum::Exact(next),
                None => IntSum::Overflow,
            },
            IntSum::Overflow => IntSum::Overflow,
        }
    }
}

/// Neumaier compensated summation for the floating-point cells: the running
/// compensation recovers the low-order bits a plain `+=` drops when values of
/// very different magnitudes meet, which is exactly the shape of a real
/// column (a few large rows among many small ones).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct FloatSum {
    total: f64,
    compensation: f64,
}

impl FloatSum {
    fn add(self, value: f64) -> Self {
        let total = self.total + value;
        let recovered = if self.total.abs() >= value.abs() {
            (self.total - total) + value
        } else {
            (value - total) + self.total
        };
        FloatSum {
            total,
            // An infinite term makes the recovered low bits `inf - inf` = NaN,
            // which would poison every later addition. There are no low bits
            // to recover around an infinity, so drop it: the running total
            // already says what an infinity in the column means.
            compensation: if recovered.is_finite() {
                self.compensation + recovered
            } else {
                self.compensation
            },
        }
    }

    fn value(self) -> f64 {
        self.total + self.compensation
    }
}

/// One numeric cell, keeping the integer/real distinction so an `i64` past
/// 2^53 still prints exactly.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Num {
    Int(i64),
    Real(f64),
}

impl Num {
    fn as_f64(self) -> f64 {
        match self {
            Num::Int(i) => i as f64,
            Num::Real(r) => r,
        }
    }

    /// Ordering for min/max. Two integers compare exactly; anything else goes
    /// through `f64`. NaN never reaches here — [`SelectionStats::compute`]
    /// keeps it out of min/max entirely.
    fn min_of(self, other: Self) -> Self {
        if self.is_below(other) {
            self
        } else {
            other
        }
    }

    fn max_of(self, other: Self) -> Self {
        if self.is_below(other) {
            other
        } else {
            self
        }
    }

    fn is_below(self, other: Self) -> bool {
        match (self, other) {
            (Num::Int(a), Num::Int(b)) => a < b,
            _ => self.as_f64() < other.as_f64(),
        }
    }

    fn display(self) -> String {
        match self {
            Num::Int(i) => i.to_string(),
            Num::Real(r) => format_float(r),
        }
    }
}

/// The sum of a selection's numeric cells: exact while every one of them is an
/// integer, floating point once a real joins them, and `TooLarge` when the
/// integer accumulator overflowed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Sum {
    Int(i128),
    Real(f64),
    TooLarge,
}

impl Sum {
    fn display(self) -> String {
        match self {
            Sum::Int(i) => i.to_string(),
            Sum::Real(r) => format_float(r),
            Sum::TooLarge => "too large to total".to_string(),
        }
    }

    /// The sum as an `f64` for the average, `None` when it overflowed. An
    /// exact `i128` sum bigger than 2^53 loses precision here; an average is
    /// approximate by nature, and the sum beside it is still exact.
    fn as_f64(self) -> Option<f64> {
        match self {
            Sum::Int(i) => Some(i as f64),
            Sum::Real(r) => Some(r),
            Sum::TooLarge => None,
        }
    }
}

/// A value reduced to what "distinct" means here. Integers and integral reals
/// share the `Num` arm, so `1` and `1.0` are one value — they are one value to
/// the database too. Text is never unified with a number: `"1"` is a third
/// value, matching the numeric-ness rule above.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum DistinctKey {
    Num(i128),
    /// A real that is not an integer (or is too large to be one), keyed on its
    /// bits with `-0.0` folded onto `0.0` and every NaN onto one bit pattern.
    Bits(u64),
    Text(String),
    Blob(Vec<u8>),
}

/// Reals beyond this magnitude are kept as bits rather than folded into
/// [`DistinctKey::Num`]: `as i128` saturates above `i128::MAX`, which would
/// make two different huge values compare equal.
const INT_KEY_LIMIT: f64 = 1e30;

impl DistinctKey {
    fn of(value: &Value) -> Option<Self> {
        Some(match value {
            Value::Null => return None,
            Value::Integer(i) => DistinctKey::Num(*i as i128),
            Value::Real(r) => {
                if r.is_finite() && r.fract() == 0.0 && r.abs() < INT_KEY_LIMIT {
                    DistinctKey::Num(*r as i128)
                } else if r.is_nan() {
                    DistinctKey::Bits(f64::NAN.to_bits())
                } else if *r == 0.0 {
                    DistinctKey::Bits(0f64.to_bits())
                } else {
                    DistinctKey::Bits(r.to_bits())
                }
            }
            Value::Text(t) => DistinctKey::Text(t.clone()),
            Value::Blob(b) => DistinctKey::Blob(b.clone()),
        })
    }
}

/// What a selection's cells add up to. Every field counts cells that were
/// actually in hand: a selection racing a shrinking page is clipped to the
/// rows the page holds, exactly as a copy is.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct SelectionStats {
    /// Shape of the part of the selection that landed on the page.
    pub(super) rows: usize,
    pub(super) cols: usize,
    /// Cells examined — never the page's or the table's row count.
    pub(super) cells: usize,
    pub(super) non_null: usize,
    /// Non-null cells that are [`Value::Integer`] or [`Value::Real`].
    pub(super) numeric: usize,
    /// Numeric cells that are NaN. They are counted as numeric but left out of
    /// sum/avg/min/max: letting one NaN poison the sum would erase what the
    /// other cells say, and NaN has no ordering to be a min or a max.
    pub(super) nan: usize,
    /// Distinct non-null values.
    pub(super) distinct: usize,
    /// Whether `distinct` is only a lower bound because some selected cell is
    /// a truncated preview (FRE-33): two values sharing a prefix collapse into
    /// one key, so the true count can only be higher.
    pub(super) distinct_is_lower_bound: bool,
    /// `None` when no numeric cell was summable (none at all, or all NaN).
    pub(super) sum: Option<Sum>,
    pub(super) min: Option<Num>,
    pub(super) max: Option<Num>,
}

impl SelectionStats {
    /// Reduces `selection` over the page in `nav` to its statistics, or `None`
    /// when the rectangle covers no cell the page actually holds.
    ///
    /// Visits the selected rectangle and nothing else — the cost is the size
    /// of the selection, not of the page and certainly not of the table.
    pub(super) fn compute(nav: &GridNav, selection: Selection) -> Option<Self> {
        let rect = selection.bounds();
        let last_row = nav.rows.len().checked_sub(1)?;
        if rect.top > last_row {
            return None;
        }
        let mut cells = 0usize;
        let mut non_null = 0usize;
        let mut numeric = 0usize;
        let mut nan = 0usize;
        let mut truncated = false;
        let mut distinct: BTreeSet<DistinctKey> = BTreeSet::new();
        let mut int_sum = IntSum::Exact(0);
        let mut float_sum = FloatSum::default();
        let mut any_real = false;
        let mut min: Option<Num> = None;
        let mut max: Option<Num> = None;
        let mut widest_row = 0usize;

        for row in nav.rows.iter().take(rect.bottom + 1).skip(rect.top) {
            let mut in_row = 0usize;
            for col in rect.left..=rect.right {
                let Some(cell) = row.cells.get(col) else {
                    continue;
                };
                in_row += 1;
                cells += 1;
                if cell.value.is_null() {
                    continue;
                }
                non_null += 1;
                truncated |= cell.truncated();
                if let Some(key) = DistinctKey::of(&cell.value) {
                    distinct.insert(key);
                }
                let num = match &cell.value {
                    Value::Integer(i) => Num::Int(*i),
                    Value::Real(r) => Num::Real(*r),
                    _ => continue,
                };
                numeric += 1;
                if num.as_f64().is_nan() {
                    nan += 1;
                    continue;
                }
                match num {
                    Num::Int(i) => int_sum = int_sum.add(i),
                    Num::Real(r) => {
                        any_real = true;
                        float_sum = float_sum.add(r);
                    }
                }
                min = Some(match min {
                    Some(current) => current.min_of(num),
                    None => num,
                });
                max = Some(match max {
                    Some(current) => current.max_of(num),
                    None => num,
                });
            }
            widest_row = widest_row.max(in_row);
        }
        if cells == 0 {
            return None;
        }
        let summable = numeric - nan;
        let sum = (summable > 0).then(|| match (int_sum, any_real) {
            (IntSum::Overflow, _) => Sum::TooLarge,
            (IntSum::Exact(total), false) => Sum::Int(total),
            (IntSum::Exact(total), true) => Sum::Real(total as f64 + float_sum.value()),
        });
        let rows = nav.rows.len().min(rect.bottom + 1) - rect.top;
        Some(SelectionStats {
            rows,
            cols: widest_row,
            cells,
            non_null,
            numeric,
            nan,
            distinct: distinct.len(),
            distinct_is_lower_bound: truncated,
            sum,
            min,
            max,
        })
    }

    /// The readout to show for `selection`, or `None` when there is nothing
    /// worth saying: a rectangle off the page, or a single cell. One cell's
    /// value is already on screen and "1 cell · 1 non-null · sum 5 · avg 5 ·
    /// min 5 · max 5" beside it is noise that teaches people to ignore the
    /// line — and the grid always has one cell focused, so that would be the
    /// resting state.
    pub(super) fn for_readout(nav: &GridNav, selection: Selection) -> Option<Self> {
        if selection.is_single() {
            return None;
        }
        Self::compute(nav, selection)
    }

    /// The mean of the summable numeric cells.
    pub(super) fn avg(&self) -> Option<f64> {
        let summable = self.numeric.checked_sub(self.nan)?;
        if summable == 0 {
            return None;
        }
        Some(self.sum?.as_f64()? / summable as f64)
    }

    /// The readout's opening: what this describes and how big it is. Rendered
    /// as its own span and never allowed to give way, because a row of numbers
    /// with nothing saying whose they are is the failure this feature exists
    /// to avoid.
    pub(super) fn scope_prefix(&self) -> String {
        format!("Selection {}×{}", self.rows, self.cols)
    }

    /// The counting half: cells, non-null, distinct. This is the part that
    /// truncates first in a narrow footer — it is the preamble, and the
    /// aggregates beside it are what the reader came for.
    pub(super) fn counts_text(&self) -> String {
        let mut parts = vec![
            format!("{} {}", self.cells, plural(self.cells, "cell", "cells")),
            format!("{} non-null", self.non_null),
        ];
        if self.non_null > 0 {
            let bound = if self.distinct_is_lower_bound {
                "≥"
            } else {
                ""
            };
            parts.push(format!("{bound}{} distinct", self.distinct));
        }
        parts.join(" · ")
    }

    /// The aggregate half: sum/avg/min/max, and the caveats that qualify them.
    /// `None` when there is no numeric cell to aggregate.
    ///
    /// The "N numeric of M non-null" qualifier lives *in here* rather than
    /// with the counts on purpose: it is the sentence that stops a sum over 7
    /// of 11 values reading as a sum over 11, so it has to survive exactly
    /// when the sum does. Splitting them would let a narrow window drop the
    /// qualifier and keep the number.
    pub(super) fn aggregate_text(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        if let Some(sum) = self.sum {
            let summable = self.numeric - self.nan;
            if summable != self.non_null {
                parts.push(format!("{summable} numeric of {} non-null", self.non_null));
            }
            parts.push(format!("sum {}", sum.display()));
            if let Some(avg) = self.avg() {
                parts.push(format!("avg {}", format_float(avg)));
            }
            if let (Some(min), Some(max)) = (self.min, self.max) {
                parts.push(format!("min {}", min.display()));
                parts.push(format!("max {}", max.display()));
            }
            if self.nan > 0 {
                parts.push(format!("{} NaN skipped", self.nan));
            }
        } else if self.numeric > 0 && self.numeric == self.nan {
            parts.push(format!("{} numeric, all NaN", self.numeric));
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }

    /// The whole readout as one line — the tooltip's first line and the basis
    /// of what a click copies. Composed from the three rendered pieces, so
    /// what is displayed and what is copied cannot drift apart.
    pub(super) fn line(&self) -> String {
        let mut parts = vec![self.scope_prefix(), self.counts_text()];
        parts.extend(self.aggregate_text());
        parts.join(" · ")
    }

    /// What a click puts on the clipboard: the readout *and* its scope, joined.
    ///
    /// The line alone says "Selection" but not which rows it covers, and a
    /// number pasted into a ticket or a message outlives every tooltip — that
    /// paste is exactly where mistaking a page's sum for the table's does its
    /// damage. So the caveat travels with the numbers.
    pub(super) fn copy_text(&self) -> String {
        format!("{}\n{}", self.line(), self.scope_note())
    }

    /// The tooltip: what the readout covers, in as many words. The readout is
    /// a description of the selection and nothing wider, and this is where
    /// that is said outright.
    pub(super) fn scope_note(&self) -> String {
        let mut note = format!(
            "Totals for the {} selected {} on this page only — not the whole table, and not the other pages.",
            self.cells,
            plural(self.cells, "cell", "cells"),
        );
        if self.distinct_is_lower_bound {
            note.push_str(" Some selected cells are truncated previews, so the distinct count is a lower bound.");
        }
        if self.nan > 0 {
            note.push_str(" NaN values are counted but left out of sum, average, min and max.");
        }
        note
    }
}

/// The CSS shrink factor for the counts span, and for the aggregate span
/// beside it (FRE-117). The footer renders both with `truncate`, and the pair
/// decides which one gives up its width first when the readout is wider than
/// the footer.
///
/// (Spelled "shrink factor" rather than with the CSS property's own name on
/// purpose: Tailwind scans these files for class candidates and emits a dead
/// utility for the bare property name, even from a comment.)
///
/// The priority is expressed by making the **counts** shed fast, not by making
/// the aggregates shed slowly, and the difference is not cosmetic. Flexbox
/// splits an overflow by `factor × width`, so either spelling sheds the counts
/// first — but a factor below 1 on the aggregates holds that span at its
/// content width, and a box that never shrinks never fires its own
/// `text-overflow: ellipsis`. The ancestor's `overflow-hidden` clips it
/// instead, and *that* cut has no ellipsis: at 1000px the readout ended
/// `sum 1.00000000000002`, a complete-looking number missing its `e16` and
/// wrong by sixteen orders of magnitude, with nothing on screen saying so.
/// Keeping the aggregates at the default 1 means the span itself shrinks and
/// its ellipsis fires, so a reader who sees a cut number can see that it is
/// cut. Pinned by `a_truncating_span_must_be_able_to_shrink`.
pub(super) const COUNTS_SHRINK: &str = "1000";
pub(super) const AGGREGATE_SHRINK: &str = "1";

fn plural(n: usize, one: &'static str, many: &'static str) -> &'static str {
    if n == 1 {
        one
    } else {
        many
    }
}

/// Renders an `f64` for the readout: integral values without a decimal point,
/// very large or very small ones in exponent form, everything else to six
/// decimals with the padding trimmed.
fn format_float(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "∞" } else { "-∞" }.to_string();
    }
    let abs = value.abs();
    if value == value.trunc() && abs < 1e15 {
        return format!("{}", value as i64);
    }
    if !(1e-4..1e15).contains(&abs) {
        return format!("{value:e}");
    }
    let mut text = format!("{value:.6}");
    while text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{ColumnInfo, PreviewInfo};

    /// A page of `rows` × `values[0].len()` cells from raw values, with no
    /// identity — the stats never need one.
    fn nav_of(values: Vec<Vec<Value>>) -> GridNav {
        nav_with_previews(values, &[])
    }

    fn nav_with_previews(
        values: Vec<Vec<Value>>,
        previews: &[Vec<Option<PreviewInfo>>],
    ) -> GridNav {
        let width = values.first().map(|r| r.len()).unwrap_or(0);
        let headers: Vec<String> = (0..width).map(|i| format!("c{i}")).collect();
        let result = QueryResult {
            columns: headers
                .iter()
                .map(|name| ColumnInfo { name: name.clone() })
                .collect(),
            rows: values,
        };
        let rows = view_rows(&result, previews, 0, None, None, true);
        GridNav::build(headers, &rows, &HashMap::new())
    }

    fn stats(values: Vec<Vec<Value>>, selection: Selection) -> SelectionStats {
        SelectionStats::compute(&nav_of(values), selection).expect("selection covers cells")
    }

    #[test]
    fn a_numeric_selection_reports_count_sum_average_and_bounds() {
        let s = stats(
            vec![
                vec![Value::Integer(1), Value::Integer(10)],
                vec![Value::Integer(2), Value::Integer(20)],
            ],
            Selection::all(2, 2).unwrap(),
        );
        assert_eq!((s.cells, s.non_null, s.numeric, s.distinct), (4, 4, 4, 4));
        assert_eq!(s.sum, Some(Sum::Int(33)));
        assert_eq!(s.avg(), Some(8.25));
        assert_eq!(s.min, Some(Num::Int(1)));
        assert_eq!(s.max, Some(Num::Int(20)));
        let line = s.line();
        assert!(line.contains("sum 33"), "{line}");
        assert!(line.contains("avg 8.25"), "{line}");
        assert!(line.contains("min 1"), "{line}");
        assert!(line.contains("max 20"), "{line}");
        // Every non-null cell is numeric, so there is no subset to qualify.
        assert!(!line.contains("numeric of"), "{line}");
    }

    #[test]
    fn nulls_are_counted_but_never_summed_or_averaged() {
        let s = stats(
            vec![
                vec![Value::Integer(4), Value::Null],
                vec![Value::Null, Value::Integer(6)],
            ],
            Selection::all(2, 2).unwrap(),
        );
        assert_eq!(s.cells, 4);
        assert_eq!(s.non_null, 2);
        assert_eq!(s.numeric, 2);
        assert_eq!(s.sum, Some(Sum::Int(10)));
        // The average divides by the numeric cells, not by the selection.
        assert_eq!(s.avg(), Some(5.0));
        assert_eq!(s.distinct, 2, "NULL is not a distinct value");

        // All NULL: counted, and nothing else claimed.
        let s = stats(
            vec![vec![Value::Null, Value::Null]],
            Selection::all(1, 2).unwrap(),
        );
        assert_eq!((s.cells, s.non_null, s.numeric), (2, 0, 0));
        assert_eq!(s.sum, None);
        assert_eq!(s.avg(), None);
        let line = s.line();
        assert!(line.contains("0 non-null"), "{line}");
        assert!(!line.contains("distinct"), "{line}");
        assert!(!line.contains("sum"), "{line}");
    }

    #[test]
    fn text_that_looks_numeric_is_text() {
        // The decoded value decides, not the characters in it: a text "10"
        // must never join a sum, or the readout would depend on formatting.
        let s = stats(
            vec![vec![Value::Text("10".into()), Value::Text("-3.5".into())]],
            Selection::all(1, 2).unwrap(),
        );
        assert_eq!(s.numeric, 0);
        assert_eq!(s.sum, None);
        assert_eq!(s.min, None);
        assert_eq!(s.distinct, 2);
        let line = s.line();
        assert!(line.contains("2 distinct"), "{line}");
        assert!(!line.contains("sum"), "{line}");
    }

    #[test]
    fn a_mixed_selection_sums_the_numeric_cells_and_says_how_many_they_were() {
        // Spanning a numeric and a text column: the sum covers 2 of 4 values,
        // and the readout has to say so — a sum labelled as covering all four
        // would be a wrong answer.
        let s = stats(
            vec![
                vec![Value::Integer(5), Value::Text("five".into())],
                vec![Value::Integer(7), Value::Text("seven".into())],
            ],
            Selection::all(2, 2).unwrap(),
        );
        assert_eq!((s.non_null, s.numeric), (4, 2));
        assert_eq!(s.sum, Some(Sum::Int(12)));
        assert_eq!(s.avg(), Some(6.0), "averaged over the numeric cells");
        assert_eq!(s.distinct, 4);
        let line = s.line();
        assert!(line.contains("2 numeric of 4 non-null"), "{line}");
        assert!(line.contains("sum 12"), "{line}");
    }

    #[test]
    fn integers_and_reals_in_one_selection_total_as_a_real() {
        let s = stats(
            vec![vec![Value::Integer(2), Value::Real(0.5)]],
            Selection::all(1, 2).unwrap(),
        );
        assert_eq!(s.sum, Some(Sum::Real(2.5)));
        assert_eq!(s.min, Some(Num::Real(0.5)));
        assert_eq!(s.max, Some(Num::Int(2)));
        assert!(s.line().contains("sum 2.5"), "{}", s.line());
    }

    #[test]
    fn an_integer_sum_stays_exact_past_what_f64_can_hold() {
        // Two i64::MAX would overflow an i64 sum and lose the low bits in an
        // f64 one; the i128 accumulator prints the exact total.
        let s = stats(
            vec![vec![Value::Integer(i64::MAX), Value::Integer(i64::MAX)]],
            Selection::all(1, 2).unwrap(),
        );
        assert_eq!(s.sum, Some(Sum::Int(i64::MAX as i128 * 2)));
        assert!(
            s.line().contains("sum 18446744073709551614"),
            "{}",
            s.line()
        );
    }

    #[test]
    fn an_overflowing_integer_sum_refuses_rather_than_wraps() {
        // The accumulator is i128 and a page holds at most PAGE_SIZE rows, so
        // real data cannot reach this — the arithmetic is checked at the
        // boundary anyway, and this drives it there directly.
        assert_eq!(IntSum::Exact(0).add(5), IntSum::Exact(5));
        assert_eq!(IntSum::Exact(i128::MAX).add(1), IntSum::Overflow);
        assert_eq!(IntSum::Exact(i128::MIN).add(-1), IntSum::Overflow);
        // Once overflowed it stays overflowed — no wrapping back into range.
        assert_eq!(IntSum::Overflow.add(-5), IntSum::Overflow);
        assert_eq!(Sum::TooLarge.as_f64(), None);
        assert_eq!(Sum::TooLarge.display(), "too large to total");

        // And the headroom the argument above rests on: a full page of
        // i64::MAX is nowhere near i128::MAX.
        let page_worst_case = i64::MAX as i128 * PAGE_SIZE as i128 * 10_000;
        assert!(page_worst_case < i128::MAX);
    }

    #[test]
    fn floating_point_addition_is_compensated() {
        // Naive accumulation loses every 1.0 next to 1e16 (one ulp there is
        // 2.0) and would report 0; Neumaier summation keeps them.
        let values = vec![vec![
            Value::Real(1e16),
            Value::Real(1.0),
            Value::Real(1.0),
            Value::Real(1.0),
            Value::Real(-1e16),
        ]];
        let naive: f64 = [1e16, 1.0, 1.0, 1.0, -1e16].iter().sum();
        assert_eq!(naive, 0.0, "the error this guards against");
        let s = stats(values, Selection::all(1, 5).unwrap());
        assert_eq!(s.sum, Some(Sum::Real(3.0)));
    }

    #[test]
    fn nan_is_counted_as_numeric_but_kept_out_of_the_aggregates() {
        let s = stats(
            vec![vec![
                Value::Real(f64::NAN),
                Value::Real(2.0),
                Value::Real(4.0),
            ]],
            Selection::all(1, 3).unwrap(),
        );
        assert_eq!((s.numeric, s.nan), (3, 1));
        assert_eq!(s.sum, Some(Sum::Real(6.0)), "the NaN did not poison it");
        assert_eq!(s.avg(), Some(3.0), "averaged over the two summable cells");
        assert_eq!(s.min, Some(Num::Real(2.0)));
        assert_eq!(s.max, Some(Num::Real(4.0)));
        let line = s.line();
        assert!(line.contains("1 NaN skipped"), "{line}");
        assert!(s.scope_note().contains("NaN"), "{}", s.scope_note());

        // Nothing but NaN: no sum to quote, and the readout says why.
        let s = stats(
            vec![vec![Value::Real(f64::NAN), Value::Real(f64::NAN)]],
            Selection::all(1, 2).unwrap(),
        );
        assert_eq!(s.sum, None);
        assert_eq!(s.avg(), None);
        assert_eq!(s.min, None);
        assert!(s.line().contains("2 numeric, all NaN"), "{}", s.line());
        // Every NaN is one distinct value, not two.
        assert_eq!(s.distinct, 1);
    }

    #[test]
    fn infinities_stay_in_the_aggregates() {
        let s = stats(
            vec![vec![Value::Real(f64::INFINITY), Value::Real(1.0)]],
            Selection::all(1, 2).unwrap(),
        );
        assert_eq!(s.max, Some(Num::Real(f64::INFINITY)));
        assert_eq!(s.min, Some(Num::Real(1.0)));
        assert!(s.line().contains("sum ∞"), "{}", s.line());
        // The compensation term must not turn `inf - inf` into a NaN that
        // poisons the rest of the sum: the finite cells after an infinity
        // still land in the total.
        let s = stats(
            vec![vec![
                Value::Real(f64::INFINITY),
                Value::Real(1.0),
                Value::Real(f64::NEG_INFINITY),
            ]],
            Selection::all(1, 3).unwrap(),
        );
        assert_eq!(
            s.sum.map(|sum| matches!(sum, Sum::Real(r) if r.is_nan())),
            Some(true),
            "+∞ and -∞ in one selection genuinely have no total"
        );
        let s = stats(
            vec![vec![
                Value::Real(f64::INFINITY),
                Value::Real(1.0),
                Value::Real(2.0),
            ]],
            Selection::all(1, 3).unwrap(),
        );
        assert_eq!(s.sum, Some(Sum::Real(f64::INFINITY)));
    }

    #[test]
    fn distinct_unifies_a_number_with_its_integral_real_but_not_with_its_text() {
        let s = stats(
            vec![vec![
                Value::Integer(1),
                Value::Real(1.0),
                Value::Text("1".into()),
                Value::Real(1.5),
                Value::Blob(vec![1]),
            ]],
            Selection::all(1, 5).unwrap(),
        );
        // 1 == 1.0; "1", 1.5 and the blob are three more values.
        assert_eq!(s.distinct, 4);
        // -0.0 and 0.0 are one value, and so is every NaN.
        let s = stats(
            vec![vec![Value::Real(-0.0), Value::Real(0.0), Value::Integer(0)]],
            Selection::all(1, 3).unwrap(),
        );
        assert_eq!(s.distinct, 1);
    }

    #[test]
    fn a_truncated_preview_makes_the_distinct_count_a_lower_bound() {
        // Two previews can share a prefix and differ past it, so the count can
        // only be too low — the readout marks it rather than overstating.
        let previews = vec![
            vec![
                None,
                Some(PreviewInfo {
                    full_len: 9_000,
                    binary: false,
                }),
            ],
            vec![
                None,
                Some(PreviewInfo {
                    full_len: 9_000,
                    binary: false,
                }),
            ],
        ];
        let nav = nav_with_previews(
            vec![
                vec![Value::Integer(1), Value::Text("same prefix".into())],
                vec![Value::Integer(2), Value::Text("same prefix".into())],
            ],
            &previews,
        );
        let s = SelectionStats::compute(&nav, Selection::all(2, 2).unwrap()).unwrap();
        assert!(s.distinct_is_lower_bound);
        assert!(s.line().contains("≥3 distinct"), "{}", s.line());
        assert!(s.scope_note().contains("lower bound"), "{}", s.scope_note());

        // Without previews the same values are counted exactly.
        let s = stats(
            vec![
                vec![Value::Integer(1), Value::Text("same prefix".into())],
                vec![Value::Integer(2), Value::Text("same prefix".into())],
            ],
            Selection::all(2, 2).unwrap(),
        );
        assert!(!s.distinct_is_lower_bound);
        assert!(s.line().contains("3 distinct"), "{}", s.line());
        assert!(!s.line().contains("≥"), "{}", s.line());
    }

    #[test]
    fn the_readout_counts_the_selection_and_never_the_page() {
        // A thousand-row page with a 2×2 selection: the numbers describe four
        // cells. If this ever reported the page, `cells` would be 2000 and the
        // sum would be enormous — which is the whole failure mode FRE-117
        // guards against.
        let values: Vec<Vec<Value>> = (0..1000)
            .map(|i| vec![Value::Integer(i), Value::Integer(1)])
            .collect();
        let nav = nav_of(values);
        let selection = Selection {
            anchor: (0, 0),
            focus: (1, 1),
        };
        let s = SelectionStats::compute(&nav, selection).unwrap();
        assert_eq!(s.cells, 4);
        assert_eq!((s.rows, s.cols), (2, 2));
        // The four selected cells hold 0, 1, 1, 1 — not the page's 1000 rows.
        assert_eq!(s.sum, Some(Sum::Int(3)));
        assert!(s.line().contains("4 cells"), "{}", s.line());
        assert!(!s.line().contains("1000"), "{}", s.line());
    }

    #[test]
    fn a_very_large_selection_is_counted_exactly() {
        // A full page, every cell selected: 100 × 20 = 2000 cells.
        let values: Vec<Vec<Value>> = (0..100)
            .map(|r| (0..20).map(|c| Value::Integer(r * 20 + c)).collect())
            .collect();
        let nav = nav_of(values);
        let s = SelectionStats::compute(&nav, Selection::all(100, 20).unwrap()).unwrap();
        assert_eq!(s.cells, 2000);
        assert_eq!(s.non_null, 2000);
        assert_eq!(s.distinct, 2000);
        // 0 + 1 + … + 1999.
        assert_eq!(s.sum, Some(Sum::Int(1999 * 2000 / 2)));
        assert_eq!(s.min, Some(Num::Int(0)));
        assert_eq!(s.max, Some(Num::Int(1999)));
    }

    #[test]
    fn a_selection_past_the_page_is_clipped_to_the_cells_in_hand() {
        // The clamp effect normally prevents this; if a selection outruns a
        // shrinking page anyway, the readout describes what was there.
        let nav = nav_of(vec![vec![Value::Integer(1), Value::Integer(2)]]);
        let selection = Selection {
            anchor: (0, 0),
            focus: (9, 9),
        };
        let s = SelectionStats::compute(&nav, selection).unwrap();
        assert_eq!(s.cells, 2);
        assert_eq!((s.rows, s.cols), (1, 2));
        assert_eq!(s.sum, Some(Sum::Int(3)));

        // Entirely past the page: nothing to describe.
        let past = Selection {
            anchor: (5, 0),
            focus: (9, 1),
        };
        assert_eq!(SelectionStats::compute(&nav, past), None);
        // …and an empty page has no readout at all.
        assert_eq!(
            SelectionStats::compute(&nav_of(vec![]), Selection::single((0, 0))),
            None
        );
    }

    #[test]
    fn a_single_cell_gets_no_readout() {
        // The grid always keeps one cell focused, so a single-cell selection
        // is the resting state: a readout there would be permanent noise. The
        // moment the selection grows, it appears.
        let nav = nav_of(vec![
            vec![Value::Integer(1), Value::Integer(2)],
            vec![Value::Integer(3), Value::Integer(4)],
        ]);
        assert_eq!(
            SelectionStats::for_readout(&nav, Selection::single((0, 0))),
            None
        );
        let pair = Selection {
            anchor: (0, 0),
            focus: (0, 1),
        };
        let s = SelectionStats::for_readout(&nav, pair).expect("two cells get a readout");
        assert_eq!(s.cells, 2);
        // `compute` itself stays general — the suppression is the readout's
        // decision, not the model's.
        assert!(SelectionStats::compute(&nav, Selection::single((0, 0))).is_some());
    }

    #[test]
    fn every_readout_names_the_selection_as_its_scope() {
        // The one property a reader relies on: these are the selected cells'
        // numbers, not the table's. Checked over every shape of readout the
        // model can produce.
        let cases = vec![
            stats(
                vec![vec![Value::Integer(1), Value::Integer(2)]],
                Selection::all(1, 2).unwrap(),
            ),
            stats(
                vec![vec![Value::Text("a".into()), Value::Null]],
                Selection::all(1, 2).unwrap(),
            ),
            stats(
                vec![vec![Value::Integer(1), Value::Text("a".into())]],
                Selection::all(1, 2).unwrap(),
            ),
            stats(
                vec![vec![Value::Real(f64::NAN), Value::Real(1.0)]],
                Selection::all(1, 2).unwrap(),
            ),
            stats(vec![vec![Value::Null]], Selection::single((0, 0))),
        ];
        for s in cases {
            let line = s.line();
            assert!(
                line.starts_with(&format!("Selection {}×{}", s.rows, s.cols)),
                "{line}"
            );
            assert!(
                line.contains(&format!(
                    "{} {}",
                    s.cells,
                    if s.cells == 1 { "cell" } else { "cells" }
                )),
                "{line}"
            );
            let note = s.scope_note();
            assert!(note.contains("selected"), "{note}");
            assert!(note.contains("this page only"), "{note}");
            assert!(note.contains("not the whole table"), "{note}");
        }
    }

    #[test]
    fn the_rendered_pieces_reassemble_into_the_copied_line() {
        // The footer draws the three pieces separately so the counts can
        // truncate while the aggregates survive; the clipboard gets `line()`.
        // If those ever disagree, the copy would say something the screen
        // never did.
        for s in [
            stats(
                vec![vec![Value::Integer(1), Value::Text("a".into())]],
                Selection::all(1, 2).unwrap(),
            ),
            stats(
                vec![vec![Value::Text("a".into()), Value::Null]],
                Selection::all(1, 2).unwrap(),
            ),
            stats(
                vec![vec![Value::Real(f64::NAN), Value::Real(2.0)]],
                Selection::all(1, 2).unwrap(),
            ),
        ] {
            let mut pieces = vec![s.scope_prefix(), s.counts_text()];
            pieces.extend(s.aggregate_text());
            assert_eq!(pieces.join(" · "), s.line());
        }

        // The qualifier that scopes a partial sum belongs with the aggregates,
        // not the counts: a narrow footer must never keep "sum 12" while
        // dropping "2 numeric of 4 non-null".
        let mixed = stats(
            vec![
                vec![Value::Integer(5), Value::Text("five".into())],
                vec![Value::Integer(7), Value::Text("seven".into())],
            ],
            Selection::all(2, 2).unwrap(),
        );
        let aggregates = mixed.aggregate_text().expect("a numeric subset");
        assert!(
            aggregates.starts_with("2 numeric of 4 non-null"),
            "{aggregates}"
        );
        assert!(aggregates.contains("sum 12"), "{aggregates}");
        assert!(
            !mixed.counts_text().contains("sum"),
            "{}",
            mixed.counts_text()
        );
    }

    #[test]
    fn a_truncating_span_must_be_able_to_shrink() {
        // The footer draws both halves with `truncate`, which only produces an
        // ellipsis when the box is narrower than its text. A shrink factor
        // below 1 pins a flex item at its content width, so it never narrows,
        // never ellipsises, and gets cut without a mark by the ancestor's
        // `overflow-hidden` — a number silently missing its exponent. So the
        // shed order has to be expressed by making the counts shrink *faster*,
        // never by making the aggregates shrink slower than the default.
        let counts: f64 = COUNTS_SHRINK.parse().expect("a number");
        let aggregates: f64 = AGGREGATE_SHRINK.parse().expect("a number");
        assert!(
            aggregates >= 1.0,
            "the aggregate span must keep at least the default shrink of 1, or \
             its own ellipsis can never fire and a cut number reads as a whole one"
        );
        assert!(
            counts > aggregates,
            "the counts must shed their width before the aggregates do"
        );
    }

    #[test]
    fn the_copied_text_carries_the_scope_with_the_numbers() {
        // A pasted number outlives the tooltip that qualified it, so the
        // clipboard gets the caveat too — FRE-117's "describes the selection,
        // not the table" has to survive leaving the app.
        let s = stats(
            vec![vec![Value::Integer(1), Value::Integer(2)]],
            Selection::all(1, 2).unwrap(),
        );
        let copied = s.copy_text();
        assert!(copied.starts_with(&s.line()), "{copied}");
        assert!(copied.contains("this page only"), "{copied}");
        assert!(copied.contains("not the whole table"), "{copied}");
    }

    #[test]
    fn the_readout_cannot_reach_the_database() {
        // FRE-117 is explicit that the statistics come from cells already in
        // memory: no query, no full-table scan. `compute` takes only a
        // `&GridNav` and a `Selection`, and this pins that nothing in the
        // module's code can await, spawn, or touch a pool — a claim in a doc
        // comment would not survive the first person who added a fetch.
        let source = include_str!("stats.rs");
        // Splitting at the test-module attribute scans everything before it —
        // but only while the file carries exactly one. A second, earlier one
        // (a test-only helper dropped mid-module) would silently shorten the
        // scanned region and leave the rest of the file unexamined, which is
        // the one failure a guard like this must not have. So the count is
        // asserted, not assumed.
        //
        // The marker is spelled with `concat!` so that writing it here doesn't
        // make this test match itself — the first version did, six times over.
        let marker = concat!("#[cfg", "(test)]");
        assert_eq!(
            source.matches(marker).count(),
            1,
            "more than one test-module attribute: the source scan below would \
             stop at the first one and leave the rest of the module unchecked"
        );
        let code = source
            .split(marker)
            .next()
            .expect("the module has a non-test half");
        let code: String = code
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for forbidden in [
            "await",
            "spawn",
            "AppState",
            "DbPool",
            "use_resource",
            "Resource<",
            "load_cell",
            "query",
            "state.",
        ] {
            assert!(
                !code.contains(forbidden),
                "`{forbidden}` in the statistics module: it must compute from the page in hand"
            );
        }
        // These two only prove the scan is reading real code — an item near
        // the start and the module's last function. They do *not* prove it
        // read all of it, and it would be dangerous to think they did: a
        // marker planted after `format_float` passes both sentinels happily,
        // and only the count assertion above catches it. That assertion is
        // what holds this guard up; don't remove it in favour of these.
        assert!(code.contains("fn compute"), "source scan found no code");
        assert!(
            code.contains("fn format_float"),
            "source scan stopped before the module's last function"
        );
    }

    #[test]
    fn numbers_render_readably() {
        assert_eq!(format_float(3.0), "3");
        assert_eq!(format_float(-3.0), "-3");
        assert_eq!(format_float(2.5), "2.5");
        assert_eq!(format_float(1.0 / 3.0), "0.333333");
        assert_eq!(format_float(f64::NAN), "NaN");
        assert_eq!(format_float(f64::INFINITY), "∞");
        assert_eq!(format_float(f64::NEG_INFINITY), "-∞");
        // Beyond i64-printable range, and far below it: exponent form rather
        // than forty digits or a row of zeros.
        assert!(format_float(1e20).contains('e'), "{}", format_float(1e20));
        assert!(format_float(1e-9).contains('e'), "{}", format_float(1e-9));
        assert_eq!(format_float(0.0), "0");
        assert_eq!(Num::Int(i64::MAX).display(), "9223372036854775807");
    }
}
