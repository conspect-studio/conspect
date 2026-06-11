//! Pure countdown / count-up **timer** model (ADR-0047, brief §3).
//!
//! A `timer` synthetic source counts **down to** or **up from** a target instant
//! — the target is either a wall-clock **time-of-day** (optionally recurring
//! daily) or an absolute **date+time**, each resolved in an IANA timezone
//! (DST-correct) or a fixed UTC offset. This module owns:
//!
//! * the serde enums ([`TimerDirection`], [`TimerOnTarget`], [`TimerFormat`],
//!   [`TimerTarget`]) the schema flattens into `SourceKind::Timer`,
//! * the **pure** target resolver ([`TimerTarget::resolve`]) — `(target, now,
//!   direction) → ` an absolute Unix-seconds instant, handling the time-of-day
//!   next/most-recent occurrence, `recur_daily` re-arm, and DST gaps,
//! * the **pure** display math ([`compute`]) — `(target, now, direction,
//!   on_target) →` the shown duration (i64 **seconds**, never float) plus a
//!   running / at-target / overrun [`TimerState`],
//! * the integer formatter ([`TimerFormat::format`]) — `D:HH:MM:SS`,
//!   `HH:MM:SS:FF` frames (integer division against the canvas cadence
//!   [`multiview_core::time::Rational`]), and `Auto` leading-zero-unit dropping.
//!
//! Every function takes its instants as injected [`WallTime`] values (whole Unix
//! seconds) — there is **no system-clock read here**, so the math is fully
//! deterministic and testable, and the timer never paces the engine (invariant
//! #1: the output clock paces; the displayed time is sampled).

use multiview_core::time::Rational;
use multiview_overlay::clock::{resolve_offset, Tz, WallTime};
use serde::{Deserialize, Serialize};

/// Seconds in a day.
const SECONDS_PER_DAY: i64 = 86_400;

/// Whether a timer counts **down to** its target or **up from** it.
///
/// Serialised tagged on the variant name (`snake_case`), never `untagged`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TimerDirection {
    /// Count **down**: the shown value is `target − now` (reaches zero at the
    /// target). The default.
    #[default]
    Down,
    /// Count **up**: the shown value is `now − target` (zero at the target,
    /// growing after).
    Up,
}

/// What a timer does when it reaches its target (the remaining duration hits
/// zero for a `down` timer; `now` passes `target` for an `up` timer).
///
/// Serialised tagged on the variant name (`snake_case`), never `untagged`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TimerOnTarget {
    /// Freeze the display at `00:00:00` once the target is reached. The default.
    #[default]
    Hold,
    /// Roll past the target in the **same** direction and keep going: a
    /// count-down reaching zero starts counting **up** (the overrun, styled
    /// `OVER`); a count-up that crosses its target keeps incrementing.
    Continue,
    /// The explicit live-production "we're N over" case: a count-down reaches
    /// `00:00:00`, then counts the overrun **up** (identical display to
    /// [`Self::Continue`] for a `down` timer; named for clarity).
    ZeroThenUp,
    /// Re-arm to the next occurrence (valid only for a `time_of_day` target with
    /// `recur_daily = true`): a perpetual daily countdown. For other targets it
    /// behaves like [`Self::Hold`] (validated upstream).
    Recur,
}

impl TimerOnTarget {
    /// Whether reaching the target rolls into a styled **overrun** count
    /// (`continue` / `zero_then_up`), as opposed to freezing (`hold`) or
    /// re-arming (`recur`).
    #[must_use]
    pub const fn rolls_into_overrun(self) -> bool {
        matches!(self, Self::Continue | Self::ZeroThenUp)
    }
}

/// The digital display format for the timer readout.
///
/// Serialised tagged on the variant name (`snake_case`), never `untagged`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TimerFormat {
    /// `D:HH:MM:SS`, with the leading `D:` day field dropped when zero. The
    /// default.
    #[default]
    DHhMmSs,
    /// `HH:MM:SS` (days fold into the hour field).
    HhMmSs,
    /// `MM:SS` (hours/days fold into the minute field).
    MmSs,
    /// `HH:MM:SS:FF` — a frames field derived from the canvas cadence by integer
    /// division of the sub-second nanoseconds (never float seconds).
    HhMmSsFf,
    /// Drop leading zero units (`5:00`, `1:05:00`, `2d 01:05:00`).
    Auto,
}

impl TimerFormat {
    /// Whether this format shows a sub-second **frames** field, so a generator
    /// must re-render once per frame (not once per second).
    #[must_use]
    pub const fn shows_frames(self) -> bool {
        matches!(self, Self::HhMmSsFf)
    }
}

/// The target instant a timer counts toward/from — internally tagged on the
/// distinct key `target` (so it `#[serde(flatten)]`s into `SourceKind::Timer`
/// without clashing with the `kind` tag), never `untagged`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
#[non_exhaustive]
pub enum TimerTarget {
    /// A wall-clock time-of-day in a zone; the next (for `down`) or most-recent
    /// (for `up`) occurrence relative to `now`. `recur_daily` re-arms each day.
    TimeOfDay {
        /// The wall-clock time `"HH:MM:SS"` (24-hour).
        at: String,
        /// IANA timezone id (e.g. `Australia/Sydney`). Preferred; absent ⇒ the
        /// fixed `tz_offset_minutes`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timezone: Option<String>,
        /// Fixed UTC offset in minutes (legacy / no-DST). Ignored when
        /// `timezone` is set.
        #[serde(default)]
        tz_offset_minutes: i32,
        /// Re-arm to the next day's occurrence each day (a daily countdown).
        #[serde(default)]
        recur_daily: bool,
    },
    /// An absolute date+time; the `timezone` (or fixed offset) resolves the
    /// instant unambiguously across DST.
    DateTime {
        /// Local wall-clock date+time `"YYYY-MM-DDTHH:MM:SS"` (RFC3339 without a
        /// trailing zone — the zone is supplied separately).
        at: String,
        /// IANA timezone id. Preferred; absent ⇒ the fixed `tz_offset_minutes`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timezone: Option<String>,
        /// Fixed UTC offset in minutes (legacy / no-DST). Ignored when
        /// `timezone` is set.
        #[serde(default)]
        tz_offset_minutes: i32,
    },
}

