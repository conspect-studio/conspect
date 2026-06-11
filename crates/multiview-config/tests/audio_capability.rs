//! AUD-7: per-output **audio capability** cross-check (the verified matrix from
//! ADR-R005 §4.2 / resilience-and-av §4.2, machine-readable).
//!
//! These tests pin the *designed-in asymmetry* the brief calls for: a transport
//! that carries N simultaneous discrete tracks (MPEG-TS via SRT, RTSP) accepts a
//! multitrack selection, while a transport that cannot (NDI = channel-map only;
//! legacy RTMP = one track) **rejects** it with a typed
//! [`multiview_config::ConfigError::AudioCapability`] — never a panic, never a
//! generic string. HLS is *select-one*: multiple tracks are a selector, not
//! simultaneous monitoring, so the document is accepted but the matrix records
//! the select-one semantics. The matrix is exposed as a first-class data
//! structure (`Output::audio_capability`) for reuse by the Web UI (AUD-8).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::{
    ConfigError, MultiviewConfig, Output, OutputAudioCapability, TrackCapacity, TrackDelivery,
};

/// The shared canvas/layout/sources/audio prefix. Two stereo cameras, a program
/// bus, and two discrete tracks (`trk_a`, `trk_b`). The per-output `[[outputs]]`
/// block is appended per test so each transport is exercised against the same
/// two-discrete-track routing.
const PREFIX: &str = r##"
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
kind = "preset"
preset = "2x2"

[[sources]]
id = "cam_a"
kind = "test"
[[sources]]
id = "cam_b"
kind = "test"

[[cells]]
id = "cell_a"
rect = { x = 0.0, y = 0.0, w = 0.5, h = 1.0 }
[cells.source]
input_id = "cam_a"

[[cells]]
id = "cell_b"
rect = { x = 0.5, y = 0.0, w = 0.5, h = 1.0 }
[cells.source]
input_id = "cam_b"

[audio]
sample_rate_hz = 48000

[[audio.routes]]
input_id = "cam_a"
channels = { kind = "stereo" }
target_track = "trk_a"
include_in_program_bus = true
gain_db = 0.0

[[audio.routes]]
input_id = "cam_b"
channels = { kind = "stereo" }
target_track = "trk_b"
include_in_program_bus = true
gain_db = 0.0
"##;

fn parse(doc: &str) -> MultiviewConfig {
    MultiviewConfig::load_from_toml(doc).expect("doc must parse")
}

/// MPEG-TS-over-SRT carries N simultaneous PIDs — two discrete tracks selected
/// on an SRT output must validate (the positive half of the asymmetry).
#[test]
fn srt_accepts_two_discrete_tracks() {
    let doc = format!(
        "{PREFIX}\n[[outputs]]\nkind = \"srt\"\nurl = \"srt://[::1]:9000\"\ncodec = \"h264\"\n\
         [outputs.audio]\nmode = \"tracks\"\ntracks = [\"prog\", \"trk_a\", \"trk_b\"]\n"
    );
    let cfg = parse(&doc);
    cfg.validate()
        .expect("SRT (N PIDs) must accept two simultaneous discrete tracks");
}

/// RTSP carries N simultaneous `m=audio` subsessions — two discrete tracks must
/// validate.
#[test]
fn rtsp_accepts_two_discrete_tracks() {
    let doc = format!(
        "{PREFIX}\n[[outputs]]\nkind = \"rtsp_server\"\nmount = \"/multiview\"\ncodec = \"h264\"\n\
         [outputs.audio]\nmode = \"tracks\"\ntracks = [\"prog\", \"trk_a\", \"trk_b\"]\n"
    );
    let cfg = parse(&doc);
    cfg.validate()
        .expect("RTSP (N subsessions) must accept two simultaneous discrete tracks");
}

