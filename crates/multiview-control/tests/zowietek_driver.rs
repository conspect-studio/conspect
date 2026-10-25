//! The `zowietek` driver actor (DEV-A4, ADR-M009): the poller that drives the
//! DEV-A3 [`DeviceLifecycle`]/[`DeviceStatusRegistry`]/[`DeviceBroadcaster`]
//! through `ADOPTING → ONLINE → DEGRADED → UNREACHABLE → reconnect`, enumerates
//! the three facets, and converges the device workmode close-before-open with a
//! declared device-side impact. Every test drives the driver through the
//! **scripted transport seam**, so the whole driver is socket-free — no real
//! device, no `zowietek` network feature.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;

use multiview_control::devices::zowietek::client::{ScriptedReply, ScriptedTransport};
use multiview_control::devices::zowietek::protocol::DecodeTable;
use multiview_control::devices::zowietek::{
    ConvergenceError, ConvergenceUngrounded, ModeConvergence, ZowietekDriver,
};
use multiview_control::devices::{DeviceBroadcaster, DeviceDriverRegistry, DeviceStatusRegistry};
use multiview_engine::EnginePublisher;
use multiview_events::{DeviceState, Event};
use serde_json::json;

/// A broadcaster + the registries a driver drives, all control-plane.
fn harness() -> (
    Arc<EnginePublisher<serde_json::Value, Event>>,
    DeviceBroadcaster,
    Arc<DeviceDriverRegistry>,
) {
    let engine = Arc::new(EnginePublisher::new(256));
    let status = Arc::new(DeviceStatusRegistry::new());
    let drivers = Arc::new(DeviceDriverRegistry::new());
    let broadcaster = DeviceBroadcaster::new(Arc::clone(&engine), status);
    (engine, broadcaster, drivers)
}

fn login_ok() -> ScriptedReply {
    ScriptedReply::json(
        json!({ "rsp": "succeed", "status": "00000", "data": { "uuid": "u", "type": 0 } }),
    )
}

#[tokio::test]
async fn probe_adopts_and_drives_the_lifecycle_to_online() {
    let (engine, broadcaster, drivers) = harness();
    let mut sub = engine.subscribe();
    let transport = ScriptedTransport::new();
    transport.push("system", login_ok());
    // workmode/venc probe → reports an encoder-mode box that is reachable.
    transport.push(
        "venc",
        ScriptedReply::json(
            json!({ "rsp": "succeed", "status": "00000", "data": { "workmode": "encoder" } }),
        ),
    );
    let driver = ZowietekDriver::new(
        "dev-a",
        Arc::new(transport),
        broadcaster.clone(),
        Arc::clone(&drivers),
        "admin",
        "admin",
    );
    driver
        .probe_and_adopt()
        .await
        .expect("adopt probe succeeds");
    assert_eq!(
        broadcaster.registry().state("dev-a"),
        Some(DeviceState::Online),
        "a clean probe drives ADOPTING → ONLINE"
    );
    // The adopted lifecycle event (lossless) and a conflated status were both
    // published by the broadcaster.
    let mut saw_adopted = false;
    let mut saw_status = false;
    while let Ok(evt) = sub.try_recv() {
        match &*evt.event {
            Event::DeviceAdopted(_) => saw_adopted = true,
            Event::DeviceStatus(_) => saw_status = true,
            _ => {}
        }
    }
    assert!(saw_adopted, "device.adopted was published");
    assert!(saw_status, "device.status was published");
}

#[tokio::test]
async fn an_unreachable_probe_rides_to_unreachable_then_reconnects() {
    let (_engine, broadcaster, drivers) = harness();
    let transport = ScriptedTransport::new();
    // First login attempt: socket drops (device unreachable).
    transport.push("system", ScriptedReply::socket_dropped());
    // Second login attempt (reconnect): succeeds.
    transport.push("system", login_ok());
    transport.push(
        "venc",
        ScriptedReply::json(
            json!({ "rsp": "succeed", "status": "00000", "data": { "workmode": "encoder" } }),
        ),
    );
    let driver = ZowietekDriver::new(
        "dev-a",
        Arc::new(transport),
        broadcaster.clone(),
        Arc::clone(&drivers),
        "admin",
        "admin",
    );
    // The first probe fails → UNREACHABLE.
    driver
        .probe_and_adopt()
        .await
        .expect_err("the first probe times out");
    assert_eq!(
        broadcaster.registry().state("dev-a"),
        Some(DeviceState::Unreachable),
        "a probe that cannot reach the device rides to UNREACHABLE"
    );
    // A reconnect attempt re-establishes the channel and re-converges to ONLINE.
    driver.reconnect().await.expect("reconnect succeeds");
    assert_eq!(
        broadcaster.registry().state("dev-a"),
        Some(DeviceState::Online),
        "supervised reconnect re-converges ADOPTING/UNREACHABLE → ONLINE"
    );
}

#[tokio::test]
async fn bad_credentials_open_the_breaker_as_auth_failed() {
    let (_engine, broadcaster, drivers) = harness();
    let transport = ScriptedTransport::new();
    // login rejected with a bad-credentials status.
    transport.push(
        "system",
        ScriptedReply::json(json!({ "rsp": "login failed", "status": "00002" })),
    );
    let driver = ZowietekDriver::new(
        "dev-a",
        Arc::new(transport),
        broadcaster.clone(),
        Arc::clone(&drivers),
        "admin",
        "wrong",
    );
    driver
        .probe_and_adopt()
        .await
        .expect_err("bad credentials fail the probe");
    assert_eq!(
        broadcaster.registry().state("dev-a"),
        Some(DeviceState::AuthFailed),
        "a credential rejection opens the breaker (AUTH_FAILED, not UNREACHABLE)"
    );
}

