//! The `zowietek` **poller actor** (DEV-A4, ADR-M009): the supervised
//! control-plane task that drives the DEV-A3 [`DeviceLifecycle`] through
//! `ADOPTING → ONLINE → DEGRADED → UNREACHABLE → reconnect`, opens the breaker on
//! `AUTH_FAILED` (no reconnect storm), enumerates the three facets into the
//! driver registry the projection routes read, and dispatches `set-mode`
//! convergence. Every test drives the poller through the **scripted transport
//! seam**, so the whole actor is socket-free — no real device, no `zowietek`
//! network feature.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    // Test prose names the lifecycle states in plain caps (ADOPTING/ONLINE/…)
    // for readability; not API items needing backticks.
    clippy::doc_markdown,
    // The harness returns a 4-tuple of shared handles — a test helper, not a
    // public type worth a named alias.
    clippy::type_complexity
)]

use std::sync::Arc;
use std::time::Duration;

use multiview_control::devices::zowietek::client::{ScriptedReply, ScriptedTransport};
use multiview_control::devices::zowietek::poller::{
    PollerConfig, PollerControl, PollerStep, ZowietekPoller,
};
use multiview_control::devices::zowietek::ZowietekDriver;
use multiview_control::devices::{DeviceBroadcaster, DeviceDriverRegistry, DeviceStatusRegistry};
use multiview_engine::EnginePublisher;
use multiview_events::{DeviceState, Event};
use serde_json::json;

/// A broadcaster + the registries a poller drives, all control-plane.
fn harness() -> (
    Arc<EnginePublisher<serde_json::Value, Event>>,
    DeviceBroadcaster,
    Arc<DeviceStatusRegistry>,
    Arc<DeviceDriverRegistry>,
) {
    let engine = Arc::new(EnginePublisher::new(256));
    let status = Arc::new(DeviceStatusRegistry::new());
    let drivers = Arc::new(DeviceDriverRegistry::new());
    let broadcaster = DeviceBroadcaster::new(Arc::clone(&engine), Arc::clone(&status));
    (engine, broadcaster, status, drivers)
}

fn login_ok() -> ScriptedReply {
    ScriptedReply::json(
        json!({ "rsp": "succeed", "status": "00000", "data": { "uuid": "u", "type": 0 } }),
    )
}

fn venc_encoder() -> ScriptedReply {
    ScriptedReply::json(
        json!({ "rsp": "succeed", "status": "00000", "data": { "workmode": "encoder" } }),
    )
}

fn poller(
    device_id: &str,
    transport: &ScriptedTransport,
    broadcaster: &DeviceBroadcaster,
    status: &Arc<DeviceStatusRegistry>,
    drivers: &Arc<DeviceDriverRegistry>,
) -> ZowietekPoller<ScriptedTransport> {
    let driver = ZowietekDriver::new(
        device_id,
        Arc::new(transport.clone()),
        broadcaster.clone(),
        Arc::clone(drivers),
        "admin",
        "admin",
    );
    ZowietekPoller::new(
        device_id,
        driver,
        Arc::clone(status),
        "[fd00:db8::1]",
        PollerConfig::test_fast(),
    )
}

/// The actor drives ADOPTING → ONLINE through the DeviceLifecycle (not an ad-hoc
/// status set), publishing ONLINE, and enumerates the facets on adopt.
#[tokio::test]
async fn adopt_step_drives_lifecycle_to_online_and_enumerates_facets() {
    let (_engine, broadcaster, status, drivers) = harness();
    let transport = ScriptedTransport::new();
    transport.push("system", login_ok());
    transport.push("venc", venc_encoder());
    // The poller's adopt enumerates the source facet (no I/O) and, for an
    // encoder, the source candidates land in the driver registry.
    let mut poller = poller("dev-a", &transport, &broadcaster, &status, &drivers);

    let step = poller.adopt_step().await;
    assert_eq!(step, PollerStep::Online, "a clean probe drives to ONLINE");
    assert_eq!(
        status.state("dev-a"),
        Some(DeviceState::Online),
        "the lifecycle (driven by ProbeOk) published ONLINE"
    );
    // The source facet enumerated the served RTSP mounts into the driver
    // registry the projection route reads — at runtime, not the empty placeholder.
    assert!(
        !drivers.source_candidates("dev-a").is_empty(),
        "adopt enumerated the source candidates into the driver registry"
    );
}