/// NDI carries **no selectable discrete tracks** (channel-map only). A two-track
/// NDI selection must be rejected with the typed capability error naming NDI.
#[test]
fn ndi_rejects_discrete_tracks_with_typed_error() {
    let doc = format!(
        "{PREFIX}\n[[outputs]]\nkind = \"ndi\"\nname = \"Multiview\"\n\
         [outputs.audio]\nmode = \"tracks\"\ntracks = [\"prog\", \"trk_a\", \"trk_b\"]\n"
    );
    let cfg = parse(&doc);
    let err = cfg
        .validate()
        .expect_err("NDI must reject selectable discrete tracks");
    match &err {
        ConfigError::AudioCapability { output, reason } => {
            assert!(
                reason.to_lowercase().contains("ndi") || reason.to_lowercase().contains("channel"),
                "reason explains the NDI channel-map limit: {reason}"
            );
            assert!(!output.is_empty(), "names the offending output");
        }
        other => panic!("expected AudioCapability, got {other:?}"),
    }
}

/// NDI carrying only the program bus is fine (one mixed stream → channels).
#[test]
fn ndi_accepts_program_only() {
    let doc = format!(
        "{PREFIX}\n[[outputs]]\nkind = \"ndi\"\nname = \"Multiview\"\n\
         [outputs.audio]\nmode = \"program\"\n"
    );
    let cfg = parse(&doc);
    cfg.validate()
        .expect("NDI carrying only the mixed program bus must validate");
}

/// Legacy RTMP carries exactly one audio track. Two discrete tracks must be
/// rejected with the typed capability error (degrade explicitly to the bus —
/// never silently drop tracks).
#[test]
fn legacy_rtmp_rejects_multitrack_with_typed_error() {
    let doc = format!(
        "{PREFIX}\n[[outputs]]\nkind = \"rtmp\"\nurl = \"rtmp://[::1]/live/key\"\ncodec = \"h264\"\n\
         [outputs.audio]\nmode = \"tracks\"\ntracks = [\"prog\", \"trk_a\", \"trk_b\"]\n"
    );
    let cfg = parse(&doc);
    let err = cfg
        .validate()
        .expect_err("legacy RTMP must reject a multitrack selection");
    match &err {
        ConfigError::AudioCapability { output, reason } => {
            assert!(
                reason.to_lowercase().contains("rtmp") || reason.to_lowercase().contains("track"),
                "reason explains the RTMP single-track limit: {reason}"
            );
            assert!(
                output.contains("rtmp") || !output.is_empty(),
                "names the output"
            );
        }
        other => panic!("expected AudioCapability, got {other:?}"),
    }
}

/// A single discrete track on legacy RTMP is within capacity (one track) and
/// must validate — the limit is N>1, not "any discrete track".
#[test]
fn legacy_rtmp_accepts_single_track() {
    let doc = format!(
        "{PREFIX}\n[[outputs]]\nkind = \"rtmp\"\nurl = \"rtmp://[::1]/live/key\"\ncodec = \"h264\"\n\
         [outputs.audio]\nmode = \"tracks\"\ntracks = [\"trk_a\"]\n"
    );
    let cfg = parse(&doc);
    cfg.validate()
        .expect("legacy RTMP must accept exactly one discrete track");
}

/// Enhanced-RTMP v2 multitrack is endpoint-gated: when the operator declares the
/// endpoint supports multitrack, the same two-track selection that legacy RTMP
/// rejected now validates (per ADR-R005: negotiate per endpoint).
#[test]
fn enhanced_rtmp_accepts_multitrack_when_declared() {
    let doc = format!(
        "{PREFIX}\n[[outputs]]\nkind = \"rtmp\"\nurl = \"rtmp://[::1]/live/key\"\ncodec = \"h264\"\n\
         multitrack = true\n\
         [outputs.audio]\nmode = \"tracks\"\ntracks = [\"prog\", \"trk_a\", \"trk_b\"]\n"
    );
    let cfg = parse(&doc);
    cfg.validate()
        .expect("Enhanced-RTMP v2 (multitrack endpoint) must accept two discrete tracks");
}

