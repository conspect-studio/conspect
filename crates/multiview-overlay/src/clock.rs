//! Analog + digital clock model with multi-timezone support and an NTP/PTP
//! reference source-select model.
//!
//! Clocks are driven by a [`WallTime`] (a wall-clock instant as whole Unix
//! seconds — the overlay model never carries float fps and never paces the
//! engine). A [`TimeZoneOffset`] shifts the instant into local wall-clock
//! fields; a [`ClockFace`] turns those fields into either a digital string or a
//! set of analog hand angles.
//!
//! The clock's timing reference (NTP per RFC 5905, or PTP per IEEE 1588 /
//! SMPTE ST 2059-2) is modelled by [`TimeRef`]: a [`RefSource`] plus a
//! [`RefStatus`]. Critically, the lock / ref-loss status is exposed as **text
//! and a glyph** ([`RefStatus::label`] / [`RefStatus::glyph`]) — never colour
//! alone — so an operator can read the reference health without relying on
//! colour (accessibility).
//!
//! All arithmetic is integer (seconds, minutes, degrees-times-1000 internally)
//! so the displayed fields are exact; hand angles are the only floating-point
//! results and are computed by a single division from integer inputs.

use serde::{Deserialize, Serialize};

/// The IANA timezone type ([`chrono_tz::Tz`]) re-exported so downstream crates
/// (the synthetic-source generator, the control plane) can name a resolved zone
/// without taking a direct `chrono-tz` dependency. Parse one with [`parse_tz`].
pub use chrono_tz::Tz;

/// Seconds in a day.
const SECONDS_PER_DAY: i64 = 86_400;

/// A wall-clock instant as whole Unix seconds (seconds since the Unix epoch,
/// UTC). The overlay carries time as integer seconds; sub-second precision is
/// not needed for a clock face and would only invite float drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WallTime {
    unix_seconds: i64,
}

impl WallTime {
    /// Construct from whole Unix seconds (UTC).
    #[must_use]
    pub const fn from_unix_seconds(unix_seconds: i64) -> Self {
        Self { unix_seconds }
    }

    /// The underlying Unix-seconds value.
    #[must_use]
    pub const fn unix_seconds(self) -> i64 {
        self.unix_seconds
    }

    /// Apply a [`TimeZoneOffset`], yielding the local wall-clock fields.
    #[must_use]
    pub fn with_offset(self, offset: TimeZoneOffset) -> LocalTime {
        let shifted = self.unix_seconds + offset.seconds();
        // Euclidean remainder so a negative offset wraps to the previous day.
        let secs_of_day = shifted.rem_euclid(SECONDS_PER_DAY);
        LocalTime { secs_of_day }
    }
}

/// A fixed UTC offset for a timezone, in whole minutes. Daylight-saving
/// transitions are the caller's concern (resolved upstream to a concrete
/// offset); this is a pure, fixed shift so the model stays deterministic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeZoneOffset {
    minutes: i32,
}

impl TimeZoneOffset {
    /// UTC (zero offset).
    pub const UTC: Self = Self { minutes: 0 };

    /// Construct from a signed whole-minute offset from UTC (e.g. `600` for
    /// +10:00, `-330` for −05:30).
    #[must_use]
    pub const fn from_minutes(minutes: i32) -> Self {
        Self { minutes }
    }

    /// The offset in whole minutes.
    #[must_use]
    pub const fn minutes(self) -> i32 {
        self.minutes
    }

    /// The offset in whole seconds.
    #[must_use]
    pub fn seconds(self) -> i64 {
        i64::from(self.minutes) * 60
    }

    /// The human `UTC±HH:MM` badge for this offset (the `show_offset` text drawn
    /// on a clock face): e.g. `UTC+11:00`, `UTC-04:00`, `UTC+00:00`.
    ///
    /// The sign is on the hours/minutes block; the magnitude is split into whole
    /// hours and the remaining whole minutes (integer arithmetic, never float).
    #[must_use]
    pub fn utc_badge(self) -> String {
        let sign = if self.minutes < 0 { '-' } else { '+' };
        // `i32::MIN.unsigned_abs()` is well-defined; real offsets are small.
        let mag = self.minutes.unsigned_abs();
        let hours = mag / 60;
        let mins = mag % 60;
        format!("UTC{sign}{hours:02}:{mins:02}")
    }
}

