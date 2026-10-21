//! Postgres binary wire values rendered as the text the server prints for
//! them: `numeric`, `date`, `timestamp`, `timestamptz`, `time` and `timetz`.
//!
//! These used to go through sqlx's decodes into `rust_decimal` and `chrono`,
//! and every one of those types is narrower than the column it was read from.
//! The gaps were not cosmetic:
//!
//! - a `numeric` such as `1e30` made rust_decimal **panic** ("Multiplication
//!   overflowed") on the UI thread, taking the whole app down the moment the
//!   table was opened; NaN, `Infinity` and anything past 28 significant digits
//!   read as a `<numeric>` marker;
//! - chrono counts years astronomically, so `0001-01-01 BC` arrived as
//!   `0000-01-01` and `10000-01-01` as `+10000-01-01` — both refused as
//!   input by PostgreSQL, so a row keyed on one could be shown but not saved
//!   (CockroachDB reads the year 0 back as 1 BC, but refuses the `+`); dates
//!   past chrono's year 262143 read as a marker;
//! - sqlx builds a `time` by adding to midnight, so `24:00:00` — a legal
//!   `time` — read as `00:00:00`, a different value.
//!
//! Every value here is rendered from the wire bytes with plain integer
//! arithmetic instead, which has no range to fall out of, in a spelling the
//! server reads back: the grid's key values are bound straight back as text
//! with a cast to the column's type, so a value that displays but does not
//! parse is a row that cannot be edited.
//!
//! Each function is `None` for bytes it cannot read — the caller degrades
//! those to a marker, which beats indexing past the end of the payload.

use std::fmt::Write as _;

/// Microseconds in a day, the unit a `timestamp` is split into date and
/// time-of-day by.
const MICROS_PER_DAY: i64 = 86_400_000_000;

/// The days from 0000-03-01 (proleptic Gregorian, astronomical year 0) to
/// 2000-01-01, which is day zero on the wire.
const DAYS_TO_PG_EPOCH: i64 = 730_425;

/// A binary `numeric`, exactly as Postgres prints it — every digit, the
/// column's scale kept (`1.50` stays `1.50`), `NaN`, `Infinity`,
/// `-Infinity`.
///
/// The encoding is a header of four 16-bit fields — digit count, the weight
/// (the power of 10000) of the first digit, the sign word and the display
/// scale — followed by the base-10000 digits.
pub(super) fn numeric(bytes: &[u8]) -> Option<String> {
    let (header, digits) = bytes.split_at_checked(8)?;
    let field = |at: usize| u16::from_be_bytes([header[at], header[at + 1]]);
    let count = usize::from(field(0));
    let weight = i32::from(field(2) as i16);
    let scale = usize::from(field(6));
    let negative = match field(4) {
        0x0000 => false,
        0x4000 => true,
        0xC000 => return Some("NaN".into()),
        0xD000 => return Some("Infinity".into()),
        0xF000 => return Some("-Infinity".into()),
        _ => return None,
    };
    // A count with the high bit set is negative, which no server sends.
    if count > i16::MAX as usize || digits.len() != count * 2 {
        return None;
    }
    let digits: Vec<u16> = digits
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_be_bytes(*pair))
        .collect();
    if digits.iter().any(|&d| d > 9999) {
        return None;
    }
    // The digit at index `i` is worth 10000^(weight - i); positions outside
    // the stored digits are zeros the encoding left out.
    let digit = |i: i32| usize::try_from(i).ok().and_then(|i| digits.get(i).copied());
    let digit = |i: i32| digit(i).unwrap_or(0);
    let mut text = String::new();
    if negative {
        text.push('-');
    }
    if weight < 0 {
        text.push('0');
    } else {
        for i in 0..=weight {
            if i == 0 {
                write!(text, "{}", digit(i)).ok()?;
            } else {
                write!(text, "{:04}", digit(i)).ok()?;
            }
        }
    }
    if scale > 0 {
        let mut fraction = String::with_capacity(scale + 4);
        let mut i = weight + 1;
        while fraction.len() < scale {
            write!(fraction, "{:04}", digit(i)).ok()?;
            i += 1;
        }
        fraction.truncate(scale);
        text.push('.');
        text.push_str(&fraction);
    }
    Some(text)
}

