//! `bars` / `solid` / `clock` are first-class synthetic `SourceKind`s (ADR-0027),
//! modelled and validated like any decoded feed. `test` is retained as a
//! back-compat alias for `bars`.
//!
//! Every assertion is against the real public config API (parse → validate →
//! re-serialize) — no tautologies.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::{ClockFaceConfig, MultiviewConfig, SourceKind};

/// Wrap one source's `kind` fields in a minimal, structurally valid 1x1 grid
/// document so we exercise the real parse + validate path, not a bare enum.
fn config_with_source(source_kind_fields: &str) -> String {
    format!(
        r##"schema_version = 1
[canvas]
width = 320
height = 240
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]
[[sources]]
id = "in_a"
{source_kind_fields}
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[outputs]]
kind = "hls"
path = "/tmp/x.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##
    )
}

#[test]
fn test_kind_is_a_back_compat_alias_for_bars() {
    let doc = config_with_source("kind = \"test\"");
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    cfg.validate().expect("validates");
    assert!(
        matches!(cfg.sources[0].kind, SourceKind::Bars),
        "`kind = \"test\"` must deserialize to Bars"
    );
    // Canonical re-serialization emits the new name, not the alias.
    let toml = cfg.to_toml().expect("to_toml");
    assert!(
        toml.contains("kind = \"bars\""),
        "canonical kind is `bars`, got:\n{toml}"
    );
    assert!(
        !toml.contains("kind = \"test\""),
        "the `test` alias must not round-trip back out"
    );
}

#[test]
fn bars_kind_parses_and_validates() {
    let cfg =
        MultiviewConfig::load_from_toml(&config_with_source("kind = \"bars\"")).expect("parse");
    cfg.validate().expect("validates");
    assert!(matches!(cfg.sources[0].kind, SourceKind::Bars));
}

#[test]
fn solid_source_round_trips_with_its_colour() {
    let doc = config_with_source("kind = \"solid\"\ncolor = \"#22aa44\"");
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    cfg.validate().expect("validates");
    match &cfg.sources[0].kind {
        SourceKind::Solid { color } => assert_eq!(color, "#22aa44"),
        other => panic!("expected Solid, got {other:?}"),
    }
    let toml = cfg.to_toml().expect("to_toml");
    assert!(toml.contains("kind = \"solid\""));
    assert!(toml.contains("#22aa44"));
}

#[test]
fn solid_rejects_a_non_hex_colour() {
    let doc = config_with_source("kind = \"solid\"\ncolor = \"not-a-color\"");
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    let err = cfg
        .validate()
        .expect_err("a non-hex solid colour must fail validation");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("color") || msg.contains("colour") || msg.contains("hex"),
        "the error must name the colour: {msg}"
    );
}

#[test]
fn clock_source_defaults_to_analog_utc_and_round_trips() {
    let doc = config_with_source("kind = \"clock\"");
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    cfg.validate().expect("validates");
    match &cfg.sources[0].kind {
        SourceKind::Clock {
            face,
            twelve_hour,
            tz_offset_minutes,
            ..
        } => {
            assert_eq!(*face, ClockFaceConfig::Analog, "clock defaults to analog");
            assert!(!*twelve_hour, "12-hour defaults off");
            assert_eq!(*tz_offset_minutes, 0, "tz defaults to UTC");
        }
        other => panic!("expected Clock, got {other:?}"),
    }
}