/// HLS is select-one: a multitrack selection is a *selector* (one played at a
/// time), so the document is accepted, and the matrix records the select-one
/// delivery semantics (the UI renders a selector, not simultaneous monitors).
#[test]
fn hls_accepts_multitrack_as_select_one() {
    let doc = format!(
        "{PREFIX}\n[[outputs]]\nkind = \"hls\"\npath = \"/srv/hls\"\ncodec = \"h264\"\n\
         [outputs.audio]\nmode = \"tracks\"\ntracks = [\"prog\", \"trk_a\", \"trk_b\"]\n"
    );
    let cfg = parse(&doc);
    cfg.validate()
        .expect("HLS (select-one) must accept a multitrack selector");
}

/// The capability matrix is machine-readable and first-class: each transport
/// reports the delivery mode and discrete-track capacity the validator uses, so
/// the Web UI (AUD-8) can grey out impossible cells without re-deriving the rules.
#[test]
fn capability_matrix_is_machine_readable_per_transport() {
    let cases: &[(&str, TrackDelivery, TrackCapacity)] = &[
        (
            "[[outputs]]\nkind = \"srt\"\nurl = \"srt://[::1]:9000\"\ncodec = \"h264\"\n",
            TrackDelivery::Simultaneous,
            TrackCapacity::Unlimited,
        ),
        (
            "[[outputs]]\nkind = \"rtsp_server\"\nmount = \"/m\"\ncodec = \"h264\"\n",
            TrackDelivery::Simultaneous,
            TrackCapacity::Unlimited,
        ),
        (
            "[[outputs]]\nkind = \"hls\"\npath = \"/srv/hls\"\ncodec = \"h264\"\n",
            TrackDelivery::SelectOne,
            TrackCapacity::Unlimited,
        ),
        (
            "[[outputs]]\nkind = \"ndi\"\nname = \"M\"\n",
            TrackDelivery::None,
            TrackCapacity::AtMost(0),
        ),
        (
            "[[outputs]]\nkind = \"rtmp\"\nurl = \"rtmp://[::1]/l/k\"\ncodec = \"h264\"\n",
            TrackDelivery::Simultaneous,
            TrackCapacity::AtMost(1),
        ),
        // AES67 / ST 2110-30: one multicast PCM channel-map flow, no selectable
        // discrete tracks — mirrors NDI (a discrete-track route is a capability
        // error). Guards the AUD-7 x AES67 integration arm.
        (
            "[[outputs]]\nkind = \"aes67\"\nlabel = \"A\"\nmulticast = \"[ff3e::1]:5004\"\n",
            TrackDelivery::None,
            TrackCapacity::AtMost(0),
        ),
    ];

    for (out_toml, want_delivery, want_capacity) in cases {
        let doc = format!("{PREFIX}\n{out_toml}");
        let cfg = parse(&doc);
        let output: &Output = cfg.outputs.first().expect("one output declared");
        let cap: OutputAudioCapability = output.audio_capability();
        assert_eq!(cap.delivery, *want_delivery, "delivery for {out_toml:?}");
        assert_eq!(
            cap.discrete_capacity, *want_capacity,
            "capacity for {out_toml:?}"
        );
    }
}

/// `TrackCapacity::accepts` is the single source of truth the validator and the
/// UI share: an unlimited carrier accepts any count, a bounded one accepts up to
/// its ceiling and rejects beyond it.
#[test]
fn track_capacity_accepts_is_exact() {
    assert!(TrackCapacity::Unlimited.accepts(0));
    assert!(TrackCapacity::Unlimited.accepts(64));
    assert!(TrackCapacity::AtMost(1).accepts(0));
    assert!(TrackCapacity::AtMost(1).accepts(1));
    assert!(!TrackCapacity::AtMost(1).accepts(2));
    assert!(TrackCapacity::AtMost(0).accepts(0));
    assert!(!TrackCapacity::AtMost(0).accepts(1));
}
