//! Tests for the SRT Access-Control stream-id typed grammar (SRT-1).
//!
//! The `#!::key=value` "Standard" syntax is parsed into a typed
//! [`StreamIdParams`] value (`TryFrom` + exhaustive `match`, never
//! string-munging), with reserved `u`/`r`/`h`/`s`/`t`/`m` keys (`m=request`
//! default), namespaced `user_*`/`multiview_*` extensions, and unparseable ids
//! rejected with `SRT_REJX_BAD_REQUEST` (1400). Pure (no sockets, no
//! features); runs in the DEFAULT build.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_input::srt::{
    SrtAcMode, SrtAcType, SrtConfig, SrtError, SrtMode, SrtRejection, StreamId, StreamIdParams,
};

use proptest::prelude::*;

/// Parsing the full reserved-key set with explicit `m`.
#[test]
fn srt_ac_parses_reserved_keys() {
    let params: StreamIdParams = "#!::u=alice,r=cam1,h=ingest.example,s=sess1,t=stream,m=publish"
        .try_into()
        .expect("parse");
    assert_eq!(params.user().as_deref(), Some("alice"));
    assert_eq!(params.resource().as_deref(), Some("cam1"));
    assert_eq!(params.host().as_deref(), Some("ingest.example"));
    assert_eq!(params.session().as_deref(), Some("sess1"));
    assert_eq!(params.stream_type(), Some(SrtAcType::Stream));
    assert_eq!(params.mode(), SrtAcMode::Publish);
    assert!(params.extensions().is_empty());
}

/// `m` defaults to `request` when omitted (SRT Access-Control default).
#[test]
fn srt_ac_mode_defaults_to_request() {
    let params: StreamIdParams = "#!::r=cam1".try_into().expect("parse");
    assert_eq!(params.mode(), SrtAcMode::Request);
    // The bare `#!::` prefix with no pairs is still the (empty) grammar.
    let empty: StreamIdParams = "#!::".try_into().expect("bare prefix");
    assert_eq!(empty, StreamIdParams::default());
}

/// Parsing the namespaced extension keys; single-letter/unnamespaced custom
/// keys are rejected.
#[test]
fn srt_ac_parses_namespaced_extensions_and_rejects_bare_custom_keys() {
    let params: StreamIdParams = "#!::r=cam1,user_extra=1,multiview_tile=3"
        .try_into()
        .expect("parse");
    assert_eq!(params.resource().as_deref(), Some("cam1"));
    assert_eq!(params.extensions().get("user_extra").map(String::as_str), Some("1"));
    assert_eq!(
        params.extensions().get("multiview_tile").map(String::as_str),
        Some("3")
    );

    // A single-letter non-reserved key is not a valid custom key.
    for bad in ["#!::x=1", "#!::r=cam1,q=2", "#!::other=1", "#!::user=1"] {
        let err = StreamIdParams::try_from(bad).expect_err("must reject");
        assert!(
            matches!(err, SrtError::StreamIdUnparseable(_)),
            "{bad}: {err:?}"
        );
        assert_eq!(err.rejection(), Some(SrtRejection::BadRequest));
        assert_eq!(SrtRejection::BadRequest.code(), 1400);
    }
}

/// Unparseable ids are rejected (never string-munged).
#[test]
fn srt_ac_rejects_unparseable_ids() {
    for bad in [
        "r=cam1",               // missing `#!::` prefix
        "#!r=cam1",             // wrong prefix
        "#!::r",                // no `=`
        "#!::=cam1",             // empty key
        "#!::r=cam1,r=cam2",     // duplicate key
        "#!::m=send",            // unknown mode token
        "#!::t=video",           // unknown type token
    ] {
        let err = StreamIdParams::try_from(bad).expect_err("must reject");
        assert!(
            matches!(err, SrtError::StreamIdUnparseable(_)),
            "{bad}: {err:?}"
        );
        // An invalid id maps to the precise SRT_REJX code, not a munged string.
        assert_eq!(err.rejection(), Some(SrtRejection::BadRequest));
        assert_eq!(SrtRejection::BadRequest.code(), 1400);
    }
}

/// Canonical string form + parse round-trip (typed equality).
#[test]
fn srt_ac_string_round_trip() {
    let src = "#!::u=alice,r=cam1,h=ingest.example,s=sess1,t=stream,m=publish,user_x=9";
    let params: StreamIdParams = src.try_into().expect("parse");
    let text = params.to_stream_id_string().expect("render");
    let back: StreamIdParams = text.as_str().try_into().expect("re-parse");
    assert_eq!(params, back);

    // The canonical form keeps the `#!::` prefix and the reserved keys.
    assert!(text.starts_with("#!::"));
    assert!(text.contains("r=cam1"));
    assert!(text.contains("m=publish"));
    assert!(text.contains("user_x=9"));
}

/// The typed params flow through `StreamId` and `SrtConfig::to_url` with `#`
/// kept escaped in the URL query.
#[test]
fn srt_ac_round_trips_through_to_url_with_hash_escaped() {
    let params: StreamIdParams = "#!::r=cam1,m=publish".try_into().expect("parse");
    let sid: StreamId = (&params).try_into().expect("stream id");
    assert_eq!(sid.as_str(), "#!::r=cam1,m=publish");

    let cfg = SrtConfig {
        mode: SrtMode::Caller,
        host: "encoder.example".to_owned(),
        port: 9000,
        latency_ms: 200,
        stream_id: Some(sid),
        ..SrtConfig::default()
    };
    let url = cfg.to_url().expect("url");
    // `#` stays escaped (`%23`); the raw `#` must never appear in the URL.
    assert!(url.contains("streamid=%23!::r%3Dcam1,m%3Dpublish"), "{url}");
    assert!(!url.contains('#'), "{url}");

    // And back: the rendered stream id parses to the same typed value.
    let back: StreamIdParams = cfg.stream_id.as_ref().unwrap().try_into().unwrap();
    assert_eq!(back, params);
}

/// Property: parse → render → parse is the identity for valid reserved keys
/// plus namespaced extensions.
proptest::proptest! {
    #[test]
    fn srt_ac_prop_round_trip(
        user in "[a-zA-Z0-9_-]{0,12}",
        resource in "[a-zA-Z0-9_-]{0,12}",
        host in "[a-zA-Z0-9_.-]{0,12}",
        session in "[a-zA-Z0-9_-]{0,12}",
        mode in proptest::sample::select(vec!["request", "publish", "bidirectional"]),
        ty in proptest::sample::select(vec!["stream", "file", "auth"]),
        ext_key in proptest::sample::select(vec!["user_a", "user_b", "multiview_tile", "multiview_x"]),
        ext_val in "[a-zA-Z0-9_-]{0,12}",
    ) {
        let src = format!(
            "#!::u={user},r={resource},h={host},s={session},t={ty},m={mode},{ext_key}={ext_val}"
        );
        let params: StreamIdParams = src.as_str().try_into().expect("valid grammar");
        let text = params.to_stream_id_string().expect("render");
        let back: StreamIdParams = text.as_str().try_into().expect("re-parse");
        prop_assert_eq!(params, back);
        prop_assert_eq!(params.mode().as_token(), mode);
    }
}