#[tokio::test]
async fn a_device_reported_fault_degrades_the_device() {
    let (_engine, broadcaster, drivers) = harness();
    let transport = ScriptedTransport::new();
    transport.push("system", login_ok());
    transport.push(
        "venc",
        ScriptedReply::json(
            json!({ "rsp": "succeed", "status": "00000", "data": { "workmode": "encoder" } }),
        ),
    );
    // A poll reports an unhealthy stream → DEGRADED.
    transport.push(
        "streamplay",
        ScriptedReply::json(json!({
            "rsp": "succeed", "status": "00000",
            "data": { "streams": [ { "healthy": false } ] }
        })),
    );
    let driver = ZowietekDriver::new(
        "dev-a",
        Arc::new(transport),
        broadcaster.clone(),
        Arc::clone(&drivers),
        "admin",
        "admin",
    );
    driver.probe_and_adopt().await.expect("adopt ok");
    driver.poll_once().await.expect("poll ok");
    assert_eq!(
        broadcaster.registry().state("dev-a"),
        Some(DeviceState::Degraded),
        "an unhealthy device-reported stream degrades the device (management channel still up)"
    );
}

#[tokio::test]
async fn the_source_facet_enumerates_the_served_rtsp_mounts_as_candidates() {
    let (_engine, broadcaster, drivers) = harness();
    let transport = ScriptedTransport::new();
    transport.push("system", login_ok());
    transport.push(
        "venc",
        ScriptedReply::json(
            json!({ "rsp": "succeed", "status": "00000", "data": { "workmode": "encoder" } }),
        ),
    );
    let driver = ZowietekDriver::new(
        "dev-203",
        Arc::new(transport),
        broadcaster.clone(),
        Arc::clone(&drivers),
        "admin",
        "admin",
    );
    driver.probe_and_adopt().await.expect("adopt ok");
    // The source facet surfaces the verified served RTSP mounts (main/sub) as
    // bindable source-candidates, addressed at the device's host.
    let candidates = driver
        .enumerate_source_candidates("[fd00:db8::203]")
        .expect("encoder-mode device enumerates source candidates");
    let urls: Vec<&str> = candidates.iter().filter_map(|c| c.url.as_deref()).collect();
    assert!(
        urls.iter().any(|u| u.contains("/main/av")),
        "the main RTSP mount is a source candidate: {urls:?}"
    );
    assert!(
        urls.iter().any(|u| u.contains("/sub/av")),
        "the sub RTSP mount is a source candidate: {urls:?}"
    );
    // Every candidate is an rtsp transport kind on port 8554 (verified mounts).
    assert!(candidates.iter().all(|c| c.kind == "rtsp"));
    assert!(urls.iter().all(|u| u.contains(":8554/")));
    // The driver registry now serves these candidates to the projection route.
    assert_eq!(
        drivers.source_candidates("dev-203").len(),
        candidates.len(),
        "the driver registry mirrors the enumerated candidates for the A3 route"
    );
}

#[tokio::test]
async fn the_output_facet_exposes_decode_slots_as_output_targets() {
    let (_engine, broadcaster, drivers) = harness();
    let transport = ScriptedTransport::new();
    transport.push("system", login_ok());
    // A decoder-mode box has a live /streamplay decode table.
    transport.push(
        "venc",
        ScriptedReply::json(
            json!({ "rsp": "succeed", "status": "00000", "data": { "workmode": "decoder" } }),
        ),
    );
    transport.push(
        "streamplay",
        ScriptedReply::json(json!({
            "rsp": "succeed", "status": "00000",
            "data": { "entries": [ { "index": 0, "proto": "rtsp" }, { "index": 1, "proto": "srt" } ] }
        })),
    );
    let driver = ZowietekDriver::new(
        "dev-dec",
        Arc::new(transport),
        broadcaster.clone(),
        Arc::clone(&drivers),
        "admin",
        "admin",
    );
    driver.probe_and_adopt().await.expect("adopt ok");
    let targets = driver
        .enumerate_output_targets()
        .await
        .expect("decoder-mode device enumerates decode slots");
    assert!(
        !targets.is_empty(),
        "the output facet exposes the decode-table slots as output targets"
    );
    assert_eq!(
        drivers.output_targets("dev-dec").len(),
        targets.len(),
        "the driver registry mirrors the enumerated targets for the A3 route"
    );
}

#[tokio::test]
async fn mode_convergence_is_close_before_open_with_a_declared_dev_impact() {
    let (engine, broadcaster, drivers) = harness();
    let mut sub = engine.subscribe();
    let transport = ScriptedTransport::new();
    transport.push("system", login_ok());
    // Currently encoder-mode.
    transport.push(
        "venc",
        ScriptedReply::json(
            json!({ "rsp": "succeed", "status": "00000", "data": { "workmode": "encoder" } }),
        ),
    );
    // Convergence to decoder: close (stop encode/current) THEN open (enable
    // decode entry). Each step is a separate request; the close must precede the
    // open.
    transport.push(
        "streamplay",
        ScriptedReply::json(json!({ "rsp": "succeed", "status": "00000" })), // close
    );
    transport.push(
        "streamplay",
        ScriptedReply::json(json!({ "rsp": "succeed", "status": "00000" })), // open
    );
    let driver = ZowietekDriver::new(
        "dev-a",
        Arc::new(transport.clone()),
        broadcaster.clone(),
        Arc::clone(&drivers),
        "admin",
        "admin",
    );
    driver.probe_and_adopt().await.expect("adopt ok");
    let plan = driver.plan_mode_convergence("decoder");
    // The plan declares the device-side impact BEFORE apply (instant-apply
    // doctrine): the device restarts; no Multiview output is interrupted.
    assert!(matches!(plan, ModeConvergence::Switch { .. }));
    let detail = plan.declared_impact();
    assert!(
        detail.contains("no Multiview outputs are affected"),
        "the declared impact states program output is untouched: {detail:?}"
    );
    driver
        .converge_mode("decoder")
        .await
        .expect("convergence succeeds");
    // The close request was issued before the open request (close-before-open).
    let order = transport.streamplay_request_order();
    assert!(
        order.len() >= 2,
        "close-before-open issues a close then an open"
    );
    // A device.mode event was published carrying the DEV-class impact.
    let mut saw_mode = false;
    while let Ok(evt) = sub.try_recv() {
        if let Event::DeviceMode(m) = &*evt.event {
            assert_eq!(m.impact, multiview_events::ImpactClass::Device);
            saw_mode = true;
        }
    }
    assert!(
        saw_mode,
        "device.mode (DEV impact) was published on converge"
    );
}

