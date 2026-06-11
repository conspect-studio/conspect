//! Analog + digital clock model: multiple styles, multi-timezone, and an
//! NTP/PTP source-select model that exposes lock / ref-loss status as text+glyph
//! (never colour-only). Formatting and hand angles are checked against known
//! answers.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_overlay::clock::{
    parse_tz, resolve_offset, AnalogHands, ClockFace, ClockFaceMode, ClockModel, RefSource,
    RefStatus, TimeRef, TimeZoneOffset, WallTime,
};

/// 2026-01-15 00:00:00 UTC as whole Unix seconds — austral summer (Sydney DST on).
const JAN_2026_UTC: i64 = 1_768_435_200;
/// 2026-07-15 00:00:00 UTC as whole Unix seconds — austral winter (Sydney DST off).
const JUL_2026_UTC: i64 = 1_784_073_600;
/// 2026-03-08 06:30:00 UTC — just BEFORE the US spring-forward (NY is EST, −05:00).
const NY_BEFORE_SPRING_UTC: i64 = 1_772_951_400;
/// 2026-03-08 07:30:00 UTC — just AFTER the US spring-forward (NY is EDT, −04:00).
const NY_AFTER_SPRING_UTC: i64 = 1_772_955_000;

/// Assert two f32 angles (degrees) are equal within tolerance.
fn assert_deg(actual: f32, expected: f32) {
    assert!(
        (actual - expected).abs() < 1e-3,
        "expected {expected} deg, got {actual}"
    );
}

#[test]
fn digital_formats_hms_24h() {
    // 13:05:09 into a UTC day.
    let wall = WallTime::from_unix_seconds(13 * 3600 + 5 * 60 + 9);
    let face = ClockFace::digital_24h();
    let local = wall.with_offset(TimeZoneOffset::UTC);
    assert_eq!(face.format(local), "13:05:09");
}

#[test]
fn digital_formats_12h_with_meridiem() {
    let wall = WallTime::from_unix_seconds(13 * 3600 + 5 * 60 + 9);
    let face = ClockFace::digital_12h();
    let local = wall.with_offset(TimeZoneOffset::UTC);
    assert_eq!(face.format(local), "01:05:09 PM");
}

#[test]
fn digital_12h_midnight_is_twelve_am() {
    let wall = WallTime::from_unix_seconds(0);
    let local = wall.with_offset(TimeZoneOffset::UTC);
    assert_eq!(ClockFace::digital_12h().format(local), "12:00:00 AM");
}

#[test]
fn digital_12h_noon_is_twelve_pm() {
    let wall = WallTime::from_unix_seconds(12 * 3600);
    let local = wall.with_offset(TimeZoneOffset::UTC);
    assert_eq!(ClockFace::digital_12h().format(local), "12:00:00 PM");
}

#[test]
fn timezone_offset_shifts_displayed_time() {
    // 00:00:00 UTC seen from +10:00 (e.g. AEST) is 10:00:00.
    let wall = WallTime::from_unix_seconds(0);
    let aest = TimeZoneOffset::from_minutes(10 * 60);
    let local = wall.with_offset(aest);
    assert_eq!(ClockFace::digital_24h().format(local), "10:00:00");
}

#[test]
fn negative_timezone_offset_wraps_to_previous_day() {
    // 00:30:00 UTC seen from -05:00 is 19:30:00 (previous day wall clock).
    let wall = WallTime::from_unix_seconds(30 * 60);
    let est = TimeZoneOffset::from_minutes(-5 * 60);
    let local = wall.with_offset(est);
    assert_eq!(ClockFace::digital_24h().format(local), "19:30:00");
}

#[test]
fn analog_hands_at_three_oclock() {
    // 03:00:00 — hour hand at 90 deg, minute and second at 0.
    let wall = WallTime::from_unix_seconds(3 * 3600);
    let local = wall.with_offset(TimeZoneOffset::UTC);
    let hands = AnalogHands::from(local);
    assert_deg(hands.hour_deg, 90.0);
    assert_deg(hands.minute_deg, 0.0);
    assert_deg(hands.second_deg, 0.0);
}

#[test]
fn analog_hour_hand_advances_with_minutes() {
    // 01:30:00 — hour hand halfway between 1 and 2 = 45 deg; minute at 180.
    let wall = WallTime::from_unix_seconds(3600 + 30 * 60);
    let local = wall.with_offset(TimeZoneOffset::UTC);
    let hands = AnalogHands::from(local);
    assert_deg(hands.hour_deg, 45.0);
    assert_deg(hands.minute_deg, 180.0);
    assert_deg(hands.second_deg, 0.0);
}