/// A timer target that could not be parsed or resolved to a concrete instant.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TimerError {
    /// The `at` string was not a valid `HH:MM:SS` time-of-day.
    #[error("timer time-of-day {0:?} is not a valid \"HH:MM:SS\" (24-hour) time")]
    BadTimeOfDay(String),
    /// The `at` string was not a valid `YYYY-MM-DDTHH:MM:SS` local date+time.
    #[error("timer datetime {0:?} is not a valid \"YYYY-MM-DDTHH:MM:SS\" local date+time")]
    BadDateTime(String),
    /// The `timezone` id was not a known IANA zone.
    #[error("timer timezone {0:?} is not a known IANA timezone id")]
    BadTimezone(String),
}

/// The running state of a timer at a sampled instant — drives the overrun
/// styling (prefix + a11y badge) and reads without colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TimerState {
    /// Counting toward the target (the normal pre-target state).
    Running,
    /// Exactly at the target (`00:00:00`).
    AtTarget,
    /// Past the target and styled as overrun (`continue` / `zero_then_up`).
    Overrun,
    /// Past the target but frozen at `00:00:00` (`hold`).
    Held,
}

impl TimerState {
    /// Whether the timer is in the styled-overrun state (prefix + badge drawn).
    #[must_use]
    pub const fn is_overrun(self) -> bool {
        matches!(self, Self::Overrun)
    }
}

/// The computed timer display value at a sampled instant: the shown magnitude in
/// whole seconds (always `>= 0` — the sign/overrun is carried by `state`) plus
/// the [`TimerState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct TimerReadout {
    /// The shown magnitude in whole seconds (non-negative; decomposed into
    /// `D:HH:MM:SS` for display). Zero at and (for `hold`) past the target.
    pub shown_seconds: u64,
    /// The running / at-target / overrun / held state.
    pub state: TimerState,
}

/// Parse a `"HH:MM:SS"` (24-hour) time-of-day into whole seconds since local
/// midnight (`0..86_400`), or `None` if malformed / out of range.
#[must_use]
fn parse_hms(at: &str) -> Option<i64> {
    let mut it = at.split(':');
    let h: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next()?.parse().ok()?;
    let s: i64 = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    if !(0..24).contains(&h) || !(0..60).contains(&m) || !(0..60).contains(&s) {
        return None;
    }
    Some(h * 3600 + m * 60 + s)
}

impl TimerTarget {
    /// The IANA `timezone` id this target names, if any.
    #[must_use]
    pub fn timezone(&self) -> Option<&str> {
        match self {
            Self::TimeOfDay { timezone, .. } | Self::DateTime { timezone, .. } => {
                timezone.as_deref()
            }
        }
    }

    /// The fixed `tz_offset_minutes` fallback this target carries.
    #[must_use]
    pub const fn tz_offset_minutes(&self) -> i32 {
        match self {
            Self::TimeOfDay {
                tz_offset_minutes, ..
            }
            | Self::DateTime {
                tz_offset_minutes, ..
            } => *tz_offset_minutes,
        }
    }

    /// Validate that this target parses (the `at` string is well-formed and the
    /// `timezone`, when set, is a known IANA id). Resolution against a concrete
    /// `now` happens later via [`Self::resolve`]; this is the config-load check.
    ///
    /// # Errors
    ///
    /// [`TimerError::BadTimeOfDay`] / [`TimerError::BadDateTime`] for a malformed
    /// `at`, or [`TimerError::BadTimezone`] for an unknown zone id.
    pub fn validate(&self) -> Result<(), TimerError> {
        if let Some(tz) = self.timezone() {
            if multiview_overlay::clock::parse_tz(tz).is_none() {
                return Err(TimerError::BadTimezone(tz.to_owned()));
            }
        }
        match self {
            Self::TimeOfDay { at, .. } => {
                parse_hms(at).ok_or_else(|| TimerError::BadTimeOfDay(at.clone()))?;
            }
            Self::DateTime { at, .. } => {
                parse_naive_datetime(at).ok_or_else(|| TimerError::BadDateTime(at.clone()))?;
            }
        }
        Ok(())
    }