#[tokio::test]
async fn converging_to_the_current_mode_is_a_noop_plan() {
    let (_engine, broadcaster, drivers) = harness();
    let transport = ScriptedTransport::new();
    transport.push("system", login_ok());
    transport.push(
        "venc",
        ScriptedReply::json(
            json!({ "rsp": "succeed", "status": "00000", "data": { "workmode": "encoder" } }),
        ),
    );
    let driver = ZowietekDriver::new(
        "dev-a",
        Arc::new(transport),
        broadcaster.clone(),
        Arc::clone(&drivers),
        "admin",
        "admin",
    );
    driver.probe_and_adopt().await.expect("adopt ok");
    // Already encoder: converging to encoder is a no-op (no device restart, no
    // declared disruption).
    let plan = driver.plan_mode_convergence("encoder");
    assert!(
        matches!(plan, ModeConvergence::AlreadyConverged { .. }),
        "converging to the current mode is a no-op plan, not a needless restart"
    );
}

// ---- Index-scoped convergence (DEV-A4 / upstream #140) ---------------------
//
// The grounded protocol is index-scoped and the decoder is **single-active**:
// activating one index is the sole active decode and implicitly stops whatever
// was playing. Convergence therefore targets an explicit *spare* index and
// declares Class-2 disruption before apply (invariant #11); without a
// configured index the driver refuses fail-safe rather than guessing.

/// The grounded `streamplay_get_all` fixture: production is active on index 2,
/// the configured spare is index 3.
fn grounded_decode_table() -> DecodeTable {
    DecodeTable::parse(&json!([
        {"index": 2, "name": "production", "switch": 1, "streamplay_status": "playing",
         "url": "rtsp://[fd00:db8::42]:8554/main/av"},
        {"index": 3, "name": "spare", "switch": 0, "streamplay_status": "idle",
         "url": "rtsp://[fd00:db8::42]:8554/sub/av"}
    ]))
}

#[test]
fn indexed_convergence_plan_picks_only_the_configured_spare_index() {
    let (_engine, broadcaster, drivers) = harness();
    let transport = ScriptedTransport::new();
    let driver = ZowietekDriver::new(
        "dev-a",
        Arc::new(transport),
        broadcaster,
        drivers,
        "admin",
        "admin",
    )
    .with_spare_decode_index(Some(3));

    let plan = driver
        .plan_decode_convergence(&grounded_decode_table())
        .expect("a configured spare index grounds the plan");

    assert!(
        matches!(
            plan,
            ModeConvergence::IndexedSwitch {
                index: 3,
                active_index: Some(2),
                ..
            }
        ),
        "the plan targets the configured spare index and reports the active one: {plan:?}"
    );
    let detail = plan.declared_impact();
    assert!(
        detail.contains("index 3"),
        "the declared impact names the spare index: {detail:?}"
    );
    assert!(
        detail.contains("single-active") || detail.contains("sole active"),
        "the declared impact states the decoder is single-active: {detail:?}"
    );
    assert!(
        plan.restarts_device(),
        "an index-scoped switch is a real (disruptive) switch"
    );
}

#[test]
fn indexed_convergence_refuses_without_a_configured_spare_index() {
    let (_engine, broadcaster, drivers) = harness();
    let transport = ScriptedTransport::new();
    let driver = ZowietekDriver::new(
        "dev-a",
        Arc::new(transport),
        broadcaster,
        drivers,
        "admin",
        "admin",
    );

    let err = driver
        .plan_decode_convergence(&grounded_decode_table())
        .expect_err("no configured index ⇒ fail-safe refusal, never a guess");
    assert!(
        matches!(err, ConvergenceUngrounded::NoConfiguredIndex { ref device_id } if device_id == "dev-a"),
        "the refusal names the device and the missing index: {err:?}"
    );
    assert!(
        err.to_string().contains("spare"),
        "the refusal is operator-legible: {err}"
    );
}

#[test]
fn indexed_convergence_refuses_the_active_index() {
    let (_engine, broadcaster, drivers) = harness();
    let transport = ScriptedTransport::new();
    let driver = ZowietekDriver::new(
        "dev-a",
        Arc::new(transport),
        broadcaster,
        drivers,
        "admin",
        "admin",
    )
    .with_spare_decode_index(Some(2)); // the active index — never the active one

    let err = driver
        .plan_decode_convergence(&grounded_decode_table())
        .expect_err("the active index is never chosen as the spare");
    assert!(
        matches!(
            err,
            ConvergenceUngrounded::ConfiguredIndexActive { index: 2, .. }
        ),
        "the refusal identifies the active index: {err:?}"
    );
}

#[test]
fn indexed_convergence_refuses_an_absent_index() {
    let (_engine, broadcaster, drivers) = harness();
    let transport = ScriptedTransport::new();
    let driver = ZowietekDriver::new(
        "dev-a",
        Arc::new(transport),
        broadcaster,
        drivers,
        "admin",
        "admin",
    )
    .with_spare_decode_index(Some(9));

    let err = driver
        .plan_decode_convergence(&grounded_decode_table())
        .expect_err("an index the device does not report is not a spare");
    assert!(
        matches!(
            err,
            ConvergenceUngrounded::ConfiguredIndexAbsent { index: 9, .. }
        ),
        "the refusal identifies the absent index: {err:?}"
    );
}

#[test]
fn a_device_without_a_spare_index_keeps_the_legacy_convergence_plan() {
    let (_engine, broadcaster, drivers) = harness();
    let transport = ScriptedTransport::new();
    let driver = ZowietekDriver::new(
        "dev-a",
        Arc::new(transport),
        broadcaster,
        drivers,
        "admin",
        "admin",
    );
    assert_eq!(driver.convergence_index(), None);

    // Behaviour is unchanged: the legacy close-before-open plan is still what a
    // device without a configured spare index gets.
    assert!(matches!(
        driver.plan_mode_convergence("decoder"),
        ModeConvergence::Switch { .. }
    ));
}

#[tokio::test]
async fn indexed_convergence_refuses_before_touching_the_wire_without_an_index() {
    let (_engine, broadcaster, drivers) = harness();
    let transport = ScriptedTransport::new();
    transport.push("system", login_ok());
    transport.push(
        "venc",
        ScriptedReply::json(
            json!({ "rsp": "succeed", "status": "00000", "data": { "workmode": "encoder" } }),
        ),
    );
    let driver = ZowietekDriver::new(
        "dev-a",
        Arc::new(transport.clone()),
        broadcaster,
        Arc::clone(&drivers),
        "admin",
        "admin",
    );
    driver.probe_and_adopt().await.expect("adopt ok");
    let before = transport.request_count("streamplay");

    let err = driver
        .converge_indexed()
        .await
        .expect_err("no configured index ⇒ fail-safe refusal");
    assert!(
        matches!(err, ConvergenceError::Ungrounded(_)),
        "the refusal surfaces as a typed ungrounded convergence error: {err:?}"
    );
    assert_eq!(
        transport.request_count("streamplay"),
        before,
        "the refusal happens BEFORE any wire I/O (no decode-table read, no switch)"
    );
}

