//! WebRTC config-schema tests (lane A4 of the full-WebRTC push):
//!
//! - `SourceKind::Webrtc` — the WHIP-ingest contribution source (ADR-T014 §1):
//!   the endpoint URL is derived (`/api/v1/whip/{source_id}`), never configured;
//!   `token = None` means publishing requires a Write-scope API key.
//! - `Output::Webrtc` (WHEP serve) and `Output::WhipPush` (RFC 9725 push
//!   client) — ADR-0049: encode-once consumers of an existing H.264 rendition
//!   (`codec = "h264"` is the only v1 value), single-track audio (one Opus
//!   m-line).
//! - The top-level `[webrtc]` section — ADR-0048 §9: the shared transport
//!   endpoint knobs, fully defaulted when absent.
//!
//! Pure serde + validation — TOML/JSON round-trip, no engine, no network.
//! IPv6-first (ADR-0042): every networked example leads with a bracketed IPv6
//! literal.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::{
    DurationString, MultiviewConfig, Output, OutputAudio, OutputAudioMode, Source, SourceKind,
    TrackCapacity, TrackDelivery, WebrtcConfig,
};

/// A minimal valid document used as the base for section/validation tests.
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
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]

[[sources]]
id = "in_a"
kind = "bars"

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

// ---------------------------------------------------------------------------
// SourceKind::Webrtc (ADR-T014 §1)
// ---------------------------------------------------------------------------

#[test]
fn source_webrtc_minimal_defaults() {
    // The minimal authored form: token absent (⇒ Write-scope API key required,
    // never anonymous), audio defaulted to true.
    let toml_str = r#"
id = "cam-field-1"
kind = "webrtc"
"#;
    let src: Source = toml::from_str(toml_str).expect("minimal webrtc source");
    assert_eq!(src.id, "cam-field-1");
    match &src.kind {
        SourceKind::Webrtc { token, audio } => {
            assert_eq!(*token, None, "token defaults to None (API-key-only)");
            assert!(*audio, "audio defaults to true (one Opus m-line accepted)");
        }
        other => panic!("expected Webrtc, got {other:?}"),
    }
    src.validate().expect("minimal webrtc source validates");
}

#[test]
fn source_webrtc_full_roundtrip_toml() {
    let toml_str = r#"
id = "cam-field-1"
kind = "webrtc"
token = "s3cret"
audio = false
"#;
    let original: Source = toml::from_str(toml_str).expect("parse");
    let reparsed: Source =
        toml::from_str(&toml::to_string(&original).expect("serialize")).expect("re-parse");
    assert_eq!(original, reparsed, "TOML round-trip identity");
    match &original.kind {
        SourceKind::Webrtc { token, audio } => {
            assert_eq!(token.as_deref(), Some("s3cret"));
            assert!(!audio, "audio = false answers the m-line inactive");
        }
        other => panic!("expected Webrtc, got {other:?}"),
    }
    original.validate().expect("full webrtc source validates");
}

#[test]
fn source_webrtc_roundtrip_json_skips_defaults() {
    let json_in = r#"{ "id": "cam-field-1", "kind": "webrtc" }"#;
    let original: Source = serde_json::from_str(json_in).expect("parse json");
    let json = serde_json::to_string(&original).expect("serialize json");
    let reparsed: Source = serde_json::from_str(&json).expect("re-parse json");
    assert_eq!(original, reparsed, "JSON round-trip identity");
    // Internal tag, never untagged.
    assert!(json.contains("\"kind\":\"webrtc\""), "{json}");
    // Default-valued fields do not serialize.
    assert!(!json.contains("token"), "absent token stays absent: {json}");
    assert!(
        !json.contains("audio"),
        "default audio=true is skipped: {json}"
    );
}

#[test]
fn source_webrtc_is_not_synthetic() {
    let src: Source = toml::from_str("id = \"cam\"\nkind = \"webrtc\"\n").expect("parse");
    assert!(
        !src.kind.is_synthetic(),
        "a WHIP contribution source is decoded media, not synthetic"
    );
}