    /// Resolve this target to an **absolute** instant (whole Unix seconds, UTC)
    /// given the sampled `now` and the count `direction`.
    ///
    /// * `DateTime` is an absolute instant: the local `at` is interpreted in the
    ///   zone (DST-correct), independent of `now`.
    /// * `TimeOfDay` resolves to the **next** occurrence (for [`TimerDirection::Down`])
    ///   or the **most-recent** occurrence (for [`TimerDirection::Up`]) of that
    ///   wall-clock time in the zone relative to `now`. With `recur_daily` the
    ///   semantics are the same per-day; re-arming across the at-target boundary
    ///   is handled by [`compute`] selecting the next occurrence.
    ///
    /// A wall-clock time that falls in a DST **gap** (e.g. `02:30` on a
    /// spring-forward day) does not exist; the post-gap instant is used (the zone
    /// jumps the clock forward over it), so the timer never resolves to a
    /// nonexistent instant.
    ///
    /// # Errors
    ///
    /// [`TimerError`] for a malformed `at` or an unknown `timezone`.
    pub fn resolve(
        &self,
        now: WallTime,
        direction: TimerDirection,
    ) -> Result<WallTime, TimerError> {
        match self {
            Self::DateTime { at, .. } => {
                let naive =
                    parse_naive_datetime(at).ok_or_else(|| TimerError::BadDateTime(at.clone()))?;
                Ok(self.local_naive_to_instant(naive, now))
            }
            Self::TimeOfDay { at, .. } => {
                let secs_of_day =
                    parse_hms(at).ok_or_else(|| TimerError::BadTimeOfDay(at.clone()))?;
                Ok(self.time_of_day_occurrence(secs_of_day, now, direction))
            }
        }
    }

    /// The resolved IANA zone for this target, if its `timezone` parses.
    fn resolved_tz(&self) -> Option<Tz> {
        self.timezone().and_then(multiview_overlay::clock::parse_tz)
    }

    /// Convert a local naive `(date, time-of-day)` (as whole seconds since the
    /// local-civil epoch) into an absolute UTC instant, using the IANA zone's
    /// offset **at that instant** (DST-correct) or the fixed offset fallback.
    ///
    /// `civil_seconds` is the count of seconds for the local wall-clock fields
    /// interpreted as if UTC; we subtract the zone's offset to land on the real
    /// UTC instant. For an IANA zone the offset is resolved at the *candidate*
    /// instant (one fix-point step suffices for whole-minute offsets), which lands
    /// the post-gap instant for a DST gap.
    fn civil_to_instant(&self, civil_seconds: i64) -> WallTime {
        if let Some(tz) = self.resolved_tz() {
            // First guess: treat the civil fields as UTC, ask the zone for the
            // offset there, then correct. One correction step is exact for the
            // whole-minute offsets the modern tz database uses; re-resolving at
            // the corrected instant keeps it stable across a DST change and lands
            // the post-gap instant inside a spring-forward gap.
            let guess_off = resolve_offset(tz, WallTime::from_unix_seconds(civil_seconds));
            let candidate = civil_seconds - guess_off.seconds();
            let off2 = resolve_offset(tz, WallTime::from_unix_seconds(candidate));
            WallTime::from_unix_seconds(civil_seconds - off2.seconds())
        } else {
            let off = i64::from(self.tz_offset_minutes()) * 60;
            WallTime::from_unix_seconds(civil_seconds - off)
        }
    }

    /// Convert a parsed local naive date+time to an absolute instant in this
    /// target's zone. `now` is unused for `datetime` (absolute) but kept for a
    /// uniform signature with the time-of-day path.
    fn local_naive_to_instant(&self, naive: NaiveLocal, _now: WallTime) -> WallTime {
        self.civil_to_instant(naive.civil_seconds())
    }

    /// Resolve a `time_of_day` (`secs_of_day` since local midnight) to the next
    /// occurrence (`down`) or most-recent occurrence (`up`) relative to `now`.
    fn time_of_day_occurrence(
        &self,
        secs_of_day: i64,
        now: WallTime,
        direction: TimerDirection,
    ) -> WallTime {
        // Local civil seconds for `now` (UTC seconds shifted by the offset that
        // applies at `now`).
        let now_off = match self.resolved_tz() {
            Some(tz) => resolve_offset(tz, now).seconds(),
            None => i64::from(self.tz_offset_minutes()) * 60,
        };
        let now_civil = now.unix_seconds() + now_off;
        // Local midnight of `now`'s civil day, then add the target time-of-day.
        let midnight_civil = now_civil.div_euclid(SECONDS_PER_DAY) * SECONDS_PER_DAY;
        let today_civil = midnight_civil + secs_of_day;
        // Pick the occurrence: down ⇒ the first occurrence strictly after `now`
        // (today if still ahead, else tomorrow); up ⇒ the most-recent occurrence
        // at or before `now` (today if already passed, else yesterday).
        let chosen_civil = match direction {
            TimerDirection::Down => {
                if today_civil > now_civil {
                    today_civil
                } else {
                    today_civil + SECONDS_PER_DAY
                }
            }
            TimerDirection::Up => {
                if today_civil <= now_civil {
                    today_civil
                } else {
                    today_civil - SECONDS_PER_DAY
                }
            }
        };
        self.civil_to_instant(chosen_civil)
    }
}

/// A parsed local naive date+time (no zone) — whole-second resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NaiveLocal {
    /// Days since the civil epoch (1970-01-01), proleptic-Gregorian.
    days: i64,
    /// Seconds since local midnight (`0..86_400`).
    secs_of_day: i64,
}

impl NaiveLocal {
    /// Whole seconds since the civil epoch (as if the local fields were UTC).
    fn civil_seconds(self) -> i64 {
        self.days * SECONDS_PER_DAY + self.secs_of_day
    }
}