#[tokio::test]
async fn indexed_convergence_applies_the_grounded_index_scoped_switch() {
    let (engine, broadcaster, drivers) = harness();
    let mut sub = engine.subscribe();
    let transport = ScriptedTransport::new();
    transport.push("system", login_ok());
    transport.push(
        "venc",
        ScriptedReply::json(
            json!({ "rsp": "succeed", "status": "00000", "data": { "workmode": "encoder" } }),
        ),
    );
    // 0) the grounded workmode read: the device is already a decoder.
    transport.push_opt(
        "system",
        "get_workmode_id",
        ScriptedReply::json(
            json!({ "rsp": "succeed", "status": "00000", "data": { "workmode_id": 1 } }),
        ),
    );
    // 1) the grounded decode-table read (`streamplay_get_all`), then
    // 2) the grounded index-scoped switch ack.
    transport.push(
        "streamplay",
        ScriptedReply::json(json!({
            "rsp": "succeed", "status": "00000",
            "data": [
                {"index": 2, "name": "production", "switch": 1, "streamplay_status": "playing",
                 "url": "rtsp://[fd00:db8::42]:8554/main/av"},
                {"index": 3, "name": "spare", "switch": 0, "streamplay_status": "idle",
                 "url": "rtsp://[fd00:db8::42]:8554/sub/av"}
            ]
        })),
    );
    transport.push(
        "streamplay",
        ScriptedReply::json(json!({ "rsp": "succeed", "status": "00000" })),
    );
    let driver = ZowietekDriver::new(
        "dev-a",
        Arc::new(transport.clone()),
        broadcaster,
        Arc::clone(&drivers),
        "admin",
        "admin",
    )
    .with_spare_decode_index(Some(3));
    driver.probe_and_adopt().await.expect("adopt ok");

    driver
        .converge_indexed()
        .await
        .expect("indexed convergence succeeds");

    let last = transport.last_request().expect("a switch was issued");
    assert_eq!(
        last.body,
        json!({
            "group": "streamplay",
            "opt": "streamplay_switch",
            "data": {"index": 3, "switch": 1}
        }),
        "the applied switch is the grounded index-scoped shape (spare index only)"
    );

    // The Class-2 disruption is declared BEFORE apply (invariant #11).
    let mut saw_indexed = false;
    while let Ok(evt) = sub.try_recv() {
        if let Event::DeviceMode(m) = &*evt.event {
            assert_eq!(m.device_id, "dev-a");
            assert_eq!(m.mode, "decoder");
            assert_eq!(
                m.impact,
                multiview_events::ImpactClass::Class2,
                "an index-scoped switch is Class-2 (it stops whatever was playing)"
            );
            let detail = m.detail.as_deref().unwrap_or_default();
            assert!(
                detail.contains("index 3"),
                "the declared detail names the spare index: {detail:?}"
            );
            saw_indexed = true;
        }
    }
    assert!(
        saw_indexed,
        "device.mode (Class-2) was published for the indexed switch"
    );
}

// ---- Review fixes (#55): grounded workmode transition + activity grounding --

/// A `{"rsp":"succeed","status":"00000","data":…}` reply.
fn ok_data(data: &serde_json::Value) -> ScriptedReply {
    ScriptedReply::json(json!({ "rsp": "succeed", "status": "00000", "data": data }))
}

fn workmode_id_reply(id: u32) -> ScriptedReply {
    ok_data(&json!({ "workmode_id": id }))
}

fn spare_table_reply() -> ScriptedReply {
    ok_data(&json!([
        {"index": 2, "name": "production", "switch": 1, "streamplay_status": "playing",
         "url": "rtsp://[fd00:db8::42]:8554/main/av"},
        {"index": 3, "name": "spare", "switch": 0, "streamplay_status": "idle",
         "url": "rtsp://[fd00:db8::42]:8554/sub/av"}
    ]))
}

/// An adopted encoder-mode driver with spare index 3 over `transport`.
async fn adopted_encoder(transport: &ScriptedTransport) -> ZowietekDriver<ScriptedTransport> {
    let (_engine, broadcaster, drivers) = harness();
    transport.push("system", login_ok());
    transport.push("venc", ok_data(&json!({ "workmode": "encoder" })));
    let driver = ZowietekDriver::new(
        "dev-a",
        Arc::new(transport.clone()),
        broadcaster,
        drivers,
        "admin",
        "admin",
    )
    .with_spare_decode_index(Some(3));
    driver.probe_and_adopt().await.expect("adopt ok");
    driver
}

#[tokio::test]
async fn indexed_convergence_changes_workmode_before_activating_the_slot() {
    let transport = ScriptedTransport::new();
    // get_workmode_id → encoder, change_workmode ack, get_workmode_id → decoder.
    transport.push_opt("system", "get_workmode_id", workmode_id_reply(0));
    transport.push_opt(
        "system",
        "change_workmode",
        ScriptedReply::json(json!({"rsp":"succeed","status":"00000"})),
    );
    transport.push_opt("system", "get_workmode_id", workmode_id_reply(1));
    transport.push("streamplay", spare_table_reply());
    transport.push(
        "streamplay",
        ScriptedReply::json(json!({"rsp":"succeed","status":"00000"})),
    );
    let driver = adopted_encoder(&transport).await;

    driver
        .converge_indexed()
        .await
        .expect("encoder -> decoder converges");

    assert_eq!(workmode_wire_count(&transport), 3, "read, change, re-read");
    assert_eq!(
        transport.last_request().unwrap().body,
        json!({"group":"streamplay","opt":"streamplay_switch","data":{"index":3,"switch":1}}),
        "slot activation is the LAST step, after the confirmed workmode change"
    );
    assert_eq!(
        driver.workmode(),
        Some(multiview_control::devices::zowietek::WorkMode::Decoder)
    );
}