#[test]
fn digital_clock_with_tz_round_trips() {
    let doc = config_with_source(
        "kind = \"clock\"\nface = \"digital\"\ntwelve_hour = true\ntz_offset_minutes = 600",
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    cfg.validate().expect("validates");
    match &cfg.sources[0].kind {
        SourceKind::Clock {
            face,
            twelve_hour,
            tz_offset_minutes,
            ..
        } => {
            assert_eq!(*face, ClockFaceConfig::Digital);
            assert!(*twelve_hour);
            assert_eq!(*tz_offset_minutes, 600);
        }
        other => panic!("expected Clock, got {other:?}"),
    }
    let toml = cfg.to_toml().expect("to_toml");
    assert!(toml.contains("kind = \"clock\""));
    assert!(toml.contains("face = \"digital\""));
}

#[test]
fn clock_rejects_an_out_of_range_tz_offset() {
    // Real UTC offsets span -12:00..=+14:00 (-720..=840 min); 5000 is absurd.
    let doc = config_with_source("kind = \"clock\"\ntz_offset_minutes = 5000");
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    let err = cfg
        .validate()
        .expect_err("an out-of-range tz offset must fail validation");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("tz") || msg.contains("offset") || msg.contains("timezone"),
        "the error must name the tz offset: {msg}"
    );
}

#[test]
fn dual_clock_with_iana_timezone_and_metadata_round_trips() {
    // The operator-requested world-clock tile: dual face, IANA zone, label, and a
    // visible UTC-offset badge.
    let doc = config_with_source(
        "kind = \"clock\"\nface = \"dual\"\ntimezone = \"Australia/Sydney\"\nlabel = \"Sydney\"\n\
         show_offset = true\nshow_reference = true\nnumerals = true",
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    cfg.validate().expect("validates");
    match &cfg.sources[0].kind {
        SourceKind::Clock {
            face,
            timezone,
            label,
            show_offset,
            show_reference,
            numerals,
            ..
        } => {
            assert_eq!(*face, ClockFaceConfig::Dual, "dual face parsed");
            assert_eq!(timezone.as_deref(), Some("Australia/Sydney"));
            assert_eq!(label.as_deref(), Some("Sydney"));
            assert!(*show_offset);
            assert!(*show_reference);
            assert!(*numerals);
        }
        other => panic!("expected Clock, got {other:?}"),
    }
    let toml = cfg.to_toml().expect("to_toml");
    assert!(toml.contains("face = \"dual\""), "dual round-trips: {toml}");
    assert!(
        toml.contains("timezone = \"Australia/Sydney\""),
        "timezone round-trips: {toml}"
    );
    assert!(toml.contains("label = \"Sydney\""));
    assert!(toml.contains("show_offset = true"));
    // Round-trip through JSON too (robust across both serde formats).
    let json = cfg.to_json().expect("to_json");
    let back = MultiviewConfig::load_from_json(&json).expect("json round-trip");
    assert_eq!(back.sources[0].kind, cfg.sources[0].kind);
}

#[test]
fn clock_metadata_defaults_are_absent_and_skip_serializing() {
    // A plain clock carries no metadata: the new optionals default to None/false
    // and must NOT appear in the canonical serialization (skip_serializing_if).
    let doc = config_with_source("kind = \"clock\"");
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    match &cfg.sources[0].kind {
        SourceKind::Clock {
            timezone,
            label,
            show_offset,
            show_reference,
            numerals,
            ..
        } => {
            assert!(timezone.is_none(), "timezone defaults absent");
            assert!(label.is_none(), "label defaults absent");
            assert!(!*show_offset);
            assert!(!*show_reference);
            assert!(!*numerals);
        }
        other => panic!("expected Clock, got {other:?}"),
    }
    let toml = cfg.to_toml().expect("to_toml");
    assert!(
        !toml.contains("timezone"),
        "absent timezone must not serialize: {toml}"
    );
    assert!(
        !toml.contains("label"),
        "absent label must not serialize: {toml}"
    );
}

#[test]
fn clock_rejects_an_unknown_iana_timezone() {
    let doc = config_with_source("kind = \"clock\"\ntimezone = \"Mars/Olympus_Mons\"");
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    let err = cfg
        .validate()
        .expect_err("an unknown IANA timezone must fail validation");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("timezone") || msg.contains("iana") || msg.contains("zone"),
        "the error must name the timezone: {msg}"
    );
    assert!(
        msg.contains("mars/olympus_mons"),
        "the error must echo the bad id: {msg}"
    );
}