#[test]
fn source_webrtc_empty_token_rejected() {
    let src: Source = toml::from_str("id = \"cam\"\nkind = \"webrtc\"\ntoken = \"\"\n")
        .expect("parses structurally");
    let err = src
        .validate()
        .expect_err("empty token must fail validation");
    assert!(
        err.to_string().contains("token"),
        "error should name the token, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// Output::Webrtc — WHEP serve (ADR-0049)
// ---------------------------------------------------------------------------

#[test]
fn output_webrtc_deserializes_full() {
    let toml_str = r#"
kind = "webrtc"
id = "pgm-whep"
label = "Program WHEP"
max_viewers = 16
token = "viewer-secret"
codec = "h264"
"#;
    let out: Output = toml::from_str(toml_str).expect("valid webrtc output");
    match &out {
        Output::Webrtc {
            id,
            label,
            max_viewers,
            token,
            codec,
            gpu_pin,
            audio,
            ..
        } => {
            assert_eq!(id.as_deref(), Some("pgm-whep"));
            assert_eq!(label, "Program WHEP");
            assert_eq!(*max_viewers, 16);
            assert_eq!(token.as_deref(), Some("viewer-secret"));
            assert_eq!(codec, "h264");
            assert!(gpu_pin.is_none());
            assert!(audio.is_none());
        }
        other => panic!("expected Webrtc output, got {other:?}"),
    }
    out.validate().expect("full webrtc output validates");
}

#[test]
fn output_webrtc_defaults_max_viewers_and_codec() {
    let toml_str = r#"
kind = "webrtc"
label = "Defaulted"
"#;
    let out: Output = toml::from_str(toml_str).expect("webrtc output with defaults");
    match &out {
        Output::Webrtc {
            max_viewers,
            codec,
            token,
            ..
        } => {
            assert_eq!(*max_viewers, 8, "max_viewers defaults to 8 (ADR-0049)");
            assert_eq!(codec, "h264", "codec defaults to h264 (the only v1 value)");
            assert_eq!(*token, None, "token defaults to None (API-key View scope)");
        }
        other => panic!("expected Webrtc output, got {other:?}"),
    }
    out.validate().expect("defaulted webrtc output validates");
}

#[test]
fn output_webrtc_accessors_and_id_derivation() {
    let toml_str = r#"
kind = "webrtc"
label = "Program WHEP"
audio = { mode = "program" }
"#;
    let out: Output = toml::from_str(toml_str).expect("valid webrtc output");
    // Explicit label, like Aes67 (no mount/path/url to derive one from).
    assert_eq!(out.label(), "Program WHEP");
    assert_eq!(out.explicit_id(), None);
    assert_eq!(
        out.id(),
        "Program WHEP",
        "id derives from label when absent"
    );
    assert!(out.gpu_pin().is_none());
    assert!(matches!(
        out.audio(),
        Some(OutputAudio {
            mode: OutputAudioMode::Program,
            ..
        })
    ));
}

#[test]
fn output_webrtc_audio_capability_is_single_track() {
    // ADR-0049: one Opus m-line — single-track; multitrack selections are
    // rejected at config time, matching the capability matrix.
    let out: Output =
        toml::from_str("kind = \"webrtc\"\nlabel = \"WHEP\"\n").expect("valid webrtc output");
    let cap = out.audio_capability();
    assert_eq!(cap.delivery, TrackDelivery::Simultaneous);
    assert_eq!(cap.discrete_capacity, TrackCapacity::AtMost(1));
    assert!(cap.discrete_capacity.accepts(1));
    assert!(!cap.discrete_capacity.accepts(2));
}

#[test]
fn output_webrtc_roundtrip_json_skips_defaults() {
    let json_in = r#"{ "kind": "webrtc", "label": "RT WHEP" }"#;
    let original: Output = serde_json::from_str(json_in).expect("parse");
    let json = serde_json::to_string(&original).expect("serialize");
    let reparsed: Output = serde_json::from_str(&json).expect("re-parse");
    assert_eq!(original, reparsed, "JSON round-trip identity");
    assert!(json.contains("\"kind\":\"webrtc\""), "{json}");
    // Default-valued fields do not serialize.
    assert!(
        !json.contains("max_viewers"),
        "default 8 is skipped: {json}"
    );
    assert!(!json.contains("codec"), "default h264 is skipped: {json}");
    assert!(!json.contains("token"), "absent token stays absent: {json}");
}

#[test]
fn output_webrtc_non_h264_codec_rejected() {
    let out: Output = toml::from_str("kind = \"webrtc\"\nlabel = \"W\"\ncodec = \"vp8\"\n")
        .expect("parses structurally");
    let err = out.validate().expect_err("non-h264 codec must fail (v1)");
    let msg = err.to_string();
    assert!(msg.contains("h264"), "error should name h264, got: {msg}");
    assert!(
        msg.contains("ADR-0049"),
        "error should cite ADR-0049, got: {msg}"
    );
}

#[test]
fn output_webrtc_zero_max_viewers_rejected() {
    let out: Output = toml::from_str("kind = \"webrtc\"\nlabel = \"W\"\nmax_viewers = 0\n")
        .expect("parses structurally");
    let err = out.validate().expect_err("max_viewers = 0 must fail");
    assert!(
        err.to_string().contains("max_viewers"),
        "error should name max_viewers, got: {err}"
    );
}

#[test]
fn output_webrtc_empty_token_rejected() {
    let out: Output = toml::from_str("kind = \"webrtc\"\nlabel = \"W\"\ntoken = \"\"\n")
        .expect("parses structurally");
    let err = out.validate().expect_err("empty token must fail");
    assert!(
        err.to_string().contains("token"),
        "error should name the token, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// Output::WhipPush — RFC 9725 push client (ADR-0049)
// ---------------------------------------------------------------------------

#[test]
fn output_whip_push_deserializes_and_roundtrips() {
    // IPv6-first: the example endpoint is a bracketed IPv6 https literal.
    let toml_str = r#"
kind = "whip_push"
id = "origin-push"
url = "https://[2001:db8::15]:8443/whip/pgm1"
token = "push-secret"
"#;
    let original: Output = toml::from_str(toml_str).expect("valid whip_push output");
    match &original {
        Output::WhipPush {
            id,
            url,
            token,
            codec,
            gpu_pin,
            audio,
            ..
        } => {
            assert_eq!(id.as_deref(), Some("origin-push"));
            assert_eq!(url, "https://[2001:db8::15]:8443/whip/pgm1");
            assert_eq!(token.as_deref(), Some("push-secret"));
            assert_eq!(codec, "h264", "codec defaults to h264");
            assert!(gpu_pin.is_none());
            assert!(audio.is_none());
        }
        other => panic!("expected WhipPush output, got {other:?}"),
    }
    original.validate().expect("whip_push output validates");

    let reparsed: Output =
        toml::from_str(&toml::to_string(&original).expect("serialize")).expect("re-parse");
    assert_eq!(original, reparsed, "TOML round-trip identity");

    let json = serde_json::to_string(&original).expect("serialize json");
    let from_json: Output = serde_json::from_str(&json).expect("re-parse json");
    assert_eq!(original, from_json, "JSON round-trip identity");
    assert!(json.contains("\"kind\":\"whip_push\""), "{json}");
    assert!(!json.contains("codec"), "default h264 is skipped: {json}");
}

#[test]
fn output_whip_push_label_and_id_derivation() {
    let out: Output =
        toml::from_str("kind = \"whip_push\"\nurl = \"https://[2001:db8::15]:8443/whip/pgm1\"\n")
            .expect("valid whip_push output");
    assert_eq!(
        out.label(),
        "whip_push https://[2001:db8::15]:8443/whip/pgm1"
    );
    assert_eq!(out.explicit_id(), None);
    assert_eq!(out.id(), out.label(), "id derives from label when absent");
}

#[test]
fn output_whip_push_audio_capability_is_single_track() {
    let out: Output =
        toml::from_str("kind = \"whip_push\"\nurl = \"https://[2001:db8::15]:8443/whip/pgm1\"\n")
            .expect("valid whip_push output");
    let cap = out.audio_capability();
    assert_eq!(cap.delivery, TrackDelivery::Simultaneous);
    assert_eq!(cap.discrete_capacity, TrackCapacity::AtMost(1));
}

#[test]
fn output_whip_push_http_url_accepted_https_recommended() {
    // http(s) both parse; https is RECOMMENDED but http is not an error.
    let out: Output =
        toml::from_str("kind = \"whip_push\"\nurl = \"http://[2001:db8::15]:8080/whip/pgm1\"\n")
            .expect("parses structurally");
    out.validate().expect("http whip_push url still validates");
}

#[test]
fn output_whip_push_non_http_scheme_rejected() {
    for bad in [
        "rtmp://origin.example/whip",
        "ws://origin.example/whip",
        "not a url",
    ] {
        let out: Output = toml::from_str(&format!("kind = \"whip_push\"\nurl = \"{bad}\"\n"))
            .expect("parses structurally");
        let err = out
            .validate()
            .expect_err("non-http(s) whip_push url must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("http"),
            "error should name the http(s) requirement, got: {msg}"
        );
    }
}

#[test]
fn output_whip_push_empty_url_host_rejected() {
    let out: Output =
        toml::from_str("kind = \"whip_push\"\nurl = \"https://\"\n").expect("parses structurally");
    assert!(
        out.validate().is_err(),
        "an https URL with no host must fail validation"
    );
}

#[test]
fn output_whip_push_non_h264_codec_rejected() {
    let out: Output = toml::from_str(
        "kind = \"whip_push\"\nurl = \"https://[2001:db8::15]/whip/p\"\ncodec = \"hevc\"\n",
    )
    .expect("parses structurally");
    let err = out.validate().expect_err("non-h264 codec must fail (v1)");
    assert!(
        err.to_string().contains("ADR-0049"),
        "error should cite ADR-0049, got: {err}"
    );
}

#[test]
fn output_whip_push_empty_token_rejected() {
    let out: Output = toml::from_str(
        "kind = \"whip_push\"\nurl = \"https://[2001:db8::15]/whip/p\"\ntoken = \"\"\n",
    )
    .expect("parses structurally");
    let err = out.validate().expect_err("empty token must fail");
    assert!(
        err.to_string().contains("token"),
        "error should name the token, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// The `[webrtc]` section (ADR-0048 §9)
// ---------------------------------------------------------------------------

#[test]
fn absent_webrtc_section_yields_full_defaults() {
    let cfg = MultiviewConfig::load_from_toml(BASE).expect("base parses");
    cfg.validate().expect("base validates");
    let w = &cfg.webrtc;
    assert_eq!(w.udp_port, 8189, "udp_port defaults to 8189 (ADR-0048 §4)");
    assert!(
        w.advertised_addresses.is_empty(),
        "advertised_addresses defaults to []"
    );
    assert_eq!(w.max_sessions, 64, "max_sessions defaults to 64 (§8)");
    assert_eq!(
        w.session_idle_timeout.millis(),
        30_000,
        "session_idle_timeout defaults to 30s (§8)"
    );
    assert_eq!(
        w.cors_allow_origins,
        vec!["*".to_owned()],
        "cors_allow_origins defaults to [\"*\"] (§9)"
    );
    assert_eq!(
        *w,
        WebrtcConfig::default(),
        "absent ⇒ fully-defaulted struct"
    );
}

#[test]
fn default_webrtc_section_does_not_serialize() {
    let cfg = MultiviewConfig::load_from_toml(BASE).expect("base parses");
    let toml_text = cfg.to_toml().expect("to_toml");
    assert!(
        !toml_text.contains("[webrtc]"),
        "a default-valued section must not serialize:\n{toml_text}"
    );
    let json = cfg.to_json().expect("to_json");
    assert!(
        !json.contains("\"webrtc\""),
        "a default-valued section must not serialize: {json}"
    );
}

#[test]
fn webrtc_section_roundtrips_and_skips_default_fields() {
    // Only udp_port diverges from the defaults: it alone must serialize.
    let doc = format!("{BASE}\n[webrtc]\nudp_port = 9000\n");
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parses");
    cfg.validate().expect("validates");
    assert_eq!(cfg.webrtc.udp_port, 9000);
    assert_eq!(cfg.webrtc.max_sessions, 64, "untouched fields stay default");

    let toml_text = cfg.to_toml().expect("to_toml");
    assert!(toml_text.contains("udp_port = 9000"), "{toml_text}");
    assert!(
        !toml_text.contains("max_sessions"),
        "default-valued fields must not serialize:\n{toml_text}"
    );
    assert!(
        !toml_text.contains("session_idle_timeout"),
        "default-valued fields must not serialize:\n{toml_text}"
    );

    // TOML and JSON round-trips preserve the document.
    let from_toml = MultiviewConfig::load_from_toml(&toml_text).expect("re-parse toml");
    assert_eq!(cfg, from_toml, "TOML round-trip");
    let from_json =
        MultiviewConfig::load_from_json(&cfg.to_json().expect("to_json")).expect("re-parse json");
    assert_eq!(cfg, from_json, "JSON round-trip");
}

#[test]
fn webrtc_section_full_roundtrip() {
    let doc = format!(
        "{BASE}\n[webrtc]\nudp_port = 8190\nadvertised_addresses = [\"2001:db8::15\", \
         \"192.0.2.15\"]\nmax_sessions = 32\nsession_idle_timeout = \"45s\"\n\
         cors_allow_origins = [\"https://ops.example.net\"]\n"
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parses");
    cfg.validate().expect("validates");
    assert_eq!(
        cfg.webrtc.advertised_addresses,
        vec!["2001:db8::15".to_owned(), "192.0.2.15".to_owned()],
        "IPv6 listed first (ADR-0042)"
    );
    assert_eq!(cfg.webrtc.session_idle_timeout.millis(), 45_000);

    let from_toml =
        MultiviewConfig::load_from_toml(&cfg.to_toml().expect("to_toml")).expect("re-parse");
    assert_eq!(cfg, from_toml, "TOML round-trip");
    let from_json =
        MultiviewConfig::load_from_json(&cfg.to_json().expect("to_json")).expect("re-parse");
    assert_eq!(cfg, from_json, "JSON round-trip");
}

#[test]
fn webrtc_udp_port_zero_rejected() {
    let doc = format!("{BASE}\n[webrtc]\nudp_port = 0\n");
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parses");
    let err = cfg.validate().expect_err("udp_port = 0 must fail");
    assert!(
        err.to_string().contains("udp_port"),
        "error should name udp_port, got: {err}"
    );
}

#[test]
fn webrtc_max_sessions_zero_rejected() {
    let doc = format!("{BASE}\n[webrtc]\nmax_sessions = 0\n");
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parses");
    let err = cfg.validate().expect_err("max_sessions = 0 must fail");
    assert!(
        err.to_string().contains("max_sessions"),
        "error should name max_sessions, got: {err}"
    );
}

#[test]
fn webrtc_zero_idle_timeout_rejected() {
    let doc = format!("{BASE}\n[webrtc]\nsession_idle_timeout = \"0s\"\n");
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parses");
    let err = cfg.validate().expect_err("a zero idle timeout must fail");
    assert!(
        err.to_string().contains("session_idle_timeout"),
        "error should name session_idle_timeout, got: {err}"
    );
}

#[test]
fn webrtc_advertised_addresses_accept_ips_and_hostnames() {
    let doc = format!(
        "{BASE}\n[webrtc]\nadvertised_addresses = [\"2001:db8::15\", \"192.0.2.15\", \
         \"media.example.net\"]\n"
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parses");
    cfg.validate()
        .expect("IP literals and hostnames are valid advertised addresses");
}

#[test]
fn webrtc_advertised_address_garbage_rejected() {
    // A bracketed literal is the URL form, not the bare-address form this
    // field carries; same for a port suffix or whitespace.
    for bad in [
        "[2001:db8::15]",
        "2001:db8::15]:5004",
        "not a host name",
        "",
    ] {
        let doc = format!("{BASE}\n[webrtc]\nadvertised_addresses = [\"{bad}\"]\n");
        let cfg = MultiviewConfig::load_from_toml(&doc).expect("parses");
        let err = match cfg.validate() {
            Ok(()) => panic!("advertised address {bad:?} must fail validation"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("advertised_addresses"),
            "error should name advertised_addresses, got: {err}"
        );
    }
}

#[test]
fn webrtc_empty_cors_origin_rejected() {
    let doc = format!("{BASE}\n[webrtc]\ncors_allow_origins = [\"\"]\n");
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parses");
    let err = cfg.validate().expect_err("an empty CORS origin must fail");
    assert!(
        err.to_string().contains("cors_allow_origins"),
        "error should name cors_allow_origins, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// DurationString — the explicit-unit duration form ("30s", ADR-0048 §9)
// ---------------------------------------------------------------------------

#[test]
fn duration_string_parses_explicit_units() {
    let d: DurationString = "30s".parse().expect("30s");
    assert_eq!(d.millis(), 30_000);
    let d: DurationString = "1500ms".parse().expect("1500ms");
    assert_eq!(d.millis(), 1_500);
    let d: DurationString = "2m".parse().expect("2m");
    assert_eq!(d.millis(), 120_000);
}

#[test]
fn duration_string_rejects_bare_numbers_and_floats() {
    assert!("30".parse::<DurationString>().is_err(), "unit is mandatory");
    assert!("1.5s".parse::<DurationString>().is_err(), "no floats");
    assert!("".parse::<DurationString>().is_err());
    assert!("s".parse::<DurationString>().is_err());
    assert!("30h".parse::<DurationString>().is_err(), "unknown unit");
}

#[test]
fn duration_string_rejects_non_string_toml_value() {
    // A bare TOML integer deliberately fails (the unit must be explicit).
    let doc = format!("{BASE}\n[webrtc]\nsession_idle_timeout = 30\n");
    assert!(
        MultiviewConfig::load_from_toml(&doc).is_err(),
        "a unitless timeout must fail to parse"
    );
}

// ---------------------------------------------------------------------------
// Whole-document integration: source + outputs + section together
// ---------------------------------------------------------------------------

#[test]
fn document_with_webrtc_source_outputs_and_section_validates() {
    let doc = format!(
        r#"{BASE}
[[sources]]
id = "cam-field-1"
kind = "webrtc"
token = "s3cret"

[[outputs]]
kind = "webrtc"
label = "Program WHEP"
max_viewers = 16

[[outputs]]
kind = "whip_push"
url = "https://[2001:db8::15]:8443/whip/pgm1"
token = "push-secret"

[webrtc]
udp_port = 8189
advertised_addresses = ["2001:db8::15"]
"#
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parses");
    cfg.validate().expect("the combined document validates");

    // Round-trips hold for the combined document.
    let from_toml =
        MultiviewConfig::load_from_toml(&cfg.to_toml().expect("to_toml")).expect("re-parse");
    assert_eq!(cfg, from_toml, "TOML round-trip");
    let from_json =
        MultiviewConfig::load_from_json(&cfg.to_json().expect("to_json")).expect("re-parse");
    assert_eq!(cfg, from_json, "JSON round-trip");
}

#[test]
fn webrtc_outputs_reject_multitrack_audio_selection() {
    // ADR-0049: both kinds are single-track (one Opus m-line) — a two-track
    // discrete selection is a config-time capability error.
    let doc = format!(
        r#"{BASE}
[[sources]]
id = "in_x"
kind = "bars"

[audio]
sample_rate_hz = 48000

[[audio.routes]]
input_id = "in_a"
channels = {{ kind = "stereo" }}
target_track = "talent"

[[audio.routes]]
input_id = "in_x"
channels = {{ kind = "stereo" }}
target_track = "crew"

[[outputs]]
kind = "webrtc"
label = "Two tracks"
audio = {{ mode = "tracks", tracks = ["talent", "crew"] }}
"#
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parses");
    let err = cfg
        .validate()
        .expect_err("a multitrack selection on a webrtc output must fail");
    assert!(
        err.to_string().contains("track"),
        "error should explain the track capability, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// `[webrtc].ice_servers` — STUN + TURN (ADR-0048 §5.1, ADR-T013/T014 NAT path)
// ---------------------------------------------------------------------------

#[test]
fn webrtc_ice_servers_stun_and_turn_roundtrip() {
    use multiview_config::{IceServerConfig, IceServerKindConfig};
    // IPv6-first STUN + a coturn-style ephemeral-REST TURN server. The
    // `static_auth_secret` field name is caught by the control-plane redactor.
    let doc = format!(
        "{BASE}\n[webrtc]\n\
[[webrtc.ice_servers]]\nkind = \"stun\"\nurl = \"stun:[2001:db8::53]:3478\"\n\
[[webrtc.ice_servers]]\nkind = \"turn\"\nurl = \"turn:[2001:db8::55]:3478\"\n\
username = \"multiview\"\nstatic_auth_secret = \"shared-secret\"\n"
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parses ice_servers");
    cfg.validate().expect("ice_servers validate");
    assert_eq!(cfg.webrtc.ice_servers.len(), 2);
    let stun = &cfg.webrtc.ice_servers[0];
    assert_eq!(stun.kind, IceServerKindConfig::Stun);
    assert_eq!(stun.url, "stun:[2001:db8::53]:3478");
    let turn = &cfg.webrtc.ice_servers[1];
    assert_eq!(turn.kind, IceServerKindConfig::Turn);
    assert_eq!(turn.username.as_deref(), Some("multiview"));
    assert_eq!(turn.static_auth_secret.as_deref(), Some("shared-secret"));

    // Round-trips preserve the document (the secret is plaintext in config, like
    // the rtmp/srt stream keys — the redactor strips it on export).
    let _: IceServerConfig = turn.clone();
    let from_toml =
        MultiviewConfig::load_from_toml(&cfg.to_toml().expect("to_toml")).expect("re-parse");
    assert_eq!(cfg, from_toml, "TOML round-trip with ice_servers");
}

#[test]
fn webrtc_turn_without_credentials_rejected() {
    let doc = format!(
        "{BASE}\n[webrtc]\n[[webrtc.ice_servers]]\nkind = \"turn\"\n\
url = \"turn:[2001:db8::55]:3478\"\n"
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parses structurally");
    let err = cfg
        .validate()
        .expect_err("a TURN server without credentials must fail");
    assert!(
        err.to_string().contains("credential") || err.to_string().contains("TURN"),
        "error should explain the missing TURN credential, got: {err}"
    );
}

#[test]
fn webrtc_ice_server_empty_url_rejected() {
    let doc = format!("{BASE}\n[webrtc]\n[[webrtc.ice_servers]]\nkind = \"stun\"\nurl = \"\"\n");
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parses structurally");
    assert!(
        cfg.validate().is_err(),
        "an empty ICE-server URL must fail validation"
    );
}