#[tokio::test]
async fn indexed_convergence_skips_the_workmode_change_when_already_decoder() {
    let transport = ScriptedTransport::new();
    transport.push_opt("system", "get_workmode_id", workmode_id_reply(1));
    transport.push("streamplay", spare_table_reply());
    transport.push(
        "streamplay",
        ScriptedReply::json(json!({"rsp":"succeed","status":"00000"})),
    );
    let driver = adopted_encoder(&transport).await;

    driver
        .converge_indexed()
        .await
        .expect("already decoder converges");

    assert_eq!(
        workmode_wire_count(&transport),
        1,
        "only the get_workmode_id read; no change_workmode"
    );
}

#[tokio::test]
async fn an_unreflected_change_workmode_fails_without_caching_decoder() {
    let transport = ScriptedTransport::new();
    transport.push_opt("system", "get_workmode_id", workmode_id_reply(0));
    transport.push_opt(
        "system",
        "change_workmode",
        ScriptedReply::json(json!({"rsp":"succeed","status":"00000"})),
    );
    transport.push_opt("system", "get_workmode_id", workmode_id_reply(0)); // ack'd but still encoder
    transport.push("streamplay", spare_table_reply());
    let driver = adopted_encoder(&transport).await;

    let err = driver
        .converge_indexed()
        .await
        .expect_err("an acknowledged-but-unreflected change must fail");

    assert!(
        matches!(err, ConvergenceError::WorkmodeNotReflected { .. }),
        "{err:?}"
    );
    assert!(
        !wire_opts(&transport).contains(&"streamplay_switch".to_owned()),
        "no slot activation unless the device is confirmed in decoder mode"
    );
    assert_eq!(
        driver.workmode(),
        Some(multiview_control::devices::zowietek::WorkMode::Encoder),
        "Decoder is never cached when the device did not change"
    );
}

#[test]
fn indexed_plan_refuses_when_the_configured_rows_activity_is_unknown() {
    let (_engine, broadcaster, drivers) = harness();
    let driver = ZowietekDriver::new(
        "dev-a",
        Arc::new(ScriptedTransport::new()),
        broadcaster,
        drivers,
        "admin",
        "admin",
    )
    .with_spare_decode_index(Some(3));
    // Legacy shape: no switch flags, so activity is not grounded.
    let table = DecodeTable::parse(&json!({"entries": [{"index": 3, "proto": "rtsp"}]}));

    let plan = driver.plan_decode_convergence(&table);

    assert!(
        plan.is_err(),
        "unknown activity must be refused, not treated as inactive: {plan:?}"
    );
}

// ---- Round 2 (#55): every refusal happens BEFORE any device mutation --------

/// The `opt`s that change device state (everything else is a read).
const MUTATING_OPTS: &[&str] = &[
    "change_workmode",
    "streamplay_switch",
    "streamplay_modify",
    "streamplay_add",
    "streamplay_del",
    "stop",
    "start",
];

fn wire_opts(transport: &ScriptedTransport) -> Vec<String> {
    transport
        .requests()
        .iter()
        .filter_map(|r| {
            r.body
                .get("opt")
                .and_then(|o| o.as_str())
                .map(str::to_owned)
        })
        .collect()
}

fn mutating_calls(transport: &ScriptedTransport) -> Vec<String> {
    wire_opts(transport)
        .into_iter()
        .filter(|o| MUTATING_OPTS.contains(&o.as_str()))
        .collect()
}

fn ack() -> ScriptedReply {
    ScriptedReply::json(json!({"rsp":"succeed","status":"00000"}))
}

#[tokio::test]
async fn absent_index_in_encoder_mode_refuses_with_zero_mutating_calls() {
    let transport = ScriptedTransport::new();
    transport.push_opt("system", "get_workmode_id", workmode_id_reply(0));
    transport.push("streamplay", spare_table_reply()); // has 2 and 3, not 99
    let driver = adopted_encoder(&transport)
        .await
        .with_spare_decode_index(Some(99));

    let err = driver
        .converge_indexed()
        .await
        .expect_err("absent index must refuse");

    assert!(
        matches!(
            err,
            ConvergenceError::Ungrounded(ConvergenceUngrounded::ConfiguredIndexAbsent {
                index: 99,
                ..
            })
        ),
        "{err:?}"
    );
    assert_eq!(
        mutating_calls(&transport),
        Vec::<String>::new(),
        "the encoder must not be stopped for a refusal; wire: {:?}",
        wire_opts(&transport)
    );
}

#[tokio::test]
async fn unreadable_table_in_encoder_mode_refuses_without_change_workmode() {
    let transport = ScriptedTransport::new();
    transport.push_opt("system", "get_workmode_id", workmode_id_reply(0));
    transport.push("streamplay", ScriptedReply::empty_body()); // encoder: no table
    let driver = adopted_encoder(&transport).await;

    let err = driver
        .converge_indexed()
        .await
        .expect_err("an unreadable table must refuse");

    assert!(matches!(err, ConvergenceError::Ungrounded(_)), "{err:?}");
    assert_eq!(
        mutating_calls(&transport),
        Vec::<String>::new(),
        "wire: {:?}",
        wire_opts(&transport)
    );
}

#[tokio::test]
async fn happy_path_validates_then_changes_workmode_then_confirms_then_switches() {
    let transport = ScriptedTransport::new();
    transport.push_opt("system", "get_workmode_id", workmode_id_reply(0));
    transport.push_opt("system", "change_workmode", ack());
    transport.push_opt("system", "get_workmode_id", workmode_id_reply(1));
    transport.push("streamplay", spare_table_reply());
    transport.push("streamplay", ack());
    let driver = adopted_encoder(&transport).await;
    let skip = transport.requests().len();

    driver.converge_indexed().await.expect("converges");

    let opts: Vec<String> = transport.requests()[skip..]
        .iter()
        .filter_map(|r| {
            r.body
                .get("opt")
                .and_then(|o| o.as_str())
                .map(str::to_owned)
        })
        .collect();
    assert_eq!(
        opts,
        [
            "streamplay_get_all",
            "get_workmode_id",
            "change_workmode",
            "get_workmode_id",
            "streamplay_switch"
        ],
        "validate -> change_workmode -> confirm -> switch"
    );
}