/// Parse an IANA timezone id (e.g. `Australia/Sydney`, `America/New_York`,
/// `UTC`) to a concrete [`chrono_tz::Tz`].
///
/// Returns [`None`] for an unknown or malformed id (IANA ids are case-sensitive)
/// — callers turn that into a typed configuration error; this never panics. The
/// tz database is bundled at build time by `chrono-tz` (no runtime I/O).
#[must_use]
pub fn parse_tz(name: &str) -> Option<chrono_tz::Tz> {
    name.parse::<chrono_tz::Tz>().ok()
}

/// Resolve an IANA [`chrono_tz::Tz`] to the concrete [`TimeZoneOffset`] in effect
/// at the displayed instant `at` — **DST-correct**.
///
/// This is the upstream resolver the model's [`TimeZoneOffset`] doc anticipates:
/// it picks the right offset for the instant, so a `Australia/Sydney` clock reads
/// `UTC+11:00` in the austral summer and `UTC+10:00` in winter automatically, and
/// the `show_offset` badge reflects the resolved value. Pure (a function of
/// `(Tz, instant)`) and unit-testable with injected instants; no system clock.
///
/// Offsets are whole minutes on the modern tz database; a (historical)
/// sub-minute offset is truncated toward zero to whole minutes, matching the
/// model's whole-minute [`TimeZoneOffset`].
#[must_use]
pub fn resolve_offset(tz: chrono_tz::Tz, at: WallTime) -> TimeZoneOffset {
    use chrono::{Offset, TimeZone};
    // Map Unix seconds → a UTC datetime, then ask the zone for the offset at that
    // UTC instant (DST-aware). `from_timestamp` is `None` only for out-of-range
    // values far outside any real clock instant; fall back to UTC then.
    let Some(utc) = chrono::DateTime::from_timestamp(at.unix_seconds(), 0) else {
        return TimeZoneOffset::UTC;
    };
    let offset_seconds = tz
        .offset_from_utc_datetime(&utc.naive_utc())
        .fix()
        .local_minus_utc();
    // Whole minutes (truncating any historical sub-minute remainder toward zero).
    TimeZoneOffset::from_minutes(offset_seconds / 60)
}

/// Local wall-clock fields for a single day: seconds since local midnight,
/// decomposed into hours / minutes / seconds on demand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    /// Seconds since local midnight, in `0..86_400`.
    secs_of_day: i64,
}

impl LocalTime {
    /// Hour in `0..24`.
    ///
    /// `secs_of_day` is in `0..86_400` (a [`WallTime::with_offset`] invariant),
    /// so `secs_of_day / 3600` is in `0..24` and the `u8` conversion is exact;
    /// the saturating fallback can never be reached.
    #[must_use]
    pub fn hour(self) -> u8 {
        u8::try_from(self.secs_of_day / 3600).unwrap_or(0)
    }

    /// Minute in `0..60`.
    #[must_use]
    pub fn minute(self) -> u8 {
        u8::try_from((self.secs_of_day % 3600) / 60).unwrap_or(0)
    }

    /// Second in `0..60`.
    #[must_use]
    pub fn second(self) -> u8 {
        u8::try_from(self.secs_of_day % 60).unwrap_or(0)
    }
}

/// A clock presentation: digital (12- or 24-hour) or analog hands.
///
/// Serialised tagged on `face` (never `untagged`, per repo serde policy).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "face", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ClockFace {
    /// Digital `HH:MM:SS` time-of-day.
    Digital {
        /// `true` for 12-hour with an AM/PM meridiem, `false` for 24-hour.
        twelve_hour: bool,
    },
    /// Analog face (hour / minute / second hands).
    Analog,
}

impl ClockFace {
    /// A 24-hour digital face.
    #[must_use]
    pub const fn digital_24h() -> Self {
        Self::Digital { twelve_hour: false }
    }

    /// A 12-hour digital face with an AM/PM meridiem.
    #[must_use]
    pub const fn digital_12h() -> Self {
        Self::Digital { twelve_hour: true }
    }

    /// An analog face.
    #[must_use]
    pub const fn analog() -> Self {
        Self::Analog
    }

    /// Whether this is a digital face.
    #[must_use]
    pub const fn is_digital(self) -> bool {
        matches!(self, Self::Digital { .. })
    }