/// A binary `date` (days since 2000-01-01): `2024-02-29`, `0044-03-15 BC`,
/// `10000-01-01`, `infinity`.
pub(super) fn date(bytes: &[u8]) -> Option<String> {
    let days = i32::from_be_bytes(bytes.try_into().ok()?);
    Some(match days {
        i32::MAX => "infinity".into(),
        i32::MIN => "-infinity".into(),
        days => {
            let (ymd, era) = calendar_date(i64::from(days));
            format!("{ymd}{era}")
        }
    })
}

/// A binary `timestamp` (microseconds since 2000-01-01 00:00): the date, the
/// time of day, then `zone` and the era — `0044-03-15 12:00:00.5 BC`.
///
/// `zone` is what a `timestamptz` carries: the wire holds an instant, which
/// is rendered in UTC with the offset spelled out so it reads as zoned. The
/// era comes after it because that is the order the server prints
/// (`0001-01-01 00:00:00+00:00 BC`); PostgreSQL also reads the era before
/// the zone, so the order is a matter of display, not of the value.
pub(super) fn timestamp(bytes: &[u8], zone: &str) -> Option<String> {
    let micros = i64::from_be_bytes(bytes.try_into().ok()?);
    Some(match micros {
        i64::MAX => "infinity".into(),
        i64::MIN => "-infinity".into(),
        micros => {
            let (ymd, era) = calendar_date(micros.div_euclid(MICROS_PER_DAY));
            let clock = clock(micros.rem_euclid(MICROS_PER_DAY));
            format!("{ymd} {clock}{zone}{era}")
        }
    })
}

/// A binary `time` (microseconds since midnight), up to and including
/// `24:00:00`, which is a legal value distinct from `00:00:00`.
pub(super) fn time(bytes: &[u8]) -> Option<String> {
    let micros = i64::from_be_bytes(bytes.try_into().ok()?);
    (0..=MICROS_PER_DAY)
        .contains(&micros)
        .then(|| clock(micros))
}

/// A binary `timetz`: the `time`, then the zone as seconds *west* of UTC —
/// so `+02` is sent as -7200. Rendered `07:08:09+02:00`, seconds added to
/// the offset only when it has them.
pub(super) fn time_tz(bytes: &[u8]) -> Option<String> {
    let (time_bytes, zone) = bytes.split_at_checked(8)?;
    let time = time(time_bytes)?;
    let west = i32::from_be_bytes(zone.try_into().ok()?);
    let sign = if west > 0 { '-' } else { '+' };
    let east = west.unsigned_abs();
    let (hours, minutes, seconds) = (east / 3600, east / 60 % 60, east % 60);
    let mut text = format!("{time}{sign}{hours:02}:{minutes:02}");
    if seconds != 0 {
        write!(text, ":{seconds:02}").ok()?;
    }
    Some(text)
}