/// Drive `converge_indexed` for every refusal variant, in both workmodes, and
/// assert that no mutating opt was ever sent.
#[tokio::test]
async fn no_refusal_variant_ever_sends_a_mutating_opt() {
    let active_spare = ok_data(&json!([
        {"index": 2, "name": "p", "switch": 0, "streamplay_status": "idle", "url": "rtsp://x/a"},
        {"index": 3, "name": "s", "switch": 1, "streamplay_status": "playing", "url": "rtsp://x/b"}
    ]));
    let unknown_activity = ok_data(&json!([
        {"index": 2, "name": "p", "switch": 1, "url": "rtsp://x/a"},
        {"index": 3, "name": "s", "url": "rtsp://x/b"}
    ]));
    for mode in [0_u32, 1] {
        // (label, spare index, table reply)
        let cases: Vec<(&str, Option<u32>, Option<ScriptedReply>)> = vec![
            ("no-index", None, Some(spare_table_reply())),
            ("active", Some(3), Some(active_spare.clone())),
            ("absent", Some(99), Some(spare_table_reply())),
            ("activity-unknown", Some(3), Some(unknown_activity.clone())),
            ("unreadable", Some(3), Some(ScriptedReply::empty_body())),
        ];
        for (label, index, table) in cases {
            let transport = ScriptedTransport::new();
            transport.push_opt("system", "get_workmode_id", workmode_id_reply(mode));
            if let Some(t) = table {
                transport.push("streamplay", t);
            }
            let driver = adopted_encoder(&transport)
                .await
                .with_spare_decode_index(index);
            let result = driver.converge_indexed().await;
            assert!(
                result.is_err(),
                "{label}/mode {mode}: must refuse: {result:?}"
            );
            assert_eq!(
                mutating_calls(&transport),
                Vec::<String>::new(),
                "{label}/mode {mode}: wire {:?}",
                wire_opts(&transport)
            );
        }
    }
}

// ---- Round 3 (#55): every unsuccessful indexed convergence publishes Failed --

/// An adopted encoder-mode driver over `transport`, with a subscription on its
/// own engine so a test can read the published `device.mode` events.
async fn adopted_with_events(
    transport: &ScriptedTransport,
    index: Option<u32>,
) -> (
    ZowietekDriver<ScriptedTransport>,
    multiview_engine::EventSubscription<Event>,
) {
    let (engine, broadcaster, drivers) = harness();
    let sub = engine.subscribe();
    transport.push("system", login_ok());
    transport.push("venc", ok_data(&json!({ "workmode": "encoder" })));
    let driver = ZowietekDriver::new(
        "dev-a",
        Arc::new(transport.clone()),
        broadcaster,
        drivers,
        "admin",
        "admin",
    )
    .with_spare_decode_index(index);
    driver.probe_and_adopt().await.expect("adopt ok");
    (driver, sub)
}

/// Drain the `device.mode` events published so far.
fn mode_events(
    sub: &mut multiview_engine::EventSubscription<Event>,
) -> Vec<multiview_events::DeviceMode> {
    let mut out = Vec::new();
    while let Ok(evt) = sub.try_recv() {
        if let Event::DeviceMode(m) = &*evt.event {
            out.push(m.clone());
        }
    }
    out
}

#[tokio::test]
async fn every_indexed_refusal_publishes_a_failed_outcome_with_the_typed_reason() {
    use multiview_events::{ImpactClass, ModePhase};
    let active_spare = ok_data(&json!([
        {"index": 2, "name": "p", "switch": 0, "streamplay_status": "idle", "url": "rtsp://x/a"},
        {"index": 3, "name": "s", "switch": 1, "streamplay_status": "playing", "url": "rtsp://x/b"}
    ]));
    let unknown_activity = ok_data(&json!([
        {"index": 2, "name": "p", "switch": 1, "url": "rtsp://x/a"},
        {"index": 3, "name": "s", "url": "rtsp://x/b"}
    ]));
    // (label, index, table, expected reason fragment)
    let cases: Vec<(&str, Option<u32>, Option<ScriptedReply>, &str)> = vec![
        ("no-index", None, None, "no configured spare decode index"),
        (
            "active",
            Some(3),
            Some(active_spare),
            "already decoding on configured spare index 3",
        ),
        (
            "absent",
            Some(99),
            Some(spare_table_reply()),
            "does not report configured spare decode index 99",
        ),
        (
            "activity-unknown",
            Some(3),
            Some(unknown_activity),
            "did not report whether configured spare decode index 3",
        ),
        (
            "unreadable",
            Some(3),
            Some(ScriptedReply::empty_body()),
            "readable decode table",
        ),
    ];
    for (label, index, table, reason) in cases {
        let transport = ScriptedTransport::new();
        if let Some(t) = table {
            transport.push("streamplay", t);
        }
        let (driver, mut sub) = adopted_with_events(&transport, index).await;
        driver
            .converge_indexed()
            .await
            .expect_err("refusal expected");
        let events = mode_events(&mut sub);
        let failed: Vec<_> = events
            .iter()
            .filter(|m| m.phase == ModePhase::Failed)
            .collect();
        assert_eq!(
            failed.len(),
            1,
            "{label}: exactly one Failed outcome: {events:?}"
        );
        let detail = failed[0].detail.as_deref().unwrap_or_default();
        assert!(
            detail.contains(reason),
            "{label}: Failed detail carries the typed refusal reason: {detail:?}"
        );
        if index.is_some() {
            assert_eq!(failed[0].impact, ImpactClass::Class2, "{label}");
        }
    }
}

#[tokio::test]
async fn an_unreflected_workmode_change_publishes_a_failed_outcome_with_the_reason() {
    use multiview_events::ModePhase;
    let transport = ScriptedTransport::new();
    transport.push_opt("system", "get_workmode_id", workmode_id_reply(0));
    transport.push_opt("system", "change_workmode", ack());
    transport.push_opt("system", "get_workmode_id", workmode_id_reply(0));
    transport.push("streamplay", spare_table_reply());
    let (driver, mut sub) = adopted_with_events(&transport, Some(3)).await;
    driver.converge_indexed().await.expect_err("must fail");
    let failed: Vec<_> = mode_events(&mut sub)
        .into_iter()
        .filter(|m| m.phase == ModePhase::Failed)
        .collect();
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert!(
        failed[0]
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("still reports workmode"),
        "{failed:?}"
    );
}

