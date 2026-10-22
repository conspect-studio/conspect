#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Serde round-trip + topic-routing + conflation contract tests for the AUD-8
//! program-bus loudness wire type: the [`AudioLoudness`] EBU R128 sample and the
//! `audio.loudness` [`Event`] variant.
//!
//! These prove the new variant is internally-tagged (`t`/`data`, never untagged),
//! survives a JSON round-trip, rides the reserved [`Topic::AudioLoudness`] lane,
//! and is classified **conflated** (drop-oldest, excluded from the lossless replay
//! ring) exactly like the high-rate `audio.meter` / `system.metrics` lanes
//! (invariant #10). The compliance reference fields (target LUFS, true-peak
//! ceiling, tolerance) ride the wire so the browser meter renders the same
//! compliance the engine normalises toward (ADR-R005/R006).

use multiview_events::{AudioLoudness, Envelope, Event, EventEnvelope, Seq, Topic};
use serde_json::{json, Value};

fn sample_loudness() -> AudioLoudness {
    // Values chosen to be exactly representable in `f32` so the JSON wire form
    // (which widens to `f64`) compares cleanly — this is a wire-shape test, not a
    // float-precision test.
    AudioLoudness {
        program: 0,
        momentary: Some(-22.5),
        short_term: Some(-23.5),
        integrated: Some(-23.0),
        lra: Some(4.25),
        true_peak_dbtp: Some(-2.5),
        target_lufs: -23.0,
        ceiling_dbtp: -1.5,
        tolerance_lu: 1.0,
        gain_db: Some(0.5),
        sampled_hz: 10,
    }
}

#[test]
fn audio_loudness_event_routes_on_the_audio_loudness_topic() {
    let env: EventEnvelope = Envelope::new(
        Topic::AudioLoudness,
        Seq::new(4242),
        multiview_core::time::MediaTime::from_nanos(1),
        Event::AudioLoudness(sample_loudness()),
    );

    let v: Value = serde_json::to_value(&env).unwrap();
    let obj = v.as_object().unwrap();
    // Internally-tagged: top-level `t`, body under `data`, no Rust field leak.
    assert_eq!(obj.get("t").unwrap(), &json!("audio.loudness"));
    assert_eq!(obj.get("topic").unwrap(), &json!("audio.loudness"));
    assert!(!obj.contains_key("payload"));

    let data = obj.get("data").unwrap().as_object().unwrap();
    assert_eq!(data.get("program").unwrap(), &json!(0));
    assert_eq!(data.get("short_term").unwrap(), &json!(-23.5));
    assert_eq!(data.get("integrated").unwrap(), &json!(-23.0));
    assert_eq!(data.get("lra").unwrap(), &json!(4.25));
    assert_eq!(data.get("true_peak_dbtp").unwrap(), &json!(-2.5));
    assert_eq!(data.get("target_lufs").unwrap(), &json!(-23.0));
    assert_eq!(data.get("ceiling_dbtp").unwrap(), &json!(-1.5));
    assert_eq!(data.get("tolerance_lu").unwrap(), &json!(1.0));
    assert_eq!(data.get("sampled_hz").unwrap(), &json!(10));

    let back: EventEnvelope = serde_json::from_value(v).unwrap();
    assert_eq!(back, env, "audio.loudness must survive a round-trip");
}

#[test]
fn audio_loudness_type_tag_and_is_data_event() {
    let event = Event::AudioLoudness(sample_loudness());
    assert_eq!(event.type_tag(), "audio.loudness");
    assert!(!event.is_control(), "audio.loudness is a data event");
    let v = serde_json::to_value(&event).unwrap();
    assert_eq!(v.get("t").unwrap(), &json!("audio.loudness"));
    let back: Event = serde_json::from_value(v).unwrap();
    assert_eq!(back, event, "audio.loudness event must round-trip");
}

#[test]
fn audio_loudness_is_conflated_latest_wins() {
    // The loudness lane is a high-rate latest-wins telemetry sample, so it is
    // excluded from the lossless replay ring (inv #10) — exactly like the
    // existing `audio.meter` / `system.metrics` conflated lanes.
    let event = Event::AudioLoudness(sample_loudness());
    assert!(
        event.is_conflated(),
        "audio.loudness is a conflated latest-wins telemetry sample"
    );
}

#[test]
fn audio_loudness_topic_is_high_rate() {
    // The `audio.loudness` topic is a conflated/sampled lane (ADR-RT003): a slow
    // client skips samples, never polls them, and the topic is excluded from the
    // bounded replay ring (healed by a re-snapshot).
    assert!(
        Topic::AudioLoudness.is_high_rate(),
        "audio.loudness must be a high-rate conflated lane"
    );
}

#[test]
fn audio_loudness_gate_silence_omits_optional_lufs() {
    // Below the absolute gate the engine's meter returns no loudness; those
    // fields are `None` and must be OMITTED on the wire (never a false -inf or 0),
    // while the compliance reference fields (target/ceiling/tolerance) always ride.
    let gated = AudioLoudness {
        program: 1,
        momentary: None,
        short_term: None,
        integrated: None,
        lra: None,
        true_peak_dbtp: None,
        target_lufs: -16.0,
        ceiling_dbtp: -1.5,
        tolerance_lu: 1.0,
        gain_db: None,
        sampled_hz: 10,
    };
    let v = serde_json::to_value(&gated).unwrap();
    let obj = v.as_object().unwrap();
    assert!(!obj.contains_key("momentary"), "gated momentary is omitted");
    assert!(
        !obj.contains_key("short_term"),
        "gated short_term is omitted"
    );
    assert!(
        !obj.contains_key("integrated"),
        "gated integrated is omitted"
    );
    assert!(!obj.contains_key("lra"), "gated lra is omitted");
    assert!(
        !obj.contains_key("true_peak_dbtp"),
        "gated true_peak_dbtp is omitted"
    );
    assert!(!obj.contains_key("gain_db"), "absent gain is omitted");
    // The compliance reference is always present (the meter needs it to colour).
    assert_eq!(obj.get("target_lufs").unwrap(), &json!(-16.0));
    assert_eq!(obj.get("ceiling_dbtp").unwrap(), &json!(-1.5));
    assert_eq!(obj.get("tolerance_lu").unwrap(), &json!(1.0));
    assert_eq!(obj.get("program").unwrap(), &json!(1));

    let back: AudioLoudness = serde_json::from_value(v).unwrap();
    assert_eq!(back, gated, "gated loudness must round-trip");
}

#[test]
fn unknown_audio_loudness_discriminator_is_rejected() {
    // Tagged, never untagged: a near-miss tag must hard-fail, not fall through.
    let bad = json!({"t": "audio.loudness.exploded", "data": {}});
    let parsed: Result<Event, _> = serde_json::from_value(bad);
    assert!(parsed.is_err());
}
