//! The `zowietek` typed JSON-RPC-over-HTTP client (DEV-A4, ADR-M009): the
//! defensive client that survives the firmware hazards verified on real
//! `ZowieBox` units (2026-06-10). Every test drives the client through the
//! **scripted transport seam** ([`ScriptedTransport`]) so the whole client is
//! socket-free — no real device, no `zowietek` network feature needed.
//!
//! Hazards under test (managed-devices.md §3.1, corrected by the live probe):
//! lenient numeric status compare (`"00000"` and `"000000"` both succeed; the
//! human `rsp` text is never branched on); the login → uuid → logout flow; an
//! empty response body is a distinct protocol error (never parse-or-default to
//! success); the rate-limit (`00009`) / restarting (`00010`) codes back off and
//! are retried; per-device request serialization; the advisory URL query verb;
//! and the bps bitrate newtype with a kbps-shaped magnitude guard.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;
use std::time::Duration;

use multiview_control::devices::zowietek::client::{
    BitrateBps, RpcVerb, ScriptedReply, ScriptedTransport, ZowietekClient, ZowietekClientError,
    ZowietekStatus,
};
use serde_json::json;

/// A login reply body in the firmware's envelope shape.
fn login_ok(uuid: &str) -> ScriptedReply {
    ScriptedReply::json(json!({
        "rsp": "succeed",
        "status": "00000",
        "data": { "uuid": uuid, "type": 0 }
    }))
}

#[test]
fn status_success_is_lenient_about_zero_count_and_ignores_rsp_text() {
    // Both the five-zero and six-zero success forms verified on firmware are
    // success; the human `rsp` text drifts (succeed/success) and is NEVER the
    // decision input.
    assert!(ZowietekStatus::from_code("00000").is_success());
    assert!(ZowietekStatus::from_code("000000").is_success());
    // A drifted `rsp` string with a success code is still success.
    assert!(ZowietekStatus::from_parts("success", "00000").is_success());
    assert!(ZowietekStatus::from_parts("succeed", "000000").is_success());
    // A non-zero code is never success regardless of a "succeed"-looking text.
    assert!(!ZowietekStatus::from_parts("succeed", "00004").is_success());
}

#[test]
fn status_classifies_the_verified_hazard_codes() {
    assert!(ZowietekStatus::from_code("00009").is_rate_limited());
    assert!(ZowietekStatus::from_code("00010").is_restarting());
    assert!(ZowietekStatus::from_code("00004").is_workmode_unsupported());
    // A success code is none of the hazard classes.
    let ok = ZowietekStatus::from_code("00000");
    assert!(!ok.is_rate_limited() && !ok.is_restarting() && !ok.is_workmode_unsupported());
}

#[test]
fn bitrate_bps_accepts_a_real_bps_value_and_rejects_a_kbps_shaped_one() {
    // 12 Mbps as bps (verified on firmware).
    let bitrate = BitrateBps::from_field(12_000_000).expect("12 Mbps is a valid bps value");
    assert_eq!(bitrate.get(), 12_000_000);
    // A kbps-shaped value (12000 == 12 kbps) is implausibly small for a video
    // bitrate in bps and is rejected by the magnitude sanity guard, never
    // silently accepted (the doc's kbps/bps ambiguity must not corrupt the
    // schema).
    assert!(BitrateBps::from_field(12_000).is_err());
    // Zero is rejected.
    assert!(BitrateBps::from_field(0).is_err());
}

#[tokio::test]
async fn login_keeps_the_uuid_and_logout_consumes_it() {
    let transport = ScriptedTransport::new();
    // login → returns a uuid; logout → succeeds using that uuid.
    transport.push("system", login_ok("uuid-abc"));
    transport.push(
        "system",
        ScriptedReply::json(json!({ "rsp": "succeed", "status": "00000" })),
    );
    let client = ZowietekClient::new(Arc::new(transport.clone()), "admin", "admin");
    let session = client.login().await.expect("login succeeds");
    assert_eq!(session.uuid(), "uuid-abc");
    client.logout(&session).await.expect("logout succeeds");
    // The logout request body carried the session uuid (used only at logout).
    let last = transport.last_request().expect("a logout request was sent");
    assert_eq!(last.body["data"]["uuid"], "uuid-abc");
}

#[tokio::test]
async fn an_empty_response_body_is_a_distinct_protocol_error() {
    // Some getinfo shapes return an EMPTY body when the group/opt does not fit
    // the current workmode: that is a protocol error, NEVER parsed-or-defaulted
    // to success.
    let transport = ScriptedTransport::new();
    transport.push("system", login_ok("u"));
    transport.push("venc", ScriptedReply::empty_body());
    let client = ZowietekClient::new(Arc::new(transport.clone()), "admin", "admin");
    let session = client.login().await.expect("login succeeds");
    let err = client
        .get_info(&session, "venc", "venc", json!({ "ch": 0 }))
        .await
        .expect_err("an empty body must surface as an error");
    assert!(
        matches!(err, ZowietekClientError::EmptyBody { .. }),
        "empty body maps to EmptyBody, got {err:?}"
    );
}

