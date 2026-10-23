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
    assert_eq!(params.user(), Some("alice"));
    assert_eq!(params.resource(), Some("cam1"));
    assert_eq!(params.host(), Some("ingest.example"));
    assert_eq!(params.session(), Some("sess1"));
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
    assert_eq!(params.resource(), Some("cam1"));
    assert_eq!(
        params.extensions().get("user_extra").map(String::as_str),
        Some("1")
    );
    assert_eq!(
        params
            .extensions()
            .get("multiview_tile")
            .map(String::as_str),
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
        "r=cam1",            // missing `#!::` prefix
        "#!r=cam1",          // wrong prefix
        "#!::r",             // no `=`
        "#!::=cam1",         // empty key
        "#!::r=cam1,r=cam2", // duplicate key
        "#!::m=send",        // unknown mode token
        "#!::t=video",       // unknown type token
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

/// The typed params flow through `StreamId` and `SrtConfig::to_url` with every
/// non-unreserved byte percent-escaped (RFC 3986), `#` included.
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
    // RFC 3986: only unreserved bytes (`ALPHA`/`DIGIT`/`-`/`.`/`_`/`~`) stay
    // literal; `#`, `!`, `:`, `=` and `,` all become `%XX`. The raw `#` must
    // never appear in the URL.
    assert!(
        url.contains("streamid=%23%21%3A%3Ar%3Dcam1%2Cm%3Dpublish"),
        "{url}"
    );
    assert!(!url.contains('#'), "{url}");

    // And back: the rendered stream id parses to the same typed value.
    let back: StreamIdParams = cfg.stream_id.as_ref().unwrap().try_into().unwrap();
    assert_eq!(back, params);
}

/// Regression (cross-vendor review): the grammar accepts any UTF-8 value, so
/// the URL encoder must percent-encode every non-unreserved byte of the
/// value's UTF-8 encoding (RFC 3986). `#!::r=café` must round-trip: decoding
/// the `streamid=` query value returns the original string, never the
/// per-byte mojibake `cafÃ©` (one Latin-1 code point per UTF-8 byte).
#[test]
fn srt_ac_url_round_trip_preserves_utf8_values() {
    let params: StreamIdParams = "#!::r=café".try_into().expect("parse");
    let sid: StreamId = (&params).try_into().expect("stream id");
    let cfg = SrtConfig {
        host: "encoder.example".to_owned(),
        port: 9000,
        stream_id: Some(sid),
        ..SrtConfig::default()
    };
    let url = cfg.to_url().expect("url");
    // `é` is UTF-8 `C3 A9`; strict percent-encoding emits one `%XX` per byte.
    assert!(url.contains("streamid=%23%21%3A%3Ar%3Dcaf%C3%A9"), "{url}");
    assert!(!url.contains('Ã'), "{url}");

    let raw = streamid_query_value(&url).expect("streamid query value");
    assert_eq!(percent_decode(raw).as_deref(), Some("#!::r=café"));
}

/// Extract the raw (still percent-encoded) `streamid=` query value from a
/// [`SrtConfig::to_url`] URL. Under RFC 3986 encoding the value can never
/// contain a literal `&` or `=`, so splitting on `&` is unambiguous.
fn streamid_query_value(url: &str) -> Option<&str> {
    let query = url.split_once('?')?.1;
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("streamid="))
}

/// Percent-decode a query value back into the string it encoded: `%XX`
/// triplets become their bytes, everything else passes through; the result
/// must be valid UTF-8, because the encoder only ever encodes a UTF-8
/// string's bytes.
fn percent_decode(value: &str) -> Option<String> {
    fn hex_val(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = hex_val(*bytes.get(i + 1)?)?;
            let lo = hex_val(*bytes.get(i + 2)?)?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

// Property: parse → render → parse is the identity for valid reserved keys
// plus namespaced extensions, and parse → `to_url()` → percent-decode the
// `streamid=` query value is the identity too (RFC 3986 percent-encoding of
// every non-unreserved byte), for ASCII and Unicode values alike — `é` (2
// UTF-8 bytes), `中` (3) and `😀` (4) exercise every sequence length.
// (Plain comment: rustdoc does not document macro invocations.)
proptest::proptest! {
    #[test]
    fn srt_ac_prop_round_trip(
        user in "[a-zA-Z0-9_é中😀-]{0,12}",
        resource in "[a-zA-Z0-9_é中😀-]{0,12}",
        host in "[a-zA-Z0-9_.-]{0,12}",
        session in "[a-zA-Z0-9_é中😀-]{0,12}",
        mode in proptest::sample::select(vec!["request", "publish", "bidirectional"]),
        ty in proptest::sample::select(vec!["stream", "file", "auth"]),
        ext_key in proptest::sample::select(vec!["user_a", "user_b", "multiview_tile", "multiview_x"]),
        ext_val in "[a-zA-Z0-9_é中😀-]{0,12}",
    ) {
        let src = format!(
            "#!::u={user},r={resource},h={host},s={session},t={ty},m={mode},{ext_key}={ext_val}"
        );
        let params: StreamIdParams = src.as_str().try_into().expect("valid grammar");
        let text = params.to_stream_id_string().expect("render");
        let back: StreamIdParams = text.as_str().try_into().expect("re-parse");
        prop_assert_eq!(params.mode().as_token(), mode);

        // URL round-trip: the `streamid=` query value, percent-decoded, is
        // the exact rendered stream id.
        let sid: StreamId = (&params).try_into().expect("stream id");
        let cfg = SrtConfig {
            host: "encoder.example".to_owned(),
            port: 9000,
            stream_id: Some(sid),
            ..SrtConfig::default()
        };
        let url = cfg.to_url().expect("url");
        let raw = streamid_query_value(&url).expect("streamid query value");
        let decoded = percent_decode(raw).expect("decodable streamid value");
        prop_assert_eq!(decoded, text);
        // And the stream id never leaks raw URL metacharacters.
        prop_assert!(!raw.contains('&'));
        prop_assert!(!raw.contains('#'));

        prop_assert_eq!(params, back);
    }
}