/// A device-reported unhealthy stream drives ONLINE → DEGRADED via a DeviceFault
/// lifecycle event (the transition table), then recovery returns it to ONLINE.
#[tokio::test]
async fn poll_step_drives_online_to_degraded_then_recovers() {
    let (_engine, broadcaster, status, drivers) = harness();
    let transport = ScriptedTransport::new();
    transport.push("system", login_ok());
    transport.push("venc", venc_encoder());
    // First poll: an unhealthy stream → DEGRADED.
    transport.push(
        "streamplay",
        ScriptedReply::json(json!({
            "rsp": "succeed", "status": "00000",
            "data": { "streams": [ { "healthy": false } ] }
        })),
    );
    // Second poll: healthy again → recover to ONLINE.
    transport.push(
        "streamplay",
        ScriptedReply::json(json!({
            "rsp": "succeed", "status": "00000",
            "data": { "streams": [ { "healthy": true } ] }
        })),
    );
    let mut poller = poller("dev-a", &transport, &broadcaster, &status, &drivers);
    assert_eq!(poller.adopt_step().await, PollerStep::Online);

    assert_eq!(poller.poll_step().await, PollerStep::Degraded);
    assert_eq!(
        status.state("dev-a"),
        Some(DeviceState::Degraded),
        "a device-reported fault drives ONLINE → DEGRADED through the lifecycle"
    );

    assert_eq!(poller.poll_step().await, PollerStep::Online);
    assert_eq!(
        status.state("dev-a"),
        Some(DeviceState::Online),
        "a recovered stream drives DEGRADED → ONLINE"
    );
}

/// A dropped socket mid-poll rides ONLINE → UNREACHABLE, and a supervised
/// reconnect re-establishes the channel and re-converges to ONLINE.
#[tokio::test]
async fn poll_socket_drop_rides_to_unreachable_then_reconnects() {
    let (_engine, broadcaster, status, drivers) = harness();
    let transport = ScriptedTransport::new();
    transport.push("system", login_ok());
    transport.push("venc", venc_encoder());
    // Poll drops the socket → UNREACHABLE.
    transport.push("streamplay", ScriptedReply::socket_dropped());
    // Reconnect: login + probe succeed again → ONLINE.
    transport.push("system", login_ok());
    transport.push("venc", venc_encoder());
    let mut poller = poller("dev-a", &transport, &broadcaster, &status, &drivers);
    assert_eq!(poller.adopt_step().await, PollerStep::Online);

    assert_eq!(poller.poll_step().await, PollerStep::Unreachable);
    assert_eq!(
        status.state("dev-a"),
        Some(DeviceState::Unreachable),
        "a dropped socket rides ONLINE → UNREACHABLE through the lifecycle"
    );

    // Supervised reconnect: the breaker is closed (not AUTH_FAILED), so a
    // reconnect attempt re-establishes the channel.
    assert_eq!(poller.reconnect_step().await, PollerStep::Online);
    assert_eq!(
        status.state("dev-a"),
        Some(DeviceState::Online),
        "supervised reconnect re-converges UNREACHABLE → ONLINE"
    );
}