    /// Format `local` as a digital string for this face.
    ///
    /// For an [`ClockFace::Analog`] face this still returns the digital string
    /// (useful as an accompanying readout); callers wanting hands use
    /// [`AnalogHands::from`].
    #[must_use]
    pub fn format(self, local: LocalTime) -> String {
        match self {
            Self::Digital { twelve_hour: false } | Self::Analog => {
                format!(
                    "{:02}:{:02}:{:02}",
                    local.hour(),
                    local.minute(),
                    local.second()
                )
            }
            Self::Digital { twelve_hour: true } => {
                let (h12, meridiem) = to_12_hour(local.hour());
                format!(
                    "{:02}:{:02}:{:02} {}",
                    h12,
                    local.minute(),
                    local.second(),
                    meridiem
                )
            }
        }
    }
}

/// The face a clock **source** renders: an analogue dial, a digital readout, or
/// **both** (`Dual`, an analogue face with a digital readout beneath it).
///
/// This is the 3-state widening of the original `analog`/`digital` boolean
/// (ADR-0047): `Dual` is a placement of the two existing faces, not a new
/// renderer. Serialised tagged on the variant name (`snake_case`), never
/// `untagged` (repo serde policy).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ClockFaceMode {
    /// Analogue dial only (hour / minute / second hands on a ticked bezel).
    #[default]
    Analog,
    /// Digital `HH:MM:SS` readout only.
    Digital,
    /// Both: an analogue face with a digital readout beneath it.
    Dual,
}

impl ClockFaceMode {
    /// Whether this mode draws the analogue dial (`Analog` or `Dual`).
    #[must_use]
    pub const fn has_analog(self) -> bool {
        matches!(self, Self::Analog | Self::Dual)
    }

    /// Whether this mode draws the digital readout (`Digital` or `Dual`).
    #[must_use]
    pub const fn has_digital(self) -> bool {
        matches!(self, Self::Digital | Self::Dual)
    }
}

/// Convert a 0..24 hour into a (12-hour clock value, meridiem) pair.
const fn to_12_hour(hour24: u8) -> (u8, &'static str) {
    let meridiem = if hour24 < 12 { "AM" } else { "PM" };
    let h = hour24 % 12;
    let h12 = if h == 0 { 12 } else { h };
    (h12, meridiem)
}

/// Analog hand angles in degrees clockwise from 12 o'clock (straight up = 0°).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AnalogHands {
    /// Hour-hand angle in degrees (0..360), advancing smoothly with minutes.
    pub hour_deg: f32,
    /// Minute-hand angle in degrees (0..360), advancing smoothly with seconds.
    pub minute_deg: f32,
    /// Second-hand angle in degrees (0..360).
    pub second_deg: f32,
}

impl AnalogHands {
    /// Hand angles for `local` on a **12-hour** dial (`twelve_hour == true`) or a
    /// **24-hour** dial (`false`). The minute and second hands are identical
    /// between the two; only the hour hand's rate differs:
    /// * 12-hour: `30°/hour` (two revolutions/day; `12`/`24` o'clock straight up),
    /// * 24-hour: `15°/hour` (one revolution/day; `00:00` straight up, `12:00`
    ///   straight down).
    #[must_use]
    pub fn for_dial(local: LocalTime, twelve_hour: bool) -> Self {
        let m = f32::from(local.minute());
        let s = f32::from(local.second());
        // 360 / 60 = 6 deg/minute (+0.1/sec); 360 / 60 = 6 deg/second.
        let hour_deg = if twelve_hour {
            // 360 / 12 = 30 deg/hour, +0.5/min (30/60), +0.5/3600/sec.
            f32::from(local.hour() % 12) * 30.0 + m * 0.5 + s * (0.5 / 60.0)
        } else {
            // 360 / 24 = 15 deg/hour, +0.25/min (15/60), +0.25/3600/sec.
            f32::from(local.hour()) * 15.0 + m * 0.25 + s * (0.25 / 60.0)
        };
        Self {
            hour_deg,
            minute_deg: m * 6.0 + s * 0.1,
            second_deg: s * 6.0,
        }
    }
}

impl From<LocalTime> for AnalogHands {
    /// A 12-hour dial (the conventional default). Use [`AnalogHands::for_dial`]
    /// to select a 24-hour dial.
    fn from(local: LocalTime) -> Self {
        Self::for_dial(local, true)
    }
}

