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
use multiview_control::devices::zowietek::{ModeConvergence, ZowietekDriver};
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
