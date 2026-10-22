//! `EXT-X-PROGRAM-DATE-TIME` formatting (RFC 8216 §4.4.4.6) — **pure integer**
//! ISO 8601 from Unix nanoseconds (ADR-M010, DEV-C1).
//!
//! The instant comes from the outbound presentation epoch
//! (`epoch.wall_at(segment first PTS)`), already exact integer ns; this module
//! renders it as `YYYY-MM-DDThh:mm:ss.mmmZ` (UTC, millisecond precision) with
//! no float and no date-time dependency. The civil-date split is the standard
//! days-to-`{y,m,d}` algorithm over the proleptic Gregorian calendar
//! (era/year-of-era/day-of-year integer arithmetic), exact for the whole i64
//! nanosecond range (1677–2262).
//!
//! ## Leap seconds
//!
//! The input is **Unix time**, which is leap-second-naive by definition
//! (every day is exactly 86 400 s; a positive leap second is absorbed by the
//! clock discipline smearing or stepping, never rendered as `:60`). This
//! formatter inherits that naivety: it can never emit `23:59:60`, and around
//! a leap event the rendered instants are only as accurate as the host's
//! handling of it (chrony's slew/smear, or the kernel step). That matches
//! what HLS consumers expect — `EXT-X-PROGRAM-DATE-TIME` carriers in the wild
//! are POSIX-clock-derived — and no leap-second table is maintained here.

/// Nanoseconds per civil day.
const NS_PER_DAY: i64 = 86_400_000_000_000;

/// Format a Unix instant (integer ns past the epoch, UTC) as an HLS
/// `PROGRAM-DATE-TIME` value: `YYYY-MM-DDThh:mm:ss.mmmZ`.
///
/// Sub-millisecond precision is **floored** (never rounded up across a
/// second boundary); pre-1970 instants resolve via Euclidean division, so the
/// rendered civil time is always valid.
#[must_use]
pub fn format_program_date_time(unix_ns: i64) -> String {
    let days = unix_ns.div_euclid(NS_PER_DAY);
    let ns_of_day = unix_ns.rem_euclid(NS_PER_DAY);

    let secs_of_day = ns_of_day.div_euclid(1_000_000_000);
    let millis = ns_of_day.rem_euclid(1_000_000_000).div_euclid(1_000_000);
    let hour = secs_of_day.div_euclid(3_600);
    let minute = secs_of_day.rem_euclid(3_600).div_euclid(60);
    let second = secs_of_day.rem_euclid(60);

    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Convert a count of days since 1970-01-01 to a proleptic-Gregorian civil
/// date `(year, month, day)` — the standard integer era/year-of-era/day-of-
/// year algorithm (400-year eras of 146 097 days), exact for any `i64` day
/// count this module can receive.
const fn civil_from_days(days: i64) -> (i64, i64, i64) {
    // Shift the epoch from 1970-01-01 to 0000-03-01 (the era origin), so leap
    // days land at the END of each cycle year (March-first years).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // day of era    [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year [0, 365]
    let mp = (5 * doy + 2) / 153; // March-based month [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}