/// The elements of a binary one-dimensional array, `None` standing for a
/// NULL element. An empty array (zero dimensions) is an empty list; one of
/// two or more dimensions is `None`, as sqlx's own array decode refuses it.
///
/// The encoding is the dimension count, a has-nulls flag and the element
/// type, one (length, lower bound) pair per dimension, then each element as
/// a length (-1 for NULL) and that many bytes.
pub(super) fn array_elements(bytes: &[u8]) -> Option<Vec<Option<&[u8]>>> {
    let int = |at: usize| -> Option<i32> {
        Some(i32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
    };
    match int(0)? {
        0 => return Some(Vec::new()),
        1 => {}
        _ => return None,
    }
    let len = usize::try_from(int(12)?).ok()?;
    let mut at = 20;
    let mut elements = Vec::with_capacity(len.min(bytes.len() / 4));
    for _ in 0..len {
        let size = int(at)?;
        at += 4;
        if size == -1 {
            elements.push(None);
            continue;
        }
        let end = at.checked_add(usize::try_from(size).ok()?)?;
        elements.push(Some(bytes.get(at..end)?));
        at = end;
    }
    (at == bytes.len()).then_some(elements)
}

/// `HH:MM:SS` plus the fraction when there is one, trailing zeros trimmed —
/// how the server prints a time of day.
fn clock(micros: i64) -> String {
    let (seconds, fraction) = (micros / 1_000_000, micros % 1_000_000);
    let (h, m, s) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    let mut text = format!("{h:02}:{m:02}:{s:02}");
    if fraction != 0 {
        text.push_str(format!(".{fraction:06}").trim_end_matches('0'));
    }
    text
}

/// The proleptic Gregorian date `days` after 2000-01-01, as `YYYY-MM-DD`
/// and the era suffix: `""` for AD, `" BC"` before year 1.
///
/// The server numbers years the historical way — there is no year 0, and
/// the year before 1 AD is 1 BC — so astronomical year `y <= 0` is printed
/// as `1 - y` with the suffix. Years print at least four digits wide and
/// never with a sign, which is both what the server prints and what it
/// reads back.
fn calendar_date(days: i64) -> (String, &'static str) {
    // Howard Hinnant's `civil_from_days`, counted from 0000-03-01 so a leap
    // day falls at the end of its year: eras of 400 years (146097 days)
    // repeat exactly.
    let shifted = days + DAYS_TO_PG_EPOCH;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    let (year, suffix) = if year <= 0 {
        (1 - year, " BC")
    } else {
        (year, "")
    };
    (format!("{year:04}-{month:02}-{day:02}"), suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A binary numeric from its header fields and base-10000 digits.
    fn wire_numeric(weight: i16, sign: u16, scale: u16, digits: &[u16]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(digits.len() as u16).to_be_bytes());
        bytes.extend_from_slice(&weight.to_be_bytes());
        bytes.extend_from_slice(&sign.to_be_bytes());
        bytes.extend_from_slice(&scale.to_be_bytes());
        for digit in digits {
            bytes.extend_from_slice(&digit.to_be_bytes());
        }
        bytes
    }

    #[test]
    fn a_numeric_prints_every_digit_however_many_there_are() {
        // 1e30 is 10000^7 * 100: one stored digit, weight 7. rust_decimal
        // panicked on exactly this value.
        assert_eq!(
            numeric(&wire_numeric(7, 0, 0, &[100])).as_deref(),
            Some("1000000000000000000000000000000")
        );
        // 39 significant digits, past rust_decimal's 28.
        assert_eq!(
            numeric(&wire_numeric(
                9,
                0,
                0,
                &[123, 4567, 8901, 2345, 6789, 123, 4567, 8901, 2345, 6789]
            ))
            .as_deref(),
            Some("123456789012345678901234567890123456789")
        );
        // Inner zero groups keep their width: 10000.0001.
        assert_eq!(
            numeric(&wire_numeric(1, 0, 4, &[1, 0, 1])).as_deref(),
            Some("10000.0001")
        );
    }

    #[test]
    fn a_numeric_keeps_its_scale_and_its_sign() {
        // numeric(10,2) holding 1.5 prints 1.50: the scale is part of the
        // value as printed, not padding to strip.
        assert_eq!(
            numeric(&wire_numeric(0, 0, 2, &[1, 5000])).as_deref(),
            Some("1.50")
        );
        // A scale shorter than the stored group truncates it.
        assert_eq!(
            numeric(&wire_numeric(0, 0, 1, &[2, 5000])).as_deref(),
            Some("2.5")
        );
        assert_eq!(
            numeric(&wire_numeric(0, 0x4000, 3, &[12, 3450])).as_deref(),
            Some("-12.345")
        );
        // Weight below zero: the integer part is a lone 0 and the leading
        // fraction groups the encoding omitted are zeros — 1e-31 is
        // 10000^-8 * 10.
        assert_eq!(
            numeric(&wire_numeric(-8, 0, 31, &[10])).as_deref(),
            Some("0.0000000000000000000000000000001")
        );
        // Zero has no digits at all.
        assert_eq!(numeric(&wire_numeric(0, 0, 0, &[])).as_deref(), Some("0"));
        assert_eq!(
            numeric(&wire_numeric(0, 0, 2, &[])).as_deref(),
            Some("0.00")
        );
        // Trailing zero groups the encoding left out are still integer digits.
        assert_eq!(
            numeric(&wire_numeric(2, 0, 0, &[5])).as_deref(),
            Some("500000000")
        );
    }

    #[test]
    fn a_numeric_special_value_prints_its_name() {
        assert_eq!(
            numeric(&wire_numeric(0, 0xC000, 0, &[])).as_deref(),
            Some("NaN")
        );
        assert_eq!(
            numeric(&wire_numeric(0, 0xD000, 0, &[])).as_deref(),
            Some("Infinity")
        );
        assert_eq!(
            numeric(&wire_numeric(0, 0xF000, 0, &[])).as_deref(),
            Some("-Infinity")
        );
    }

    #[test]
    fn a_malformed_numeric_is_refused_rather_than_misread() {
        assert_eq!(numeric(&[0, 1, 0, 0]), None, "header truncated");
        assert_eq!(
            numeric(&wire_numeric(0, 0x1234, 0, &[1])),
            None,
            "unknown sign word"
        );
        assert_eq!(
            numeric(&wire_numeric(0, 0, 0, &[10_000])),
            None,
            "a base-10000 digit out of range"
        );
        let mut short = wire_numeric(0, 0, 0, &[1, 2]);
        short.pop();
        assert_eq!(numeric(&short), None, "count claims more than was sent");
        let mut long = wire_numeric(0, 0, 0, &[1]);
        long.extend_from_slice(&[0, 2]);
        assert_eq!(numeric(&long), None, "bytes past the counted digits");
    }

    fn days(n: i32) -> [u8; 4] {
        n.to_be_bytes()
    }

    fn micros(n: i64) -> [u8; 8] {
        n.to_be_bytes()
    }

    #[test]
    fn a_date_prints_its_calendar_day() {
        assert_eq!(date(&days(0)).as_deref(), Some("2000-01-01"));
        assert_eq!(date(&days(-1)).as_deref(), Some("1999-12-31"));
        assert_eq!(date(&days(59)).as_deref(), Some("2000-02-29"));
        assert_eq!(date(&days(8825)).as_deref(), Some("2024-02-29"));
        assert_eq!(date(&days(-10957)).as_deref(), Some("1970-01-01"));
        // 1900 is not a leap year; 1600 is.
        assert_eq!(date(&days(-36465)).as_deref(), Some("1900-03-01"));
        assert_eq!(date(&days(-146038)).as_deref(), Some("1600-02-29"));
    }

    #[test]
    fn a_date_before_year_one_is_bc_and_not_year_zero() {
        // The day before 0001-01-01 is 0001-12-31 BC; astronomically it is
        // year 0, which the server refuses as input.
        assert_eq!(date(&days(-730119)).as_deref(), Some("0001-01-01"));
        assert_eq!(date(&days(-730120)).as_deref(), Some("0001-12-31 BC"));
        assert_eq!(date(&days(-730485)).as_deref(), Some("0001-01-01 BC"));
        assert_eq!(date(&days(-730486)).as_deref(), Some("0002-12-31 BC"));
        // The server's earliest date.
        assert_eq!(date(&days(-2451545)).as_deref(), Some("4714-11-24 BC"));
    }

    #[test]
    fn a_date_past_year_9999_has_no_sign() {
        assert_eq!(date(&days(2921940)).as_deref(), Some("10000-01-01"));
        // The server's latest date, far past chrono's year 262143.
        assert_eq!(date(&days(2145031948)).as_deref(), Some("5874897-12-31"));
    }

    #[test]
    fn infinite_dates_and_timestamps_print_their_names() {
        assert_eq!(date(&days(i32::MAX)).as_deref(), Some("infinity"));
        assert_eq!(date(&days(i32::MIN)).as_deref(), Some("-infinity"));
        assert_eq!(
            timestamp(&micros(i64::MAX), "").as_deref(),
            Some("infinity")
        );
        assert_eq!(
            timestamp(&micros(i64::MIN), "+00:00").as_deref(),
            Some("-infinity")
        );
    }

    #[test]
    fn a_timestamp_prints_its_date_then_its_time_then_its_era() {
        const DAY: i64 = MICROS_PER_DAY;
        assert_eq!(
            timestamp(&micros(0), "").as_deref(),
            Some("2000-01-01 00:00:00")
        );
        // Before the epoch the time of day still counts forward from
        // midnight: one microsecond before 2000 is the last of 1999.
        assert_eq!(
            timestamp(&micros(-1), "").as_deref(),
            Some("1999-12-31 23:59:59.999999")
        );
        assert_eq!(
            timestamp(&micros(8825 * DAY + 49_501_123_456), "").as_deref(),
            Some("2024-02-29 13:45:01.123456")
        );
        assert_eq!(
            timestamp(&micros(-730485 * DAY + 43_200_500_000), "").as_deref(),
            Some("0001-01-01 12:00:00.5 BC")
        );
        // The zone goes before the era, as the server prints it.
        assert_eq!(
            timestamp(&micros(-730485 * DAY), "+00:00").as_deref(),
            Some("0001-01-01 00:00:00+00:00 BC")
        );
    }

    #[test]
    fn a_malformed_date_or_time_is_refused() {
        assert_eq!(date(&[0, 0, 0]), None);
        assert_eq!(timestamp(&[0; 4], ""), None);
        assert_eq!(time(&[0; 4]), None);
        assert_eq!(time(&micros(-1)), None, "before midnight");
        assert_eq!(time(&micros(MICROS_PER_DAY + 1)), None, "past 24:00");
        assert_eq!(time_tz(&micros(0)), None, "zone missing");
    }

    #[test]
    fn a_time_can_be_the_end_of_the_day() {
        assert_eq!(time(&micros(0)).as_deref(), Some("00:00:00"));
        assert_eq!(time(&micros(25_689_500_000)).as_deref(), Some("07:08:09.5"));
        // 24:00:00 is its own value; adding it to midnight wrapped it to
        // 00:00:00.
        assert_eq!(time(&micros(MICROS_PER_DAY)).as_deref(), Some("24:00:00"));
    }

    fn wire_time_tz(micros: i64, west: i32) -> Vec<u8> {
        let mut bytes = micros.to_be_bytes().to_vec();
        bytes.extend_from_slice(&west.to_be_bytes());
        bytes
    }

    #[test]
    fn a_time_with_zone_prints_the_offset_east_of_utc() {
        // The wire counts seconds *west*: +02 arrives as -7200.
        assert_eq!(
            time_tz(&wire_time_tz(25_689_000_000, -7200)).as_deref(),
            Some("07:08:09+02:00")
        );
        assert_eq!(
            time_tz(&wire_time_tz(0, 12_600)).as_deref(),
            Some("00:00:00-03:30")
        );
        assert_eq!(
            time_tz(&wire_time_tz(MICROS_PER_DAY, 0)).as_deref(),
            Some("24:00:00+00:00")
        );
        assert_eq!(
            time_tz(&wire_time_tz(0, -3630)).as_deref(),
            Some("00:00:00+01:00:30")
        );
    }

    /// A binary one-dimensional array of the given elements.
    fn wire_array(elements: &[Option<&[u8]>]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for int in [1, 0, 1700, elements.len() as i32, 1] {
            bytes.extend_from_slice(&int.to_be_bytes());
        }
        for element in elements {
            match element {
                Some(element) => {
                    bytes.extend_from_slice(&(element.len() as i32).to_be_bytes());
                    bytes.extend_from_slice(element);
                }
                None => bytes.extend_from_slice(&(-1i32).to_be_bytes()),
            }
        }
        bytes
    }

    #[test]
    fn array_elements_are_split_at_their_lengths() {
        let wire = wire_array(&[Some(b"ab"), None, Some(b""), Some(b"c")]);
        assert_eq!(
            array_elements(&wire),
            Some(vec![
                Some(&b"ab"[..]),
                None,
                Some(&b""[..]),
                Some(&b"c"[..])
            ])
        );
        // An empty array has no dimensions at all.
        assert_eq!(array_elements(&[0; 12]), Some(Vec::new()));
    }

    #[test]
    fn a_malformed_or_nested_array_is_refused() {
        let wire = wire_array(&[Some(b"ab")]);
        assert_eq!(array_elements(&wire[..wire.len() - 1]), None, "cut short");
        let mut long = wire.clone();
        long.push(0);
        assert_eq!(array_elements(&long), None, "bytes past the last element");
        let mut nested = wire.clone();
        nested[3] = 2;
        assert_eq!(array_elements(&nested), None, "two dimensions");
        let mut huge = wire.clone();
        huge[20..24].copy_from_slice(&i32::MAX.to_be_bytes());
        assert_eq!(array_elements(&huge), None, "element longer than the wire");
        assert_eq!(array_elements(&[0, 0]), None, "header truncated");
    }
}