#[test]
fn analog_second_hand_at_quarter() {
    // ...:...:15 — second hand at 90 deg.
    let wall = WallTime::from_unix_seconds(15);
    let local = wall.with_offset(TimeZoneOffset::UTC);
    let hands = AnalogHands::from(local);
    assert_deg(hands.second_deg, 90.0);
}

#[test]
fn ref_status_is_text_and_glyph_not_colour() {
    // a11y: lock/ref-loss status must be conveyed as text + glyph.
    assert_eq!(RefStatus::Locked.label(), "locked");
    assert_eq!(RefStatus::Holdover.label(), "holdover");
    assert_eq!(RefStatus::RefLoss.label(), "ref-loss");
    assert_eq!(RefStatus::Freerun.label(), "freerun");
    // Distinct glyphs so the status reads without colour.
    let glyphs = [
        RefStatus::Locked.glyph(),
        RefStatus::Holdover.glyph(),
        RefStatus::RefLoss.glyph(),
        RefStatus::Freerun.glyph(),
    ];
    for (i, a) in glyphs.iter().enumerate() {
        for b in glyphs.iter().skip(i + 1) {
            assert_ne!(a, b, "ref-status glyphs must be distinct");
        }
    }
}

#[test]
fn ref_loss_is_disciplined_only_when_locked_or_holdover() {
    assert!(RefStatus::Locked.is_disciplined());
    assert!(RefStatus::Holdover.is_disciplined());
    assert!(!RefStatus::RefLoss.is_disciplined());
    assert!(!RefStatus::Freerun.is_disciplined());
}

#[test]
fn source_select_carries_kind_and_status() {
    let r = TimeRef::new(RefSource::Ptp, RefStatus::Locked);
    assert_eq!(r.source, RefSource::Ptp);
    assert_eq!(r.status, RefStatus::Locked);
    // Display string conveys both source and status as text.
    assert_eq!(r.status_text(), "PTP locked");
    let ntp = TimeRef::new(RefSource::Ntp, RefStatus::RefLoss);
    assert_eq!(ntp.status_text(), "NTP ref-loss");
}

#[test]
fn clock_model_carries_face_zone_and_ref() {
    let model = ClockModel::new(
        ClockFace::digital_24h(),
        TimeZoneOffset::from_minutes(0),
        TimeRef::new(RefSource::Ntp, RefStatus::Locked),
    );
    let wall = WallTime::from_unix_seconds(3661); // 01:01:01
    assert_eq!(model.render_digital(wall), Some("01:01:01".to_owned()));
    assert!(model.render_analog(wall).is_none());
    // The ref status is exposed for the a11y badge.
    assert_eq!(model.time_ref.status_text(), "NTP locked");
}

#[test]
fn clock_model_round_trips_through_json() {
    let model = ClockModel::new(
        ClockFace::analog(),
        TimeZoneOffset::from_minutes(-330),
        TimeRef::new(RefSource::Ptp, RefStatus::Holdover),
    );
    let json = serde_json::to_string(&model).unwrap();
    let back: ClockModel = serde_json::from_str(&json).unwrap();
    assert_eq!(model, back);
}

#[test]
fn for_dial_hour_hand_rate_differs_between_12h_and_24h() {
    // 18:00:00 UTC. On a 12-hour dial the hour hand is at 6 o'clock (180°); on a
    // 24-hour dial (00:00 straight up, 15°/hour) it is at 18*15 = 270° (9 o'clock).
    let local = WallTime::from_unix_seconds(18 * 3600).with_offset(TimeZoneOffset::UTC);
    let twelve = AnalogHands::for_dial(local, true);
    let twenty_four = AnalogHands::for_dial(local, false);
    assert_deg(twelve.hour_deg, 180.0);
    assert_deg(twenty_four.hour_deg, 270.0);
    // Minute + second hands are identical between the two dials.
    assert_deg(twelve.minute_deg, twenty_four.minute_deg);
    assert_deg(twelve.second_deg, twenty_four.second_deg);
    // The default `From<LocalTime>` is the 12-hour dial.
    assert_deg(AnalogHands::from(local).hour_deg, 180.0);
}

#[test]
fn for_dial_24h_advances_the_hour_hand_half_as_fast() {
    // 06:00 vs 18:00 on a 24-hour dial are 12 hours apart -> 180° apart (90° vs 270°).
    let six = WallTime::from_unix_seconds(6 * 3600).with_offset(TimeZoneOffset::UTC);
    let eighteen = WallTime::from_unix_seconds(18 * 3600).with_offset(TimeZoneOffset::UTC);
    assert_deg(AnalogHands::for_dial(six, false).hour_deg, 90.0);
    assert_deg(AnalogHands::for_dial(eighteen, false).hour_deg, 270.0);
}