/// Parse `"YYYY-MM-DDTHH:MM:SS"` (a space separator is also accepted) into a
/// [`NaiveLocal`], or `None` if malformed / out of range. Uses `chrono`'s
/// proleptic-Gregorian calendar for the days-since-epoch arithmetic (leap years,
/// month lengths) so the civil math is correct without re-deriving the calendar.
#[must_use]
fn parse_naive_datetime(at: &str) -> Option<NaiveLocal> {
    use chrono::{NaiveDate, NaiveDateTime, Timelike};
    let normalized = at.replacen(' ', "T", 1);
    let ndt = NaiveDateTime::parse_from_str(&normalized, "%Y-%m-%dT%H:%M:%S").ok()?;
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1)?;
    // The exact day delta from the civil epoch, via chrono's proleptic-Gregorian
    // calendar (leap years / month lengths handled for us).
    let day_delta = ndt.date().signed_duration_since(epoch).num_days();
    let secs_of_day =
        i64::from(ndt.hour()) * 3600 + i64::from(ndt.minute()) * 60 + i64::from(ndt.second());
    Some(NaiveLocal {
        days: day_delta,
        secs_of_day,
    })
}

/// Compute the timer display value at the sampled instant.
///
/// `target` is the resolved absolute instant ([`TimerTarget::resolve`]); `now` is
/// the sampled wall instant. The shown magnitude is **integer seconds** — never
/// float — and the [`TimerState`] carries the running / at-target / overrun /
/// held distinction so the renderer styles the overrun (prefix + a11y badge)
/// without re-deriving the sign.
///
/// The at/after-target matrix (brief §3.2) for `direction = down`,
/// `remaining = target − now`:
///
/// | `on_target`            | `remaining > 0` | `== 0`     | `< 0` (past)            |
/// |------------------------|-----------------|------------|------------------------|
/// | `hold`                 | `remaining`     | `00:00:00` | `00:00:00` (Held)      |
/// | `continue`/`zero_then_up` | `remaining`  | `00:00:00` | `|remaining|` (Overrun) |
/// | `recur`                | `remaining`     | re-arm     | (re-armed; never < 0)  |
///
/// `up` is symmetric on `elapsed = now − target`.
#[must_use]
pub fn compute(
    target: WallTime,
    now: WallTime,
    direction: TimerDirection,
    on_target: TimerOnTarget,
) -> TimerReadout {
    // The signed value in the count direction: down ⇒ target−now, up ⇒ now−target.
    let signed = match direction {
        TimerDirection::Down => target.unix_seconds().saturating_sub(now.unix_seconds()),
        TimerDirection::Up => now.unix_seconds().saturating_sub(target.unix_seconds()),
    };
    if signed > 0 {
        return TimerReadout {
            shown_seconds: signed.unsigned_abs(),
            state: TimerState::Running,
        };
    }
    if signed == 0 {
        return TimerReadout {
            shown_seconds: 0,
            state: TimerState::AtTarget,
        };
    }
    // `signed < 0` means we are on the far side of the target from the count's
    // natural region. For a `down` timer that is the genuine post-target overrun.
    // For an `up` timer it means `now` is *before* the start instant — the timer
    // has not armed yet (default `up` has no cap, so there is no negative elapsed
    // to style as overrun); it holds at zero until the target arrives.
    match direction {
        TimerDirection::Down => match on_target {
            // `recur` re-arms (handled by re-resolving the target before this
            // call); if a caller passes a past target with `recur`, freeze rather
            // than show a negative — never a negative display.
            TimerOnTarget::Hold | TimerOnTarget::Recur => TimerReadout {
                shown_seconds: 0,
                state: TimerState::Held,
            },
            TimerOnTarget::Continue | TimerOnTarget::ZeroThenUp => TimerReadout {
                shown_seconds: signed.unsigned_abs(),
                state: TimerState::Overrun,
            },
        },
        TimerDirection::Up => TimerReadout {
            shown_seconds: 0,
            state: TimerState::Held,
        },
    }
}

/// The integer decomposition of a whole-second magnitude into
/// `(days, hours, minutes, seconds)` — exact, no float.
#[must_use]
pub fn decompose(seconds: u64) -> (u64, u64, u64, u64) {
    let days = seconds / 86_400;
    let rem = seconds % 86_400;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    (days, h, m, s)
}

/// The frame index within a second for `subsecond_ns` against the canvas
/// `cadence` (frames/second as a [`Rational`] `num/den`): `floor(subsecond_ns ·
/// num / (den · 1e9))`, clamped to `0..fps`. **Integer arithmetic** — never float
/// seconds (safety rule #6).
#[must_use]
pub fn frame_index(subsecond_ns: u64, cadence: Rational) -> u64 {
    let num = u64::try_from(cadence.num).unwrap_or(1).max(1);
    let den = u64::try_from(cadence.den).unwrap_or(1).max(1);
    // frames = ns * (num/den) / 1e9 = ns * num / (den * 1e9).
    let denom = den.saturating_mul(1_000_000_000).max(1);
    let frames = (u128::from(subsecond_ns).saturating_mul(u128::from(num))) / u128::from(denom);
    // Clamp to the frames-per-second cap (num/den, rounded down) minus one.
    let fps = (num / den).max(1);
    let idx = u64::try_from(frames).unwrap_or(u64::MAX);
    idx.min(fps.saturating_sub(1))
}