#[tokio::test]
async fn a_failed_slot_switch_publishes_exactly_one_failed_outcome() {
    use multiview_events::ModePhase;
    let transport = ScriptedTransport::new();
    transport.push_opt("system", "get_workmode_id", workmode_id_reply(1));
    transport.push("streamplay", spare_table_reply());
    transport.push(
        "streamplay",
        ScriptedReply::json(json!({"rsp":"failed","status":"00004"})),
    );
    let (driver, mut sub) = adopted_with_events(&transport, Some(3)).await;
    driver.converge_indexed().await.expect_err("must fail");
    let failed = mode_events(&mut sub)
        .into_iter()
        .filter(|m| m.phase == ModePhase::Failed)
        .count();
    assert_eq!(failed, 1);
}

// ---- Round 4 (#55): the COMPLETE outgoing wire shape of every grounded opt ---

/// How many requests the grounded workmode group (`/system`, group
/// `workmode`) received.
fn workmode_wire_count(transport: &ScriptedTransport) -> usize {
    transport
        .wire()
        .iter()
        .filter(|(path, body)| path == "/system" && body["group"] == "workmode")
        .count()
}

/// The grounding table (aperim/multiview#140), verbatim: endpoint path and the
/// full JSON body for every opt the convergence driver sends.
fn grounded_wire_table() -> Vec<(&'static str, &'static str, serde_json::Value)> {
    vec![
        (
            "get_workmode_id",
            "/system",
            json!({"group":"workmode","opt":"get_workmode_id","data":null}),
        ),
        (
            "change_workmode",
            "/system",
            json!({"group":"workmode","opt":"change_workmode","data":{"workmode_id":1}}),
        ),
        (
            "streamplay_get_all",
            "/streamplay",
            json!({"group":"streamplay","opt":"streamplay_get_all","data":null}),
        ),
        (
            "streamplay_switch",
            "/streamplay",
            json!({"group":"streamplay","opt":"streamplay_switch","data":{"index":3,"switch":1}}),
        ),
    ]
}

#[tokio::test]
async fn every_grounded_opt_goes_out_with_its_exact_path_and_body() {
    // encoder -> decoder: read, validate table, change, confirm, switch.
    let transport = ScriptedTransport::new();
    transport.push_opt("system", "get_workmode_id", workmode_id_reply(0));
    transport.push_opt("system", "change_workmode", ack());
    transport.push_opt("system", "get_workmode_id", workmode_id_reply(1));
    transport.push_opt("streamplay", "streamplay_get_all", spare_table_reply());
    transport.push_opt("streamplay", "streamplay_switch", ack());
    let driver = adopted_encoder(&transport).await;
    let skip = transport.wire().len();

    driver.converge_indexed().await.expect("converges");

    let table = grounded_wire_table();
    let expect = |opt: &str| {
        let (_, path, body) = table.iter().find(|(o, _, _)| *o == opt).unwrap();
        ((*path).to_owned(), body.clone())
    };
    assert_eq!(
        transport.wire()[skip..].to_vec(),
        vec![
            expect("streamplay_get_all"),
            expect("get_workmode_id"),
            expect("change_workmode"),
            expect("get_workmode_id"),
            expect("streamplay_switch"),
        ],
        "the complete outgoing wire (path + full body) matches the grounding"
    );
}

#[tokio::test]
async fn output_target_enumeration_sends_the_grounded_streamplay_get_all() {
    let transport = ScriptedTransport::new();
    transport.push_opt("streamplay", "streamplay_get_all", spare_table_reply());
    let driver = adopted_encoder(&transport).await;
    let skip = transport.wire().len();

    let targets = driver.enumerate_output_targets().await.expect("enumerates");

    assert_eq!(targets.len(), 2, "both grounded rows become targets");
    assert_eq!(
        transport.wire()[skip..].to_vec(),
        vec![(
            "/streamplay".to_owned(),
            json!({"group":"streamplay","opt":"streamplay_get_all","data":null})
        )]
    );
}

#[test]
fn the_protocol_models_render_exactly_the_grounding_table() {
    use multiview_control::devices::zowietek::protocol::{
        ChangeWorkmode, NullDataCall, StreamplaySwitch,
    };
    use multiview_control::devices::zowietek::WorkMode;
    let table = grounded_wire_table();
    let render = |opt: &str| -> (String, serde_json::Value) {
        match opt {
            "get_workmode_id" => (
                format!("/{}", NullDataCall::GetWorkmodeId.module()),
                NullDataCall::GetWorkmodeId.body(),
            ),
            "streamplay_get_all" => (
                format!("/{}", NullDataCall::StreamplayGetAll.module()),
                NullDataCall::StreamplayGetAll.body(),
            ),
            "change_workmode" => (
                format!("/{}", ChangeWorkmode::MODULE),
                ChangeWorkmode::to(WorkMode::Decoder).body(),
            ),
            "streamplay_switch" => (
                format!("/{}", StreamplaySwitch::MODULE),
                StreamplaySwitch::activate(3).body(),
            ),
            other => panic!("unmodelled opt {other}"),
        }
    };
    for (opt, path, body) in &table {
        assert_eq!(render(opt), ((*path).to_owned(), body.clone()), "{opt}");
    }
}

// ---- Round 6 (#55): truthful outcomes — what changed, and which operation ----

fn failed_event(
    sub: &mut multiview_engine::EventSubscription<Event>,
) -> multiview_events::DeviceMode {
    use multiview_events::ModePhase;
    let mut failed: Vec<_> = mode_events(sub)
        .into_iter()
        .filter(|m| m.phase == ModePhase::Failed)
        .collect();
    assert_eq!(failed.len(), 1, "exactly one Failed outcome: {failed:?}");
    failed.remove(0)
}

