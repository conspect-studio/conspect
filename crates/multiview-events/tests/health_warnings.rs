#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Serde round-trip + topic-routing contract tests for the SA-0 health-warning
//! wire types (ADR-0035): the [`HealthWarning`] sibling-of-`Alert` model and the
//! `health.warning.raised` / `health.warning.cleared` [`Event`] variants. These
//! prove the new variants are internally-tagged (`t`/`data`, never untagged),
//! survive a JSON round-trip, ride [`Topic::Alerts`] (reusing the operator-alert
//! lane), and that the warning's actionable fields (code/severity/subsystem/
//! message/remediation/since) are carried on the wire.

use multiview_events::{
    Envelope, Event, EventEnvelope, HealthWarning, Seq, Topic, WarningCode, WarningSeverity,
};
use serde_json::{json, Value};

fn sample_warning() -> HealthWarning {
    HealthWarning {
        code: WarningCode::GpuPresentNoVulkanAdapter,
        severity: WarningSeverity::Warning,
        subsystem: "compositor".to_owned(),
        message: "GPU NVIDIA GeForce RTX 4060 detected (NVML) but GPU compositing \
                  is UNAVAILABLE (no Vulkan adapter); compositing fell back to CPU."
            .to_owned(),
        remediation: "Set NVIDIA_DRIVER_CAPABILITIES to include `graphics` (or `all`) \
                      and install `libvulkan1` + the `nvidia_icd.json` ICD."
            .to_owned(),
        since: 1_700_000_000_000_000_000,
        active: true,
    }
}

#[test]
fn health_warning_raised_event_routes_on_the_alerts_topic() {
    // The new variants reuse the existing operator-alert lane (Topic::Alerts).
    let env: EventEnvelope = Envelope::new(
        Topic::Alerts,
        Seq::new(9001),
        multiview_core::time::MediaTime::from_nanos(1),
        Event::HealthWarningRaised(sample_warning()),
    )
    .with_id("gpu-present-no-vulkan-adapter");

    let v: Value = serde_json::to_value(&env).unwrap();
    let obj = v.as_object().unwrap();
    // Internally-tagged: top-level `t`, body under `data`, no Rust field leak.
    assert_eq!(obj.get("t").unwrap(), &json!("health.warning.raised"));
    assert_eq!(obj.get("topic").unwrap(), &json!("alerts"));
    assert!(!obj.contains_key("payload"));

    let data = obj.get("data").unwrap().as_object().unwrap();
    // The actionable fields ride the wire; `code` is the stable kebab-case enum.
    assert_eq!(
        data.get("code").unwrap(),
        &json!("gpu-present-no-vulkan-adapter")
    );
    assert_eq!(data.get("severity").unwrap(), &json!("warning"));
    assert_eq!(data.get("subsystem").unwrap(), &json!("compositor"));
    assert!(data
        .get("message")
        .unwrap()
        .as_str()
        .unwrap()
        .contains("CPU"));
    assert!(data
        .get("remediation")
        .unwrap()
        .as_str()
        .unwrap()
        .contains("graphics"));
    assert_eq!(data.get("active").unwrap(), &json!(true));

    let back: EventEnvelope = serde_json::from_value(v).unwrap();
    assert_eq!(back, env, "health.warning.raised must survive a round-trip");
}

#[test]
fn both_health_warning_variants_roundtrip_and_route() {
    let warning = sample_warning();
    let cleared = {
        let mut w = warning.clone();
        w.active = false;
        w
    };
    let cases: Vec<(Event, &str)> = vec![
        (Event::HealthWarningRaised(warning), "health.warning.raised"),
        (
            Event::HealthWarningCleared(cleared),
            "health.warning.cleared",
        ),
    ];
    for (event, tag) in cases {
        assert_eq!(event.type_tag(), tag, "type_tag mismatch for {tag}");
        assert!(!event.is_control(), "{tag} is a data event, not control");
        // Both ride the Alerts topic.
        let v = serde_json::to_value(&event).unwrap();
        assert_eq!(v.get("t").unwrap(), &json!(tag));
        let back: Event = serde_json::from_value(v).unwrap();
        assert_eq!(back, event, "{tag} must round-trip");
    }
}

#[test]
fn warning_code_is_kebab_case_and_non_exhaustive_forward_compatible() {
    // The catalog code is stable kebab-case on the wire.
    let v = serde_json::to_value(WarningCode::GpuPresentNoVulkanAdapter).unwrap();
    assert_eq!(v, json!("gpu-present-no-vulkan-adapter"));
    let back: WarningCode = serde_json::from_value(v).unwrap();
    assert_eq!(back, WarningCode::GpuPresentNoVulkanAdapter);
}

#[test]
fn unknown_health_warning_discriminator_is_rejected() {
    // Tagged, never untagged: a near-miss tag must hard-fail, not fall through.
    let bad = json!({"t": "health.warning.exploded", "data": {}});
    let parsed: Result<Event, _> = serde_json::from_value(bad);
    assert!(parsed.is_err());
}

#[test]
fn health_warning_severity_renders_snake_case() {
    for (sev, wire) in [
        (WarningSeverity::Info, "info"),
        (WarningSeverity::Warning, "warning"),
        (WarningSeverity::Critical, "critical"),
    ] {
        let v = serde_json::to_value(sev).unwrap();
        assert_eq!(v, json!(wire));
        let back: WarningSeverity = serde_json::from_value(v).unwrap();
        assert_eq!(back, sev);
    }
}

#[test]
fn config_file_warning_codes_round_trip_kebab_case() {
    // ADR-W020: the config-file watcher's two catalog codes — raised by the
    // control-plane watcher (not the engine) through the same drop-oldest
    // publisher, mirrored by the same warning ingest.
    for (code, wire) in [
        (WarningCode::ConfigFileInvalid, "config-file-invalid"),
        (
            WarningCode::ConfigFileRequiresRestart,
            "config-file-requires-restart",
        ),
    ] {
        assert_eq!(code.as_str(), wire, "as_str must match the wire string");
        let v = serde_json::to_value(code).unwrap();
        assert_eq!(v, json!(wire));
        let back: WarningCode = serde_json::from_value(v).unwrap();
        assert_eq!(back, code, "{wire} must round-trip");
    }
}

#[test]
fn config_file_apply_incomplete_code_round_trips_kebab_case() {
    // ADR-W020 review M1: the interim warning raised while a valid file
    // change was only PARTIALLY applied (engine command(s) shed on a full
    // bus); the watcher retries and clears it when the apply completes.
    let code = WarningCode::ConfigFileApplyIncomplete;
    assert_eq!(code.as_str(), "config-file-apply-incomplete");
    let v = serde_json::to_value(code).unwrap();
    assert_eq!(v, json!("config-file-apply-incomplete"));
    let back: WarningCode = serde_json::from_value(v).unwrap();
    assert_eq!(back, code);
}