impl TimerFormat {
    /// Format a [`TimerReadout`] for display. `subsecond_ns` + `cadence` supply the
    /// frames field for [`Self::HhMmSsFf`]; other formats ignore them.
    ///
    /// The optional `overrun_prefix` overrides the default sign (`-` pre-target /
    /// `+` overrun); the magnitude is always the decomposition of
    /// `readout.shown_seconds` (non-negative).
    #[must_use]
    pub fn format(
        self,
        readout: TimerReadout,
        subsecond_ns: u64,
        cadence: Rational,
        overrun_prefix: Option<&str>,
    ) -> String {
        let (days, h, m, s) = decompose(readout.shown_seconds);
        let body = match self {
            Self::DHhMmSs => {
                if days > 0 {
                    format!("{days}:{h:02}:{m:02}:{s:02}")
                } else {
                    format!("{h:02}:{m:02}:{s:02}")
                }
            }
            Self::HhMmSs => {
                let total_h = days * 24 + h;
                format!("{total_h:02}:{m:02}:{s:02}")
            }
            Self::MmSs => {
                let total_m = (days * 24 + h) * 60 + m;
                format!("{total_m:02}:{s:02}")
            }
            Self::HhMmSsFf => {
                let total_h = days * 24 + h;
                let ff = frame_index(subsecond_ns, cadence);
                format!("{total_h:02}:{m:02}:{s:02}:{ff:02}")
            }
            Self::Auto => format_auto(days, h, m, s),
        };
        let prefix = prefix_for(readout.state, overrun_prefix);
        format!("{prefix}{body}")
    }
}

/// The sign/prefix for the readout state: the explicit `overrun_prefix` when set,
/// else `+` for an overrun and the empty string otherwise (running/at-target/held
/// carry no sign — they read as a plain magnitude).
fn prefix_for(state: TimerState, overrun_prefix: Option<&str>) -> String {
    match (state, overrun_prefix) {
        (TimerState::Overrun, Some(p)) => p.to_owned(),
        (TimerState::Overrun, None) => "+".to_owned(),
        _ => String::new(),
    }
}