/// A timing-reference source for a disciplined clock.
///
/// Serialised tagged (the variant name); never `untagged`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RefSource {
    /// Network Time Protocol (RFC 5905).
    Ntp,
    /// Precision Time Protocol (IEEE 1588 / SMPTE ST 2059-2).
    Ptp,
    /// Free-running from the local system clock (no external discipline).
    System,
}

impl RefSource {
    /// A short upper-case label (`NTP` / `PTP` / `SYS`).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ntp => "NTP",
            Self::Ptp => "PTP",
            Self::System => "SYS",
        }
    }
}

/// The lock / discipline status of a timing reference.
///
/// **Accessibility:** the status is conveyed as text ([`RefStatus::label`]) and
/// a distinct glyph ([`RefStatus::glyph`]) so an operator never has to rely on
/// colour to tell a locked reference from a lost one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RefStatus {
    /// Disciplined and locked to the reference.
    Locked,
    /// Reference lost recently; coasting on the last good discipline.
    Holdover,
    /// Reference lost and out of holdover; the clock is no longer disciplined.
    RefLoss,
    /// No reference configured; free-running on the local oscillator.
    #[default]
    Freerun,
}

impl RefStatus {
    /// A short, lower-case textual label for the status (accessibility).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Locked => "locked",
            Self::Holdover => "holdover",
            Self::RefLoss => "ref-loss",
            Self::Freerun => "freerun",
        }
    }

    /// A distinct glyph for the status so it reads without colour
    /// (accessibility): a filled lock, a coasting dot, a broken link, and a free
    /// circle, respectively.
    #[must_use]
    pub const fn glyph(self) -> char {
        match self {
            Self::Locked => '\u{1F512}',  // 🔒 closed lock
            Self::Holdover => '\u{25CB}', // ○ coasting
            Self::RefLoss => '\u{26A0}',  // ⚠ warning
            Self::Freerun => '\u{223F}',  // ∿ free-running wave
        }
    }

    /// Whether the clock is currently disciplined (locked or holding over) — i.e.
    /// the displayed time is trustworthy to reference accuracy.
    #[must_use]
    pub const fn is_disciplined(self) -> bool {
        matches!(self, Self::Locked | Self::Holdover)
    }
}

/// A clock's timing reference: which [`RefSource`] disciplines it and its
/// current [`RefStatus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeRef {
    /// The reference source.
    pub source: RefSource,
    /// The current lock/discipline status.
    pub status: RefStatus,
}

impl TimeRef {
    /// Construct a reference descriptor.
    #[must_use]
    pub const fn new(source: RefSource, status: RefStatus) -> Self {
        Self { source, status }
    }

    /// A combined `"<SOURCE> <status>"` badge string (e.g. `"PTP locked"`),
    /// suitable for the a11y text badge alongside [`RefStatus::glyph`].
    #[must_use]
    pub fn status_text(&self) -> String {
        format!("{} {}", self.source.label(), self.status.label())
    }
}

/// A full clock overlay model: a [`ClockFace`], a [`TimeZoneOffset`], and a
/// [`TimeRef`]. Pure — render it against a [`WallTime`] supplied by the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClockModel {
    /// The presentation (digital/analog).
    pub face: ClockFace,
    /// The timezone offset applied before display.
    pub zone: TimeZoneOffset,
    /// The timing-reference descriptor (status exposed for the a11y badge).
    pub time_ref: TimeRef,
}

impl ClockModel {
    /// Construct a clock model.
    #[must_use]
    pub const fn new(face: ClockFace, zone: TimeZoneOffset, time_ref: TimeRef) -> Self {
        Self {
            face,
            zone,
            time_ref,
        }
    }

    /// Render the digital string for `wall`, or [`None`] if this is an analog
    /// face (use [`ClockModel::render_analog`] then).
    #[must_use]
    pub fn render_digital(&self, wall: WallTime) -> Option<String> {
        if self.face.is_digital() {
            Some(self.face.format(wall.with_offset(self.zone)))
        } else {
            None
        }
    }

    /// Render the analog hand angles for `wall`, or [`None`] if this is a digital
    /// face (use [`ClockModel::render_digital`] then).
    #[must_use]
    pub fn render_analog(&self, wall: WallTime) -> Option<AnalogHands> {
        if self.face.is_digital() {
            None
        } else {
            Some(AnalogHands::from(wall.with_offset(self.zone)))
        }
    }
}