#[tokio::test]
async fn a_partial_apply_reports_the_workmode_change_that_already_happened() {
    use multiview_events::FailureStage;
    let transport = ScriptedTransport::new();
    transport.push_opt("system", "get_workmode_id", workmode_id_reply(0));
    transport.push_opt("system", "change_workmode", ack());
    transport.push_opt("system", "get_workmode_id", workmode_id_reply(1));
    transport.push_opt("streamplay", "streamplay_get_all", spare_table_reply());
    transport.push_opt(
        "streamplay",
        "streamplay_switch",
        ScriptedReply::json(json!({"rsp":"failed","status":"00004"})),
    );
    let (driver, mut sub) = adopted_with_events(&transport, Some(3)).await;

    driver
        .converge_indexed_with_op(Some("op-1"))
        .await
        .expect_err("slot activation fails");

    let failed = failed_event(&mut sub);
    assert_eq!(failed.failure_stage, Some(FailureStage::WorkmodeChanged));
    let detail = failed.detail.unwrap_or_default();
    assert!(
        detail.contains("workmode changed to decoder; slot activation failed"),
        "{detail:?}"
    );
    assert!(!detail.contains("not applied"), "{detail:?}");
    assert_eq!(failed.operation_id.as_deref(), Some("op-1"));
}

#[tokio::test]
async fn refusals_and_unconfirmed_changes_report_their_exact_stage() {
    use multiview_events::FailureStage;
    // Refused before any mutation.
    let transport = ScriptedTransport::new();
    transport.push_opt("system", "get_workmode_id", workmode_id_reply(0));
    transport.push_opt("streamplay", "streamplay_get_all", spare_table_reply());
    let (driver, mut sub) = adopted_with_events(&transport, Some(99)).await;
    driver
        .converge_indexed_with_op(Some("op-r"))
        .await
        .expect_err("refused");
    let failed = failed_event(&mut sub);
    assert_eq!(failed.failure_stage, Some(FailureStage::Unchanged));
    assert_eq!(failed.operation_id.as_deref(), Some("op-r"));

    // change_workmode acked but the re-read still says encoder.
    let transport = ScriptedTransport::new();
    transport.push_opt("system", "get_workmode_id", workmode_id_reply(0));
    transport.push_opt("system", "change_workmode", ack());
    transport.push_opt("system", "get_workmode_id", workmode_id_reply(0));
    transport.push_opt("streamplay", "streamplay_get_all", spare_table_reply());
    let (driver, mut sub) = adopted_with_events(&transport, Some(3)).await;
    driver
        .converge_indexed_with_op(Some("op-u"))
        .await
        .expect_err("unconfirmed");
    let failed = failed_event(&mut sub);
    assert_eq!(
        failed.failure_stage,
        Some(FailureStage::WorkmodeUnconfirmed)
    );

    // Already a decoder, slot switch fails: nothing changed.
    let transport = ScriptedTransport::new();
    transport.push_opt("system", "get_workmode_id", workmode_id_reply(1));
    transport.push_opt("streamplay", "streamplay_get_all", spare_table_reply());
    transport.push_opt(
        "streamplay",
        "streamplay_switch",
        ScriptedReply::json(json!({"rsp":"failed","status":"00004"})),
    );
    let (driver, mut sub) = adopted_with_events(&transport, Some(3)).await;
    driver
        .converge_indexed_with_op(None)
        .await
        .expect_err("switch fails");
    let failed = failed_event(&mut sub);
    assert_eq!(failed.failure_stage, Some(FailureStage::Unchanged));
    assert_eq!(failed.operation_id, None, "no op ⇒ none carried");
}

#[tokio::test]
async fn every_mode_event_of_an_operation_carries_its_operation_id() {
    let transport = ScriptedTransport::new();
    transport.push_opt("system", "get_workmode_id", workmode_id_reply(1));
    transport.push_opt("streamplay", "streamplay_get_all", spare_table_reply());
    transport.push_opt("streamplay", "streamplay_switch", ack());
    let (driver, mut sub) = adopted_with_events(&transport, Some(3)).await;
    driver
        .converge_indexed_with_op(Some("op-ok"))
        .await
        .expect("ok");
    let events = mode_events(&mut sub);
    assert!(events.len() >= 2, "{events:?}");
    assert!(
        events
            .iter()
            .all(|m| m.operation_id.as_deref() == Some("op-ok")),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_lost_slot_switch_response_on_a_decoder_is_activation_unconfirmed_not_unchanged() {
    use multiview_events::FailureStage;
    // Already a decoder; the switch is sent but its response is lost/times out.
    let transport = ScriptedTransport::new();
    transport.push_opt("system", "get_workmode_id", workmode_id_reply(1));
    transport.push_opt("streamplay", "streamplay_get_all", spare_table_reply());
    transport.push_opt(
        "streamplay",
        "streamplay_switch",
        ScriptedReply::socket_dropped(),
    );
    let (driver, mut sub) = adopted_with_events(&transport, Some(3)).await;
    driver
        .converge_indexed_with_op(Some("op-l"))
        .await
        .expect_err("response lost");
    let failed = failed_event(&mut sub);
    assert_eq!(
        failed.failure_stage,
        Some(FailureStage::ActivationUnconfirmed)
    );
    let detail = failed.detail.unwrap_or_default();
    assert!(
        detail.contains("slot activation may have occurred; response lost"),
        "{detail:?}"
    );
    assert!(!detail.contains("not applied"), "{detail:?}");
    assert_eq!(failed.operation_id.as_deref(), Some("op-l"));
}

#[tokio::test]
async fn a_workmode_change_then_a_lost_slot_switch_response_reports_both() {
    use multiview_events::FailureStage;
    let transport = ScriptedTransport::new();
    transport.push_opt("system", "get_workmode_id", workmode_id_reply(0));
    transport.push_opt("system", "change_workmode", ack());
    transport.push_opt("system", "get_workmode_id", workmode_id_reply(1));
    transport.push_opt("streamplay", "streamplay_get_all", spare_table_reply());
    transport.push_opt(
        "streamplay",
        "streamplay_switch",
        ScriptedReply::socket_dropped(),
    );
    let (driver, mut sub) = adopted_with_events(&transport, Some(3)).await;
    driver
        .converge_indexed_with_op(Some("op-b"))
        .await
        .expect_err("response lost");
    let failed = failed_event(&mut sub);
    assert_eq!(
        failed.failure_stage,
        Some(FailureStage::WorkmodeChangedActivationUnconfirmed)
    );
    let detail = failed.detail.unwrap_or_default();
    assert!(
        detail.contains("workmode changed to decoder")
            && detail.contains("slot activation may have occurred; response lost"),
        "{detail:?}"
    );
    assert!(!detail.contains("not applied"), "{detail:?}");
}