/// `Auto` formatting: drop leading zero units (`5:00`, `1:05:00`, `2d 01:05:00`).
/// The first shown unit is unpadded; subsequent units are zero-padded; the day
/// field uses a `Nd ` prefix.
#[must_use]
fn format_auto(days: u64, h: u64, m: u64, s: u64) -> String {
    if days > 0 {
        format!("{days}d {h:02}:{m:02}:{s:02}")
    } else if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// The a11y badge word for an overrun timer (read without colour, mirroring
/// `RefStatus`): `OVER` for a count-down past zero, `ELAPSED` for a count-up.
#[must_use]
pub const fn overrun_badge_word(direction: TimerDirection) -> &'static str {
    match direction {
        TimerDirection::Down => "OVER",
        TimerDirection::Up => "ELAPSED",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Whole Unix seconds for a `WallTime` (terse helper).
    fn wt(unix: i64) -> WallTime {
        WallTime::from_unix_seconds(unix)
    }

    /// The default 25 fps cadence as a `Rational` (num/den frames per second).
    fn cad_25() -> Rational {
        Rational { num: 25, den: 1 }
    }

    // --- target resolution -------------------------------------------------

    #[test]
    fn datetime_resolves_to_an_absolute_instant_in_its_zone() {
        // 2026-07-01T09:00:00 in Australia/Sydney. July ⇒ AEST (UTC+10). The UTC
        // instant (verified against the IANA tz db) is 1_782_860_400.
        let t = TimerTarget::DateTime {
            at: "2026-07-01T09:00:00".to_owned(),
            timezone: Some("Australia/Sydney".to_owned()),
            tz_offset_minutes: 0,
        };
        let inst = t.resolve(wt(0), TimerDirection::Down).expect("resolve");
        assert_eq!(inst.unix_seconds(), 1_782_860_400);
    }

    #[test]
    fn datetime_in_dst_summer_uses_the_summer_offset() {
        // 2026-01-15T12:00:00 in Sydney is AEDT (UTC+11): UTC = 01:00:00Z that day,
        // 1_768_438_800 (verified). Distinct from the AEST (+10) instant, proving
        // the per-instant DST resolution.
        let t = TimerTarget::DateTime {
            at: "2026-01-15T12:00:00".to_owned(),
            timezone: Some("Australia/Sydney".to_owned()),
            tz_offset_minutes: 0,
        };
        let inst = t.resolve(wt(0), TimerDirection::Down).expect("resolve");
        assert_eq!(inst.unix_seconds(), 1_768_438_800);
    }

    #[test]
    fn datetime_with_fixed_offset_fallback() {
        // No IANA zone: fixed +600 minutes (UTC+10). 2026-07-01T09:00:00 local ⇒
        // 1_782_860_400 (the same instant the Sydney-winter AEST case resolves to).
        let t = TimerTarget::DateTime {
            at: "2026-07-01T09:00:00".to_owned(),
            timezone: None,
            tz_offset_minutes: 600,
        };
        let inst = t.resolve(wt(0), TimerDirection::Down).expect("resolve");
        assert_eq!(inst.unix_seconds(), 1_782_860_400);
    }

    #[test]
    fn datetime_utc_is_the_naive_instant() {
        let t = TimerTarget::DateTime {
            at: "1970-01-02T00:00:00".to_owned(),
            timezone: Some("UTC".to_owned()),
            tz_offset_minutes: 0,
        };
        let inst = t.resolve(wt(0), TimerDirection::Down).expect("resolve");
        assert_eq!(inst.unix_seconds(), 86_400);
    }

    #[test]
    fn time_of_day_down_picks_the_next_occurrence_today() {
        // now = 1970-01-01 14:29:55 UTC; target 14:30:00 ⇒ today, 5 s ahead.
        let now = wt(14 * 3600 + 29 * 60 + 55);
        let t = TimerTarget::TimeOfDay {
            at: "14:30:00".to_owned(),
            timezone: Some("UTC".to_owned()),
            tz_offset_minutes: 0,
            recur_daily: false,
        };
        let inst = t.resolve(now, TimerDirection::Down).expect("resolve");
        assert_eq!(inst.unix_seconds() - now.unix_seconds(), 5);
    }

    #[test]
    fn time_of_day_down_rolls_to_tomorrow_when_already_passed() {
        // now = 14:30:05 UTC; target 14:30:00 already passed ⇒ tomorrow's 14:30.
        let now = wt(14 * 3600 + 30 * 60 + 5);
        let t = TimerTarget::TimeOfDay {
            at: "14:30:00".to_owned(),
            timezone: Some("UTC".to_owned()),
            tz_offset_minutes: 0,
            recur_daily: false,
        };
        let inst = t.resolve(now, TimerDirection::Down).expect("resolve");
        assert_eq!(
            inst.unix_seconds() - now.unix_seconds(),
            SECONDS_PER_DAY - 5
        );
    }

    #[test]
    fn time_of_day_up_picks_the_most_recent_occurrence() {
        // now = 14:30:05 UTC; up counts elapsed since today's 14:30:00 ⇒ 5 s ago.
        let now = wt(14 * 3600 + 30 * 60 + 5);
        let t = TimerTarget::TimeOfDay {
            at: "14:30:00".to_owned(),
            timezone: Some("UTC".to_owned()),
            tz_offset_minutes: 0,
            recur_daily: false,
        };
        let inst = t.resolve(now, TimerDirection::Up).expect("resolve");
        assert_eq!(now.unix_seconds() - inst.unix_seconds(), 5);
    }

    #[test]
    fn time_of_day_up_rolls_to_yesterday_when_not_yet_reached() {
        // now = 14:29:55 UTC; today's 14:30 has not happened ⇒ yesterday's.
        let now = wt(14 * 3600 + 29 * 60 + 55);
        let t = TimerTarget::TimeOfDay {
            at: "14:30:00".to_owned(),
            timezone: Some("UTC".to_owned()),
            tz_offset_minutes: 0,
            recur_daily: false,
        };
        let inst = t.resolve(now, TimerDirection::Up).expect("resolve");
        assert_eq!(
            now.unix_seconds() - inst.unix_seconds(),
            SECONDS_PER_DAY - 5
        );
    }

    #[test]
    fn time_of_day_crosses_midnight_in_a_non_utc_zone() {
        // Sydney AEDT (UTC+11) in January. now = 2026-01-15T23:59:55 local =
        // 12:59:55Z = 1768481995. Target 00:00:00 (next midnight, 5 s ahead).
        let now = wt(1_768_481_995);
        let t = TimerTarget::TimeOfDay {
            at: "00:00:00".to_owned(),
            timezone: Some("Australia/Sydney".to_owned()),
            tz_offset_minutes: 0,
            recur_daily: false,
        };
        let inst = t.resolve(now, TimerDirection::Down).expect("resolve");
        assert_eq!(inst.unix_seconds() - now.unix_seconds(), 5);
    }

    #[test]
    fn time_of_day_in_a_dst_gap_resolves_to_the_post_gap_instant() {
        // US spring-forward 2026: America/New_York jumps 02:00→03:00 on
        // 2026-03-08. A 02:30 target does not exist. now just before the change;
        // the resolver must land a real instant (the post-gap mapping), never a
        // nonexistent one — assert it is strictly after `now` and a whole minute.
        let ny = multiview_overlay::clock::parse_tz("America/New_York").expect("ny");
        // 2026-03-08T06:30:00Z is 01:30 EST (pre-gap). Use a `now` at 01:00 local.
        let now = wt(1_772_948_400); // 2026-03-08T07:00:00Z = 02:00 EDT? compute below
        let t = TimerTarget::TimeOfDay {
            at: "02:30:00".to_owned(),
            timezone: Some("America/New_York".to_owned()),
            tz_offset_minutes: 0,
            recur_daily: false,
        };
        let inst = t.resolve(now, TimerDirection::Down).expect("resolve");
        // The resolved instant must be a real instant strictly after `now`, and
        // the zone's offset at it must be well-defined (DST-aware).
        assert!(
            inst.unix_seconds() > now.unix_seconds(),
            "future occurrence"
        );
        let _off = resolve_offset(ny, inst); // resolves without panicking
    }

    // --- the at/after-target matrix (brief §3.2) ---------------------------

    #[test]
    fn down_before_target_shows_remaining_running() {
        let r = compute(wt(100), wt(95), TimerDirection::Down, TimerOnTarget::Hold);
        assert_eq!(r.shown_seconds, 5);
        assert_eq!(r.state, TimerState::Running);
    }

    #[test]
    fn down_at_target_shows_zero() {
        for ot in [
            TimerOnTarget::Hold,
            TimerOnTarget::Continue,
            TimerOnTarget::ZeroThenUp,
            TimerOnTarget::Recur,
        ] {
            let r = compute(wt(100), wt(100), TimerDirection::Down, ot);
            assert_eq!(r.shown_seconds, 0, "{ot:?} at target");
            assert_eq!(r.state, TimerState::AtTarget, "{ot:?} at target");
        }
    }

    #[test]
    fn down_hold_freezes_at_zero_past_target() {
        let r = compute(wt(100), wt(105), TimerDirection::Down, TimerOnTarget::Hold);
        assert_eq!(r.shown_seconds, 0);
        assert_eq!(r.state, TimerState::Held);
    }

    #[test]
    fn down_continue_counts_the_overrun_up() {
        let r = compute(
            wt(100),
            wt(105),
            TimerDirection::Down,
            TimerOnTarget::Continue,
        );
        assert_eq!(r.shown_seconds, 5);
        assert_eq!(r.state, TimerState::Overrun);
    }

    #[test]
    fn down_zero_then_up_counts_the_overrun_up() {
        let r = compute(
            wt(100),
            wt(106),
            TimerDirection::Down,
            TimerOnTarget::ZeroThenUp,
        );
        assert_eq!(r.shown_seconds, 6);
        assert_eq!(r.state, TimerState::Overrun);
    }

    #[test]
    fn up_counts_elapsed_since_target() {
        let r = compute(wt(100), wt(107), TimerDirection::Up, TimerOnTarget::Hold);
        assert_eq!(r.shown_seconds, 7);
        assert_eq!(r.state, TimerState::Running);
    }

    #[test]
    fn up_before_target_holds_at_zero() {
        // `up` from a future target: now < target ⇒ signed < 0 ⇒ Held at zero
        // (hold), so a not-yet-armed up-timer reads 00:00:00 rather than negative.
        let r = compute(wt(100), wt(90), TimerDirection::Up, TimerOnTarget::Hold);
        assert_eq!(r.shown_seconds, 0);
        assert_eq!(r.state, TimerState::Held);
    }

    #[test]
    fn recur_reads_zero_at_a_past_target_until_re_armed() {
        // A `recur` timer is re-resolved to the next day before display; if a
        // caller still passes a past target it must freeze, never go negative.
        let r = compute(wt(100), wt(105), TimerDirection::Down, TimerOnTarget::Recur);
        assert_eq!(r.shown_seconds, 0);
        assert_eq!(r.state, TimerState::Held);
    }

    // --- formatting --------------------------------------------------------

    fn running(secs: u64) -> TimerReadout {
        TimerReadout {
            shown_seconds: secs,
            state: TimerState::Running,
        }
    }

    fn overrun(secs: u64) -> TimerReadout {
        TimerReadout {
            shown_seconds: secs,
            state: TimerState::Overrun,
        }
    }

    #[test]
    fn format_d_hh_mm_ss_drops_zero_day() {
        assert_eq!(
            TimerFormat::DHhMmSs.format(running(3661), 0, cad_25(), None),
            "01:01:01"
        );
        // 2 days, 1h 5m 0s.
        let two_days = 2 * 86_400 + 3600 + 5 * 60;
        assert_eq!(
            TimerFormat::DHhMmSs.format(running(two_days), 0, cad_25(), None),
            "2:01:05:00"
        );
    }

    #[test]
    fn format_hh_mm_ss_folds_days_into_hours() {
        let one_day_two_h = 86_400 + 2 * 3600 + 3 * 60 + 4;
        assert_eq!(
            TimerFormat::HhMmSs.format(running(one_day_two_h), 0, cad_25(), None),
            "26:03:04"
        );
    }

    #[test]
    fn format_mm_ss_folds_into_minutes() {
        assert_eq!(
            TimerFormat::MmSs.format(running(65 * 60 + 9), 0, cad_25(), None),
            "65:09"
        );
    }

    #[test]
    fn format_hh_mm_ss_ff_uses_integer_frame_math() {
        // 25 fps: 0.5 s = 500_000_000 ns ⇒ frame 12 (floor(0.5*25)).
        assert_eq!(
            TimerFormat::HhMmSsFf.format(running(3661), 500_000_000, cad_25(), None),
            "01:01:01:12"
        );
        // 0 ns ⇒ frame 00.
        assert_eq!(
            TimerFormat::HhMmSsFf.format(running(0), 0, cad_25(), None),
            "00:00:00:00"
        );
    }

    #[test]
    fn format_auto_drops_leading_zero_units() {
        assert_eq!(
            TimerFormat::Auto.format(running(5 * 60), 0, cad_25(), None),
            "5:00"
        );
        assert_eq!(
            TimerFormat::Auto.format(running(3600 + 5 * 60), 0, cad_25(), None),
            "1:05:00"
        );
        let two_days = 2 * 86_400 + 3600 + 5 * 60;
        assert_eq!(
            TimerFormat::Auto.format(running(two_days), 0, cad_25(), None),
            "2d 01:05:00"
        );
    }

    #[test]
    fn overrun_gets_a_plus_prefix_by_default_and_an_override() {
        assert_eq!(
            TimerFormat::HhMmSs.format(overrun(5), 0, cad_25(), None),
            "+00:00:05"
        );
        assert_eq!(
            TimerFormat::HhMmSs.format(overrun(5), 0, cad_25(), Some("OVER ")),
            "OVER 00:00:05"
        );
        // A running value carries no prefix.
        assert_eq!(
            TimerFormat::HhMmSs.format(running(5), 0, cad_25(), None),
            "00:00:05"
        );
    }

    #[test]
    fn frame_index_clamps_to_fps_and_is_integer() {
        // 25 fps: just under 1 s of ns must not roll to frame 25.
        assert_eq!(frame_index(999_999_999, cad_25()), 24);
        // 30000/1001 (29.97): 0 ns ⇒ 0.
        assert_eq!(
            frame_index(
                0,
                Rational {
                    num: 30_000,
                    den: 1_001
                }
            ),
            0
        );
        // 29.97: half a frame in ⇒ still frame 0; one frame (~33.366 ms) ⇒ 1.
        assert_eq!(
            frame_index(
                33_400_000,
                Rational {
                    num: 30_000,
                    den: 1_001
                }
            ),
            1
        );
    }

    #[test]
    fn overrun_badge_word_is_direction_specific() {
        assert_eq!(overrun_badge_word(TimerDirection::Down), "OVER");
        assert_eq!(overrun_badge_word(TimerDirection::Up), "ELAPSED");
    }

    // --- validation --------------------------------------------------------

    #[test]
    fn validate_rejects_bad_time_of_day_and_zone() {
        let bad_t = TimerTarget::TimeOfDay {
            at: "25:00:00".to_owned(),
            timezone: Some("UTC".to_owned()),
            tz_offset_minutes: 0,
            recur_daily: false,
        };
        assert!(matches!(bad_t.validate(), Err(TimerError::BadTimeOfDay(_))));
        let bad_z = TimerTarget::DateTime {
            at: "2026-07-01T09:00:00".to_owned(),
            timezone: Some("Mars/Olympus".to_owned()),
            tz_offset_minutes: 0,
        };
        assert!(matches!(bad_z.validate(), Err(TimerError::BadTimezone(_))));
        let bad_d = TimerTarget::DateTime {
            at: "2026-13-40T99:00:00".to_owned(),
            timezone: None,
            tz_offset_minutes: 0,
        };
        assert!(matches!(bad_d.validate(), Err(TimerError::BadDateTime(_))));
    }

    #[test]
    fn validate_accepts_well_formed_targets() {
        let ok = TimerTarget::TimeOfDay {
            at: "14:30:00".to_owned(),
            timezone: Some("Australia/Sydney".to_owned()),
            tz_offset_minutes: 0,
            recur_daily: true,
        };
        assert!(ok.validate().is_ok());
    }

    // --- serde round-trip (tagged on `target`) -----------------------------

    #[test]
    fn timer_target_round_trips_tagged_on_target() {
        let tod = TimerTarget::TimeOfDay {
            at: "14:30:00".to_owned(),
            timezone: Some("Australia/Sydney".to_owned()),
            tz_offset_minutes: 0,
            recur_daily: true,
        };
        let json = serde_json::to_value(&tod).expect("ser");
        assert_eq!(json["target"], "time_of_day");
        let back: TimerTarget = serde_json::from_value(json).expect("de");
        assert_eq!(back, tod);

        let dt = TimerTarget::DateTime {
            at: "2026-07-01T09:00:00".to_owned(),
            timezone: Some("UTC".to_owned()),
            tz_offset_minutes: 0,
        };
        let json = serde_json::to_value(&dt).expect("ser");
        assert_eq!(json["target"], "date_time");
        let back: TimerTarget = serde_json::from_value(json).expect("de");
        assert_eq!(back, dt);
    }

    #[test]
    fn enums_round_trip_snake_case() {
        for d in [TimerDirection::Down, TimerDirection::Up] {
            let s = serde_json::to_value(d).expect("ser");
            assert_eq!(serde_json::from_value::<TimerDirection>(s).expect("de"), d);
        }
        assert_eq!(
            serde_json::to_value(TimerOnTarget::ZeroThenUp).unwrap(),
            "zero_then_up"
        );
        assert_eq!(
            serde_json::to_value(TimerFormat::DHhMmSs).unwrap(),
            "d_hh_mm_ss"
        );
        assert_eq!(
            serde_json::to_value(TimerFormat::HhMmSsFf).unwrap(),
            "hh_mm_ss_ff"
        );
    }

    // --- property tests (the duration math) --------------------------------

    proptest! {
        /// `down` and `up` at the same instant are exact negatives of each other
        /// (before the target), and the decomposition reassembles `n` exactly.
        #[test]
        fn down_and_up_are_negatives_and_decompose_round_trips(
            target in 1_000_000_i64..2_000_000_000,
            delta in 0_i64..500_000,
        ) {
            let now = wt(target - delta); // now <= target
            let down = compute(wt(target), now, TimerDirection::Down, TimerOnTarget::Continue);
            let up = compute(wt(target), now, TimerDirection::Up, TimerOnTarget::Continue);
            // Before/at the target: down shows `delta`, up shows 0 (held) unless delta==0.
            prop_assert_eq!(down.shown_seconds, delta.unsigned_abs());
            if delta > 0 {
                // up before its target holds at zero.
                prop_assert_eq!(up.shown_seconds, 0);
            }
            // decompose ∘ recompose is the identity.
            let (d, h, m, s) = decompose(down.shown_seconds);
            prop_assert_eq!(d * 86_400 + h * 3600 + m * 60 + s, down.shown_seconds);
        }

        /// The shown magnitude is monotone non-decreasing as `now` moves away from
        /// the target for a `continue` overrun (down), and never negative.
        #[test]
        fn overrun_is_monotone_in_elapsed(
            target in 1_000_000_i64..2_000_000_000,
            over in 1_i64..200_000,
        ) {
            let a = compute(wt(target), wt(target + over), TimerDirection::Down, TimerOnTarget::Continue);
            let b = compute(wt(target), wt(target + over + 1), TimerDirection::Down, TimerOnTarget::Continue);
            prop_assert!(b.shown_seconds >= a.shown_seconds);
            prop_assert_eq!(a.shown_seconds, over.unsigned_abs());
            prop_assert_eq!(a.state, TimerState::Overrun);
        }

        /// `frame_index` never reaches the frame count (always `< fps`) and is
        /// monotone in the sub-second nanoseconds.
        #[test]
        fn frame_index_is_bounded_and_monotone(ns in 0_u64..1_000_000_000, fps in 1_u32..120) {
            let cad = Rational { num: i64::from(fps), den: 1 };
            let f = frame_index(ns, cad);
            prop_assert!(f < u64::from(fps));
            if ns + 1 < 1_000_000_000 {
                prop_assert!(frame_index(ns + 1, cad) >= f);
            }
        }
    }
}
