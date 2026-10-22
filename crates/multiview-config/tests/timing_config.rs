//! DEV-C1 (ADR-M010): the `[timing]` config block — the per-deployment
//! outbound **link offset** (AES67 semantics applied to video: a fixed
//! receiver-side presentation delay; uniformity across nodes over smallness)
//! plus the optional PHC device path the `ptp`-feature build samples.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::{MultiviewConfig, TimingConfig};

/// A minimal, valid document (mirrors `validation.rs`'s BASE).
const BASE: &str = r##"
schema_version = 1

[canvas]
width = 1920
height = 1080
fps = "30000/1001"
pixel_format = "nv12"
background = "#101014"

[canvas.color]
profile = "sdr-bt709-limited"

[layout]
kind = "grid"
columns = ["1fr", "1fr"]
rows = ["1fr", "1fr"]
gap = 8
areas = ["a b", "c d"]

[[sources]]
id = "in_a"
kind = "test"

[[cells]]
id = "cell_a"
area = "a"
fit = "contain"
[cells.source]
input_id = "in_a"

[[outputs]]
kind = "rtsp_server"
mount = "/multiview"
codec = "h264"
"##;

#[test]
fn timing_block_is_optional_and_defaults_apply() {
    let cfg = MultiviewConfig::load_from_toml(BASE).unwrap();
    cfg.validate().expect("base document is valid");
    assert!(cfg.timing.is_none(), "no [timing] block parses as None");
    // The default link offset (ADR-M010: typically 100-300 ms; ours is 150 ms).
    let d = TimingConfig::default();
    assert_eq!(d.link_offset_ms, 150);
    assert_eq!(d.link_offset_ns(), 150_000_000);
    assert_eq!(d.ptp_phc, None);
}

#[test]
fn timing_block_parses_from_toml() {
    let doc = format!("{BASE}\n[timing]\nlink_offset_ms = 200\nptp_phc = \"/dev/ptp0\"\n");
    let cfg = MultiviewConfig::load_from_toml(&doc).unwrap();
    cfg.validate().expect("a 200 ms link offset is valid");
    let timing = cfg.timing.expect("timing block present");
    assert_eq!(timing.link_offset_ms, 200);
    assert_eq!(timing.link_offset_ns(), 200_000_000);
    assert_eq!(timing.ptp_phc.as_deref(), Some("/dev/ptp0"));
}

#[test]
fn timing_block_round_trips_toml_and_json() {
    let doc = format!("{BASE}\n[timing]\nlink_offset_ms = 120\n");
    let cfg = MultiviewConfig::load_from_toml(&doc).unwrap();
    let json = cfg.to_json().expect("serialize json");
    let back = MultiviewConfig::load_from_json(&json).expect("parse json");
    assert_eq!(back.timing, cfg.timing);
    let toml = cfg.to_toml().expect("serialize toml");
    let back2 = MultiviewConfig::load_from_toml(&toml).expect("parse toml");
    assert_eq!(back2.timing, cfg.timing);
}

#[test]
fn an_absurd_link_offset_is_rejected() {
    // Beyond 10 s the value is a typo, not a presentation-delay policy
    // (the same bound rationale as the sync-group offset cap).
    let doc = format!("{BASE}\n[timing]\nlink_offset_ms = 10001\n");
    let cfg = MultiviewConfig::load_from_toml(&doc).unwrap();
    let err = cfg
        .validate()
        .expect_err("a 10+ second link offset must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("link_offset"),
        "the error must name the offending knob, got: {msg}"
    );
}

#[test]
fn the_boundary_link_offset_is_accepted() {
    let doc = format!("{BASE}\n[timing]\nlink_offset_ms = 10000\n");
    let cfg = MultiviewConfig::load_from_toml(&doc).unwrap();
    cfg.validate().expect("exactly 10 s is the inclusive bound");
}

#[test]
fn the_ptp_utc_offset_defaults_to_the_current_tai_utc_offset() {
    // The PHC under standard linuxptp (ptp4l ST 2059-2 + phc2sys) carries
    // PTP time = TAI; the published epoch is UTC. The default conversion
    // offset is the current TAI−UTC = 37 s (sourced from ptp4l's
    // currentUtcOffset in a real deployment).
    let d = TimingConfig::default();
    assert_eq!(d.ptp_utc_offset_s, 37);
    assert_eq!(d.ptp_utc_offset_ns(), 37_000_000_000, "exact integer s→ns");
}

#[test]
fn the_ptp_utc_offset_parses_and_validates() {
    let doc = format!("{BASE}\n[timing]\nptp_phc = \"/dev/ptp0\"\nptp_utc_offset_s = 0\n");
    let cfg = MultiviewConfig::load_from_toml(&doc).unwrap();
    cfg.validate()
        .expect("a UTC PHC deployment may set the offset to 0");
    let timing = cfg.timing.expect("timing block present");
    assert_eq!(timing.ptp_utc_offset_s, 0);
    assert_eq!(timing.ptp_utc_offset_ns(), 0);
}

#[test]
fn an_absurd_or_negative_ptp_utc_offset_is_rejected() {
    // TAI−UTC has never been negative and is nowhere near 1000 s: out-of-band
    // values are typos/sign errors, not timescale policy.
    for bad in ["1001", "-1"] {
        let doc = format!("{BASE}\n[timing]\nptp_utc_offset_s = {bad}\n");
        let cfg = MultiviewConfig::load_from_toml(&doc).unwrap();
        let err = cfg
            .validate()
            .expect_err("an out-of-band ptp_utc_offset_s must fail validation");
        let msg = err.to_string();
        assert!(
            msg.contains("ptp_utc_offset"),
            "the error must name the offending knob, got: {msg}"
        );
    }
}

#[test]
fn an_unknown_timing_key_is_rejected_at_parse() {
    // Every sibling block denies unknown fields so a typo'd key fails at parse
    // instead of silently applying the default; [timing] must too.
    let doc = format!("{BASE}\n[timing]\nlink_offset_msec = 200\n");
    let err = MultiviewConfig::load_from_toml(&doc)
        .expect_err("a typo'd [timing] key must fail at parse, never be silently dropped");
    let msg = err.to_string();
    assert!(
        msg.contains("link_offset_msec"),
        "the parse error must name the unknown key, got: {msg}"
    );
}