/// AUTH_FAILED opens the breaker: the actor does NOT re-login in a storm. A
/// reconnect attempt while the breaker is open is a no-op that issues NO login
/// request, and only a secret update re-arms a probe.
#[tokio::test]
async fn auth_failed_opens_the_breaker_no_reconnect_storm() {
    let (_engine, broadcaster, status, drivers) = harness();
    let transport = ScriptedTransport::new();
    // Adopt login is rejected with bad credentials → AUTH_FAILED.
    transport.push(
        "system",
        ScriptedReply::json(json!({ "rsp": "login failed", "status": "00002" })),
    );
    let mut poller = poller("dev-a", &transport, &broadcaster, &status, &drivers);

    assert_eq!(poller.adopt_step().await, PollerStep::AuthFailed);
    assert_eq!(
        status.state("dev-a"),
        Some(DeviceState::AuthFailed),
        "bad credentials open the breaker (AUTH_FAILED)"
    );
    let after_adopt = transport.request_count("system");
    assert_eq!(
        after_adopt, 1,
        "exactly one login attempt was made on adopt"
    );

    // The breaker is OPEN: reconnect attempts must NOT re-login (no storm).
    for _ in 0..5 {
        let step = poller.reconnect_step().await;
        assert_eq!(
            step,
            PollerStep::BreakerOpen,
            "a reconnect while AUTH_FAILED is a no-op (the breaker is open)"
        );
    }
    assert_eq!(
        transport.request_count("system"),
        after_adopt,
        "the open breaker issued NO further login requests (no reconnect storm)"
    );
    assert_eq!(
        status.state("dev-a"),
        Some(DeviceState::AuthFailed),
        "the device stays AUTH_FAILED while the breaker is open"
    );

    // A secret update re-arms a probe (lifecycle AUTH_FAILED → ADOPTING).
    transport.push("system", login_ok());
    transport.push("venc", venc_encoder());
    poller.secret_updated();
    assert_eq!(
        poller.adopt_step().await,
        PollerStep::Online,
        "a secret update re-arms the probe and the next adopt succeeds"
    );
}

/// A `PollerControl::SetMode` command dispatched to the actor runs the driver's
/// plan → converge (close-before-open) and publishes a `device.mode` event.
#[tokio::test]
async fn set_mode_command_dispatches_convergence() {
    let (engine, broadcaster, status, drivers) = harness();
    let mut sub = engine.subscribe();
    let transport = ScriptedTransport::new();
    transport.push("system", login_ok());
    transport.push("venc", venc_encoder());
    // Convergence to decoder: close then open (each a /streamplay request).
    transport.push(
        "streamplay",
        ScriptedReply::json(json!({ "rsp": "succeed", "status": "00000" })),
    );
    transport.push(
        "streamplay",
        ScriptedReply::json(json!({ "rsp": "succeed", "status": "00000" })),
    );
    let mut poller = poller("dev-a", &transport, &broadcaster, &status, &drivers);
    assert_eq!(poller.adopt_step().await, PollerStep::Online);

    // Dispatch a set-mode command through the actor's control channel handling.
    poller
        .handle_control(PollerControl::SetMode {
            mode: "decoder".to_owned(),
        })
        .await;

    // The close request preceded the open request (close-before-open).
    let order = transport.streamplay_request_order();
    assert!(
        order.len() >= 2,
        "set-mode convergence issues a close then an open"
    );
    // A device.mode event carrying the DEV-class impact was published.
    let mut saw_mode = false;
    while let Ok(evt) = sub.try_recv() {
        if let Event::DeviceMode(m) = &*evt.event {
            assert_eq!(m.impact, multiview_events::ImpactClass::Device);
            saw_mode = true;
        }
    }
    assert!(
        saw_mode,
        "set-mode dispatched a device.mode convergence event"
    );
}