#[test]
fn parse_tz_accepts_iana_and_rejects_garbage() {
    assert!(parse_tz("Australia/Sydney").is_some());
    assert!(parse_tz("America/New_York").is_some());
    assert!(parse_tz("UTC").is_some());
    // Unknown / malformed ids resolve to None (the config layer turns this into a
    // typed error; the resolver never panics).
    assert!(parse_tz("Mars/Olympus_Mons").is_none());
    assert!(parse_tz("").is_none());
    assert!(parse_tz("australia/sydney").is_none()); // case-sensitive IANA ids
}

#[test]
fn resolve_offset_is_dst_correct_for_sydney() {
    let syd = parse_tz("Australia/Sydney").expect("Australia/Sydney is a known IANA zone");
    // Austral summer (DST on): +11:00 = 660 minutes.
    let jan = resolve_offset(syd, WallTime::from_unix_seconds(JAN_2026_UTC));
    assert_eq!(
        jan.minutes(),
        11 * 60,
        "Sydney in January is UTC+11:00 (DST)"
    );
    // Austral winter (DST off): +10:00 = 600 minutes.
    let jul = resolve_offset(syd, WallTime::from_unix_seconds(JUL_2026_UTC));
    assert_eq!(
        jul.minutes(),
        10 * 60,
        "Sydney in July is UTC+10:00 (standard)"
    );
    // The displayed wall-clock time reflects the resolved offset: 2026-01-15
    // 00:00 UTC is 11:00 local in Sydney (DST), but 2026-07-15 00:00 UTC is 10:00.
    assert_eq!(
        ClockFace::digital_24h().format(WallTime::from_unix_seconds(JAN_2026_UTC).with_offset(jan)),
        "11:00:00"
    );
    assert_eq!(
        ClockFace::digital_24h().format(WallTime::from_unix_seconds(JUL_2026_UTC).with_offset(jul)),
        "10:00:00"
    );
}

#[test]
fn resolve_offset_tracks_the_us_spring_forward_boundary() {
    let ny = parse_tz("America/New_York").expect("America/New_York is a known IANA zone");
    // 30 minutes before the spring-forward instant New York is EST (−05:00).
    let before = resolve_offset(ny, WallTime::from_unix_seconds(NY_BEFORE_SPRING_UTC));
    assert_eq!(
        before.minutes(),
        -5 * 60,
        "New York is UTC−05:00 (EST) before spring-forward"
    );
    // 30 minutes after, it is EDT (−04:00) — the clock follows the DST jump.
    let after = resolve_offset(ny, WallTime::from_unix_seconds(NY_AFTER_SPRING_UTC));
    assert_eq!(
        after.minutes(),
        -4 * 60,
        "New York is UTC−04:00 (EDT) after spring-forward"
    );
}

#[test]
fn resolve_offset_utc_is_zero_year_round() {
    let utc = parse_tz("UTC").expect("UTC is a known IANA zone");
    assert_eq!(
        resolve_offset(utc, WallTime::from_unix_seconds(JAN_2026_UTC)).minutes(),
        0
    );
    assert_eq!(
        resolve_offset(utc, WallTime::from_unix_seconds(JUL_2026_UTC)).minutes(),
        0
    );
}

#[test]
fn clock_face_mode_is_three_state_and_round_trips() {
    // The face is now a 3-state mode: analog / digital / dual.
    let modes = [
        ClockFaceMode::Analog,
        ClockFaceMode::Digital,
        ClockFaceMode::Dual,
    ];
    for m in modes {
        let json = serde_json::to_string(&m).unwrap();
        let back: ClockFaceMode = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }
    // Dual draws both faces.
    assert!(ClockFaceMode::Dual.has_analog());
    assert!(ClockFaceMode::Dual.has_digital());
    assert!(ClockFaceMode::Analog.has_analog());
    assert!(!ClockFaceMode::Analog.has_digital());
    assert!(ClockFaceMode::Digital.has_digital());
    assert!(!ClockFaceMode::Digital.has_analog());
}

#[test]
fn offset_badge_text_renders_signed_hh_mm() {
    // The "UTC±HH:MM" badge text the `show_offset` flag draws.
    assert_eq!(
        TimeZoneOffset::from_minutes(11 * 60).utc_badge(),
        "UTC+11:00"
    );
    assert_eq!(
        TimeZoneOffset::from_minutes(10 * 60).utc_badge(),
        "UTC+10:00"
    );
    assert_eq!(
        TimeZoneOffset::from_minutes(-4 * 60).utc_badge(),
        "UTC-04:00"
    );
    assert_eq!(TimeZoneOffset::from_minutes(-330).utc_badge(), "UTC-05:30");
    assert_eq!(TimeZoneOffset::UTC.utc_badge(), "UTC+00:00");
}