#[test]
fn clock_with_valid_iana_timezone_validates() {
    for tz in [
        "UTC",
        "Australia/Sydney",
        "America/New_York",
        "Europe/London",
    ] {
        let doc = config_with_source(&format!("kind = \"clock\"\ntimezone = \"{tz}\""));
        let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
        cfg.validate()
            .unwrap_or_else(|e| panic!("zone {tz:?} must validate: {e}"));
    }
}

#[test]
fn clock_timezone_wins_over_tz_offset_minutes_and_warns() {
    // When both are set, `timezone` (IANA, DST-correct) is authoritative and
    // `tz_offset_minutes` is reported as ignored via a validation warning. The
    // config still validates (both-present is legal, just redundant).
    let doc = config_with_source(
        "kind = \"clock\"\ntimezone = \"Australia/Sydney\"\ntz_offset_minutes = 123",
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    cfg.validate().expect("both-present still validates");
    let warnings = cfg.sources[0].clock_warnings();
    assert!(
        warnings
            .iter()
            .any(|w| w.to_lowercase().contains("timezone")
                && w.to_lowercase().contains("tz_offset_minutes")),
        "a both-present clock must warn that timezone wins: {warnings:?}"
    );
    // An out-of-range tz_offset_minutes is NOT an error when a timezone is set
    // (the offset is ignored), but the warning still fires.
    let doc2 = config_with_source(
        "kind = \"clock\"\ntimezone = \"Australia/Sydney\"\ntz_offset_minutes = 5000",
    );
    let cfg2 = MultiviewConfig::load_from_toml(&doc2).expect("parse");
    cfg2.validate()
        .expect("an ignored out-of-range offset does not fail when timezone wins");
}

#[test]
fn clock_without_timezone_still_range_checks_the_offset() {
    // Back-compat: with no `timezone`, `tz_offset_minutes` is authoritative and an
    // out-of-range value still fails (the existing rule is preserved).
    let doc = config_with_source("kind = \"clock\"\ntz_offset_minutes = 5000");
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    cfg.validate()
        .expect_err("an out-of-range offset with no timezone must still fail");
}

#[test]
fn a_full_multiview_of_synthetic_sources_validates() {
    // bars + clock + solid + timer wired into a 1x1 each must validate — proving
    // they are first-class peers of a decoded feed, not a special case.
    for fields in [
        "kind = \"bars\"",
        "kind = \"clock\"\nface = \"analog\"",
        "kind = \"solid\"\ncolor = \"#000000\"",
        "kind = \"timer\"\ntarget = \"time_of_day\"\nat = \"14:30:00\"\ntimezone = \"UTC\"",
    ] {
        let cfg = MultiviewConfig::load_from_toml(&config_with_source(fields))
            .unwrap_or_else(|e| panic!("parse {fields:?}: {e}"));
        cfg.validate()
            .unwrap_or_else(|e| panic!("validate {fields:?}: {e}"));
    }
}

#[test]
fn timer_is_synthetic() {
    let doc = config_with_source(
        "kind = \"timer\"\ntarget = \"date_time\"\nat = \"2026-07-01T09:00:00\"\ntimezone = \"UTC\"",
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    assert!(
        cfg.sources[0].kind.is_synthetic(),
        "a timer is an in-process synthetic source"
    );
}

#[test]
fn timer_time_of_day_round_trips_tagged_on_kind_and_target() {
    // The operator's "ON AIR IN" daily countdown: kind=timer, target=time_of_day,
    // recurring, rolling into the overrun count-up. Two distinct tag keys (`kind`
    // and `target`) must coexist without clashing (the flatten note in §6.2).
    let doc = config_with_source(
        "kind = \"timer\"\ntarget = \"time_of_day\"\nat = \"14:30:00\"\n\
         timezone = \"Australia/Sydney\"\nrecur_daily = true\ndirection = \"down\"\n\
         on_target = \"zero_then_up\"\nformat = \"auto\"\nlabel = \"ON AIR IN\"",
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    cfg.validate().expect("validate");
    let src = &cfg.sources[0];
    match &src.kind {
        SourceKind::Timer {
            target,
            direction,
            on_target,
            format,
            label,
            overrun_badge,
            ..
        } => {
            assert!(matches!(
                target,
                multiview_config::TimerTarget::TimeOfDay {
                    recur_daily: true,
                    ..
                }
            ));
            assert_eq!(*direction, multiview_config::TimerDirection::Down);
            assert_eq!(*on_target, multiview_config::TimerOnTarget::ZeroThenUp);
            assert_eq!(*format, multiview_config::TimerFormat::Auto);
            assert_eq!(label.as_deref(), Some("ON AIR IN"));
            // overrun_badge defaults to true (the opt-out boolean).
            assert!(*overrun_badge, "overrun_badge defaults to true");
        }
        other => panic!("expected a Timer, got {other:?}"),
    }
    // Re-serialize: both tags survive a TOML round-trip.
    let toml = toml::to_string(&cfg).expect("serialize");
    assert!(toml.contains("kind = \"timer\""), "kind tag: {toml}");
    assert!(
        toml.contains("target = \"time_of_day\""),
        "target tag: {toml}"
    );
}

#[test]
fn timer_datetime_round_trips_with_an_absolute_instant() {
    let doc = config_with_source(
        "kind = \"timer\"\ntarget = \"date_time\"\nat = \"2026-07-01T09:00:00\"\n\
         timezone = \"UTC\"\ndirection = \"down\"\non_target = \"hold\"\nformat = \"d_hh_mm_ss\"",
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    cfg.validate().expect("validate");
    assert!(matches!(
        &cfg.sources[0].kind,
        SourceKind::Timer {
            target: multiview_config::TimerTarget::DateTime { .. },
            ..
        }
    ));
}

#[test]
fn timer_rejects_a_malformed_time_and_unknown_zone() {
    let bad_time = config_with_source(
        "kind = \"timer\"\ntarget = \"time_of_day\"\nat = \"99:99:99\"\ntimezone = \"UTC\"",
    );
    let cfg = MultiviewConfig::load_from_toml(&bad_time).expect("parse");
    cfg.validate()
        .expect_err("a malformed time-of-day must fail validation");

    let bad_zone = config_with_source(
        "kind = \"timer\"\ntarget = \"date_time\"\nat = \"2026-07-01T09:00:00\"\n\
         timezone = \"Mars/Olympus_Mons\"",
    );
    let cfg = MultiviewConfig::load_from_toml(&bad_zone).expect("parse");
    cfg.validate()
        .expect_err("an unknown IANA zone must fail validation");
}

#[test]
fn timer_recur_requires_a_recurring_time_of_day_target() {
    // `recur` on a date_time target is rejected (it would silently degrade).
    let doc = config_with_source(
        "kind = \"timer\"\ntarget = \"date_time\"\nat = \"2026-07-01T09:00:00\"\n\
         timezone = \"UTC\"\non_target = \"recur\"",
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    cfg.validate()
        .expect_err("on_target = recur on a date_time target must fail validation");

    // `recur` on a non-recurring time_of_day is also rejected.
    let doc2 = config_with_source(
        "kind = \"timer\"\ntarget = \"time_of_day\"\nat = \"14:30:00\"\ntimezone = \"UTC\"\n\
         on_target = \"recur\"",
    );
    let cfg2 = MultiviewConfig::load_from_toml(&doc2).expect("parse");
    cfg2.validate()
        .expect_err("on_target = recur without recur_daily must fail validation");

    // `recur` with recur_daily = true is accepted.
    let ok = config_with_source(
        "kind = \"timer\"\ntarget = \"time_of_day\"\nat = \"14:30:00\"\ntimezone = \"UTC\"\n\
         recur_daily = true\non_target = \"recur\"",
    );
    let cfg3 = MultiviewConfig::load_from_toml(&ok).expect("parse");
    cfg3.validate().expect("recur with recur_daily is valid");
}