#[tokio::test]
async fn a_rate_limit_code_backs_off_and_retries() {
    // 00009 (too fast) is retried after a backoff; the client must not surface it
    // as a hard failure on the first hit. Time is paused so the test does not
    // actually sleep.
    tokio::time::pause();
    let transport = ScriptedTransport::new();
    transport.push("system", login_ok("u"));
    // First call: rate-limited; second call (after backoff): success.
    transport.push(
        "venc",
        ScriptedReply::json(json!({ "rsp": "too fast", "status": "00009" })),
    );
    transport.push(
        "venc",
        ScriptedReply::json(json!({ "rsp": "succeed", "status": "00000", "data": { "ch": 0 } })),
    );
    let client = ZowietekClient::new(Arc::new(transport.clone()), "admin", "admin");
    let session = client.login().await.expect("login succeeds");
    let value = client
        .get_info(&session, "venc", "venc", json!({ "ch": 0 }))
        .await
        .expect("a retried call after backoff succeeds");
    assert_eq!(value["ch"], 0);
    // Two venc attempts were made (one rate-limited, one success).
    assert_eq!(transport.request_count("venc"), 2);
}

#[tokio::test]
async fn the_query_verb_is_advisory_and_the_body_group_opt_are_authoritative() {
    // Even when calling under the SET verb, the body group/opt select the
    // operation: the client sends what it was asked, the verb is advisory.
    let transport = ScriptedTransport::new();
    transport.push("system", login_ok("u"));
    transport.push(
        "vo",
        ScriptedReply::json(json!({ "rsp": "succeed", "status": "00000" })),
    );
    let client = ZowietekClient::new(Arc::new(transport.clone()), "admin", "admin");
    let session = client.login().await.expect("login succeeds");
    client
        .request(
            &session,
            RpcVerb::SetInfo,
            "vo",
            "vo",
            "set_hdmi",
            json!({ "format": "2160p30" }),
        )
        .await
        .expect("the request goes through under the advisory verb");
    let last = transport.last_request().expect("a vo request was sent");
    assert_eq!(last.verb, RpcVerb::SetInfo);
    assert_eq!(last.body["group"], "vo");
    assert_eq!(last.body["opt"], "set_hdmi");
    // login_check_flag=1 rides every URL.
    assert!(last.url.contains("login_check_flag=1"));
}

#[tokio::test]
async fn requests_to_one_device_are_serialized() {
    // The per-device serialization gate means two concurrent calls never overlap
    // on the wire: the transport records that no second request begins before the
    // first completes.
    let transport = ScriptedTransport::new();
    transport.push("system", login_ok("u"));
    transport.push(
        "venc",
        ScriptedReply::json(json!({ "rsp": "succeed", "status": "00000" }))
            .with_delay(Duration::from_millis(50)),
    );
    transport.push(
        "venc",
        ScriptedReply::json(json!({ "rsp": "succeed", "status": "00000" })),
    );
    let client = Arc::new(ZowietekClient::new(
        Arc::new(transport.clone()),
        "admin",
        "admin",
    ));
    let session = client.login().await.expect("login succeeds");
    let session = Arc::new(session);
    let c1 = Arc::clone(&client);
    let s1 = Arc::clone(&session);
    let c2 = Arc::clone(&client);
    let s2 = Arc::clone(&session);
    let (a, b) = tokio::join!(
        async move { c1.get_info(&s1, "venc", "venc", json!({ "ch": 0 })).await },
        async move { c2.get_info(&s2, "venc", "venc", json!({ "ch": 1 })).await },
    );
    a.expect("first call ok");
    b.expect("second call ok");
    // The scripted transport asserts non-overlap internally; here we confirm both
    // ran and the max observed concurrency never exceeded one.
    assert_eq!(
        transport.max_observed_concurrency(),
        1,
        "per-device requests must be serialized (never overlapping on the wire)"
    );
}

#[tokio::test]
async fn a_reboot_request_that_drops_the_socket_is_fire_and_forget() {
    // LAN/mDNS/port changes + reboot return NO HTTP response (socket drops). The
    // client's fire-and-forget path treats a transport drop as expected, NOT a
    // hard error: the caller rides the UNREACHABLE→reconnect path.
    let transport = ScriptedTransport::new();
    transport.push("system", login_ok("u"));
    transport.push("system", ScriptedReply::socket_dropped());
    let client = ZowietekClient::new(Arc::new(transport.clone()), "admin", "admin");
    let session = client.login().await.expect("login succeeds");
    let outcome = client
        .fire_and_forget(&session, "system", "reboot", "reboot", json!({}))
        .await;
    // Fire-and-forget returns Ok even when the socket drops (the device is
    // rebooting); a later probe rides reconnect.
    assert!(
        outcome.is_ok(),
        "a dropped socket on a reboot is the expected fire-and-forget outcome, got {outcome:?}"
    );
}