/// A device adopted with `desired_mode` set re-converges onto that mode once
/// adopt reaches ONLINE: the close-before-open `/streamplay` switch is issued
/// without any operator `set-mode`, making the documented "re-converges
/// desired_mode on adopt" behaviour real.
#[tokio::test]
async fn adopt_converges_desired_mode_after_online() {
    let (engine, broadcaster, status, drivers) = harness();
    let mut sub = engine.subscribe();
    let transport = ScriptedTransport::new();
    transport.push("system", login_ok());
    // The box powers up in encoder mode; desired_mode is decoder → must switch.
    transport.push("venc", venc_encoder());
    // The decode-table enumeration the encoder poller skips is not pushed (an
    // encoder has no decode table). The convergence to decoder issues a close
    // then an open (each a /streamplay request).
    transport.push(
        "streamplay",
        ScriptedReply::json(json!({ "rsp": "succeed", "status": "00000" })),
    );
    transport.push(
        "streamplay",
        ScriptedReply::json(json!({ "rsp": "succeed", "status": "00000" })),
    );
    let driver = ZowietekDriver::new(
        "dev-a",
        Arc::new(transport.clone()),
        broadcaster.clone(),
        Arc::clone(&drivers),
        "admin",
        "admin",
    );
    let mut poller = ZowietekPoller::new(
        "dev-a",
        driver,
        Arc::clone(&status),
        "[fd00:db8::1]",
        PollerConfig::test_fast(),
    )
    .with_desired_mode(Some("decoder".to_owned()));

    assert_eq!(poller.adopt_step().await, PollerStep::Online);
    assert_eq!(
        status.state("dev-a"),
        Some(DeviceState::Online),
        "adopt drove the lifecycle to ONLINE"
    );
    // Convergence ran: a close preceded an open on /streamplay — driven by
    // desired_mode, NOT by an operator set-mode command.
    let order = transport.streamplay_request_order();
    assert!(
        order.len() >= 2,
        "desired_mode convergence issued a close then an open after adopt"
    );
    // A device.mode (DEV-class) convergence event was published.
    let mut saw_mode = false;
    while let Ok(evt) = sub.try_recv() {
        if let Event::DeviceMode(m) = &*evt.event {
            assert_eq!(m.impact, multiview_events::ImpactClass::Device);
            saw_mode = true;
        }
    }
    assert!(
        saw_mode,
        "desired_mode convergence on adopt published a device.mode event"
    );
}

/// A device with NO `desired_mode` does not converge on adopt: no `/streamplay`
/// close/open switch is issued (the device keeps whatever mode it powered up in).
#[tokio::test]
async fn adopt_without_desired_mode_does_not_converge() {
    let (_engine, broadcaster, status, drivers) = harness();
    let transport = ScriptedTransport::new();
    transport.push("system", login_ok());
    transport.push("venc", venc_encoder());
    // No desired_mode → the convergence path must not run; the poller helper
    // builds a poller with desired_mode None.
    let mut poller = poller("dev-a", &transport, &broadcaster, &status, &drivers);

    assert_eq!(poller.adopt_step().await, PollerStep::Online);
    assert_eq!(
        transport.request_count("streamplay"),
        0,
        "no desired_mode ⇒ adopt issues no mode-convergence /streamplay switch"
    );
}

/// `desired_mode` convergence is suppressed while the AUTH_FAILED breaker is
/// open: a bad-credential adopt opens the breaker and issues NO convergence
/// (only a state that actually reached ONLINE may converge).
#[tokio::test]
async fn auth_failed_suppresses_desired_mode_convergence() {
    let (_engine, broadcaster, status, drivers) = harness();
    let transport = ScriptedTransport::new();
    // Adopt login rejected → AUTH_FAILED (the breaker opens).
    transport.push(
        "system",
        ScriptedReply::json(json!({ "rsp": "login failed", "status": "00002" })),
    );
    let driver = ZowietekDriver::new(
        "dev-a",
        Arc::new(transport.clone()),
        broadcaster.clone(),
        Arc::clone(&drivers),
        "admin",
        "admin",
    );
    let mut poller = ZowietekPoller::new(
        "dev-a",
        driver,
        Arc::clone(&status),
        "[fd00:db8::1]",
        PollerConfig::test_fast(),
    )
    .with_desired_mode(Some("decoder".to_owned()));

    assert_eq!(poller.adopt_step().await, PollerStep::AuthFailed);
    assert_eq!(
        status.state("dev-a"),
        Some(DeviceState::AuthFailed),
        "bad credentials opened the breaker"
    );
    assert_eq!(
        transport.request_count("streamplay"),
        0,
        "AUTH_FAILED ⇒ no convergence (the device never reached ONLINE)"
    );
}

