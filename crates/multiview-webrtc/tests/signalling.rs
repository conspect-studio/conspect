//! Failing-first tests for the WHIP/WHEP signalling types: the resource model,
//! offer→answer→resource→DELETE round-trip, content-type/auth gating, and the
//! status-code mapping (201/204/404/405/406/409/415/503) per ADR-T014/0049.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions
)]

use multiview_webrtc::session::SessionId;
use multiview_webrtc::signalling::{
    session_resource_path, validate_offer_request, SignalKind, SignalStatus, SignalledAnswer,
};

#[test]
fn whip_resource_path_is_derived_from_source_and_session() {
    let id = SessionId::from_str("abc123");
    assert_eq!(
        session_resource_path(SignalKind::Whip, "cam-1", &id),
        "/api/v1/whip/cam-1/sessions/abc123"
    );
    assert_eq!(
        session_resource_path(SignalKind::Whep, "program", &id),
        "/api/v1/whep/program/sessions/abc123"
    );
}

#[test]
fn offer_with_wrong_content_type_is_415() {
    let err = validate_offer_request("application/json", "v=0...").unwrap_err();
    assert_eq!(err, SignalStatus::UnsupportedMediaType);
}

#[test]
fn offer_with_empty_body_is_400() {
    let err = validate_offer_request("application/sdp", "").unwrap_err();
    assert_eq!(err, SignalStatus::BadRequest);
}

#[test]
fn offer_too_large_is_413() {
    // ADR-T014 §2: SDP request bodies are capped at 64 KiB.
    let huge = "v=0\r\n".repeat(20_000);
    let err = validate_offer_request("application/sdp", &huge).unwrap_err();
    assert_eq!(err, SignalStatus::PayloadTooLarge);
}

#[test]
fn well_formed_offer_passes_validation() {
    let offer = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=rtpmap:96 H264/90000\r\n";
    assert!(validate_offer_request("application/sdp", offer).is_ok());
    // Content-type may carry parameters (`application/sdp; charset=utf-8`).
    assert!(validate_offer_request("application/sdp; charset=utf-8", offer).is_ok());
}

#[test]
fn answer_carries_201_location_and_sdp_body() {
    let id = SessionId::from_str("sess-xyz");
    let answer = SignalledAnswer::created(SignalKind::Whip, "cam-1", &id, "v=0\r\n...".to_owned());
    assert_eq!(answer.status, SignalStatus::Created);
    assert_eq!(
        answer.location.as_deref(),
        Some("/api/v1/whip/cam-1/sessions/sess-xyz")
    );
    assert_eq!(answer.content_type.as_deref(), Some("application/sdp"));
    assert!(answer.body.contains("v=0"));
}

#[test]
fn status_maps_to_http_codes() {
    assert_eq!(SignalStatus::Created.code(), 201);
    assert_eq!(SignalStatus::NoContent.code(), 204);
    assert_eq!(SignalStatus::BadRequest.code(), 400);
    assert_eq!(SignalStatus::Unauthorized.code(), 401);
    assert_eq!(SignalStatus::Forbidden.code(), 403);
    assert_eq!(SignalStatus::NotFound.code(), 404);
    assert_eq!(SignalStatus::MethodNotAllowed.code(), 405);
    assert_eq!(SignalStatus::NotAcceptable.code(), 406);
    assert_eq!(SignalStatus::Conflict.code(), 409);
    assert_eq!(SignalStatus::PayloadTooLarge.code(), 413);
    assert_eq!(SignalStatus::UnsupportedMediaType.code(), 415);
    assert_eq!(SignalStatus::ServiceUnavailable.code(), 503);
}

#[test]
fn patch_is_405_with_allow_delete_options() {
    // ADR-T014/0049: PATCH (trickle/ICE restart) is rejected 405 with the Allow
    // header — vanilla ICE, never a stub success.
    let (status, allow) = multiview_webrtc::signalling::patch_rejection();
    assert_eq!(status, SignalStatus::MethodNotAllowed);
    assert_eq!(allow, "DELETE, OPTIONS");
}