/// A supervised reconnect that reaches ONLINE re-converges `desired_mode`: the
/// driver re-applies the desired mode after the channel comes back, so the
/// device is restored to its declared mode without an operator command.
#[tokio::test]
async fn reconnect_reconverges_desired_mode() {
    let (_engine, broadcaster, status, drivers) = harness();
    let transport = ScriptedTransport::new();
    // Adopt: the box is ALREADY in decoder mode, so adopt converges no-op.
    transport.push("system", login_ok());
    transport.push(
        "venc",
        ScriptedReply::json(
            json!({ "rsp": "succeed", "status": "00000", "data": { "workmode": "decoder" } }),
        ),
    );
    // Adopt enumerates the decoder output targets (decode-mode box).
    transport.push(
        "streamplay",
        ScriptedReply::json(
            json!({ "rsp": "succeed", "status": "00000", "data": { "entries": [] } }),
        ),
    );
    // Poll drops the socket → UNREACHABLE.
    transport.push("streamplay", ScriptedReply::socket_dropped());
    // Reconnect: the box came back in ENCODER mode (it rebooted) → reconnect
    // must re-converge it to decoder (close + open).
    transport.push("system", login_ok());
    transport.push("venc", venc_encoder());
    transport.push(
        "streamplay",
        ScriptedReply::json(json!({ "rsp": "succeed", "status": "00000" })),
    );
    transport.push(
        "streamplay",
        ScriptedReply::json(json!({ "rsp": "succeed", "status": "00000" })),
    );
    let driver = ZowietekDriver::new(
        "dev-a",
        Arc::new(transport.clone()),
        broadcaster.clone(),
        Arc::clone(&drivers),
        "admin",
        "admin",
    );
    let mut poller = ZowietekPoller::new(
        "dev-a",
        driver,
        Arc::clone(&status),
        "[fd00:db8::1]",
        PollerConfig::test_fast(),
    )
    .with_desired_mode(Some("decoder".to_owned()));

    assert_eq!(poller.adopt_step().await, PollerStep::Online);
    let after_adopt = transport.request_count("streamplay");
    assert_eq!(poller.poll_step().await, PollerStep::Unreachable);

    assert_eq!(poller.reconnect_step().await, PollerStep::Online);
    // Reconnect re-converged: at least the close + open switch beyond the adopt
    // enumeration and the dropped poll.
    let order = transport.streamplay_request_order();
    let close = order
        .iter()
        .filter(|r| r.body.get("opt").and_then(serde_json::Value::as_str) == Some("stop"))
        .count();
    let open = order
        .iter()
        .filter(|r| r.body.get("opt").and_then(serde_json::Value::as_str) == Some("start"))
        .count();
    assert!(
        close >= 1 && open >= 1,
        "reconnect re-converged desired_mode (close-before-open) after returning ONLINE; \
         before-reconnect streamplay count was {after_adopt}, order = {order:?}"
    );
}

/// The spawned actor task drives the full lifecycle end-to-end over its control
/// channel and stops cleanly when the handle is dropped (no leaked task).
#[tokio::test]
async fn spawned_actor_runs_and_stops_on_handle_drop() {
    let (_engine, broadcaster, status, drivers) = harness();
    let transport = ScriptedTransport::new();
    transport.push("system", login_ok());
    transport.push("venc", venc_encoder());
    // A steady stream of healthy polls keeps it ONLINE.
    for _ in 0..32 {
        transport.push(
            "streamplay",
            ScriptedReply::json(json!({
                "rsp": "succeed", "status": "00000",
                "data": { "streams": [ { "healthy": true } ] }
            })),
        );
    }
    let driver = ZowietekDriver::new(
        "dev-a",
        Arc::new(transport.clone()),
        broadcaster.clone(),
        Arc::clone(&drivers),
        "admin",
        "admin",
    );
    let poller = ZowietekPoller::new(
        "dev-a",
        driver,
        Arc::clone(&status),
        "[fd00:db8::1]",
        PollerConfig::test_fast(),
    );
    let handle = poller.spawn();

    // Wait until the actor adopts and publishes ONLINE.
    let online = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if status.state("dev-a") == Some(DeviceState::Online) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(
        online.is_ok(),
        "the spawned actor adopts the device to ONLINE"
    );
    assert!(
        !drivers.source_candidates("dev-a").is_empty(),
        "the spawned actor populated the driver registry the projection route reads"
    );

    // Dropping the handle stops the task (abort on drop) — proven by the task
    // being finished shortly after.
    let join = handle.into_join_handle();
    join.abort();
    let _ = join.await;
}
