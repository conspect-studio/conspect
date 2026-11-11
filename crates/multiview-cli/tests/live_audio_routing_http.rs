//! Audio routing applies LIVE to the running program bus (no restart).
//!
//! `PUT /api/v1/audio-routing` through the REAL bound server, wired with the
//! SAME capability the binary installs (`audio::routing_live_capability` over
//! the run's `AudioControlHandle` slot), must:
//! * answer `X-Multiview-Apply: live`;
//! * change the running bus gains once the bake consumer polls the mailbox
//!   (`apply_pending`) — include/exclude, `gain_db` and `mute` — on a short
//!   ramp (no hard step), within a few ticks;
//! * answer `restart` and leave the bus untouched for what cannot change live
//!   (a different `sample_rate_hz`, or a mixed input with no audio store).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;

use multiview_audio::{AudioFormat, AudioStore, ChannelLayout, ProgramBus};
use multiview_cli::audio::{routing_live_capability, AudioControlHandle};
use multiview_cli::control;
use multiview_config::MultiviewConfig;
use multiview_control::{command_bus, EngineStateSnapshot, LiveSourceCapability};
use multiview_core::time::Rational;
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn config() -> MultiviewConfig {
    let doc = r##"schema_version = 1
[canvas]
width = 64
height = 64
fps = "25/1"
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
[cells.source]
input_id = "in_a"
[[outputs]]
kind = "hls"
path = "/tmp/live-audio-routing.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;
    MultiviewConfig::load_from_toml(doc).expect("parse config")
}

fn stereo48() -> AudioFormat {
    AudioFormat::new(48_000, ChannelLayout::Stereo)
}

/// A store holding `seconds` of a constant `level` on both channels.
fn steady_store(level: f32, seconds: usize) -> Arc<AudioStore> {
    let store = Arc::new(AudioStore::new(stereo48(), 48_000 * (seconds + 1)));
    store
        .publish_samples(&vec![level; 48_000 * seconds * 2])
        .unwrap();
    store
}

/// A running program bus exactly as the pipeline builds it: every store on
/// the bus (cnn mixed at unity, bbc metered at zero gain), the channel map and
/// the control handle published into the run's slot.
struct Run {
    bus: ProgramBus,
    handle: Arc<AudioControlHandle>,
    slot: Arc<multiview_framestore::SwapCell<Option<Arc<AudioControlHandle>>>>,
    cnn: multiview_audio::RoutePoint,
    bbc: multiview_audio::RoutePoint,
}

fn running_bus() -> Run {
    let cnn_store = steady_store(0.2, 4);
    let bbc_store = steady_store(0.4, 4);
    let mut bus = ProgramBus::new(stereo48(), Rational::new(25, 1));
    let bbc = bus.add_source("bbc", Arc::clone(&bbc_store), 0.0);
    let cnn = bus.add_source("cnn", Arc::clone(&cnn_store), 1.0);
    let mut stores = std::collections::HashMap::new();
    stores.insert("cnn".to_owned(), cnn_store);
    stores.insert("bbc".to_owned(), bbc_store);
    let mut channels = std::collections::HashMap::new();
    channels.insert("cnn".to_owned(), cnn);
    channels.insert("bbc".to_owned(), bbc);
    let handle =
        Arc::new(AudioControlHandle::from_sources(stores, channels).with_sample_rate(48_000));
    let slot = Arc::new(multiview_framestore::SwapCell::new(Some(Arc::clone(
        &handle,
    ))));
    Run {
        bus,
        handle,
        slot,
        cnn,
        bbc,
    }
}

/// PUT a routing document over raw HTTP/1.0 against a server wired with the
/// binary's live capability over `slot`; return (status line, apply header).
async fn put_routing(
    slot: Arc<multiview_framestore::SwapCell<Option<Arc<AudioControlHandle>>>>,
    body: &serde_json::Value,
) -> (String, Option<String>) {
    #[cfg(feature = "gpu")]
    static GPU_PROBE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    std::env::set_var("MULTIVIEW_CONTROL_TOKEN", "test-secret");
    let cfg = config();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(8));
    let (commands, _command_rx) = command_bus(8);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let caps = multiview_control::LiveApplyCaps::default()
        .with_audio_routing(routing_live_capability(slot));
    // `nvidia` enables GPU capability discovery in bind_and_serve. Each test
    // creates its own server, and concurrent wgpu adapter probes can stall the
    // host GPU driver. Serialize only server startup, not HTTP requests or the
    // audio bus: the independent routing tests still run concurrently.
    #[cfg(feature = "gpu")]
    let gpu_probe = GPU_PROBE.lock().await;
    let (addr, server, _state) = control::bind_and_serve(
        "[::1]:0",
        &cfg,
        publisher,
        commands,
        multiview_control::no_preview(),
        None, // switcher absent (legacy path)
        None, // switcher lease owner absent
        None,
        None,
        None,
        None,
        None,
        caps,
        LiveSourceCapability::synthetic_only(),
        None,
        None, // source_events
        async move {
            let _ = shutdown_rx.await;
        },
    )
    .await
    .expect("control server binds");
    #[cfg(feature = "gpu")]
    drop(gpu_probe);
    let body = serde_json::to_string(body).unwrap();
    let req = format!(
        "PUT /api/v1/audio-routing HTTP/1.0\r\nHost: {addr}\r\nAuthorization: Bearer admin.test-secret\r\nIf-Match: W/\"1\"\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    let response = String::from_utf8_lossy(&buf).into_owned();
    let _ = shutdown_tx.send(());
    let _ = server.await;
    let status = response.lines().next().unwrap_or_default().to_owned();
    let header = response
        .lines()
        .take_while(|l| !l.trim().is_empty())
        .find_map(|l| {
            let (name, value) = l.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("x-multiview-apply")
                .then(|| value.trim().to_owned())
        });
    (status, header)
}

/// Swap the mix: drop cnn (0.2), bring bbc (0.4) in at +3 dB (≈0.565), so a
/// hard step would jump the program by ≈0.36 in one sample.
fn swap_doc(rate: u32) -> serde_json::Value {
    serde_json::json!({
        "sample_rate_hz": rate,
        "routes": [
            { "input_id": "cnn", "channels": { "kind": "stereo" }, "include_in_program_bus": false },
            { "input_id": "bbc", "channels": { "kind": "stereo" }, "include_in_program_bus": true, "gain_db": 3.0 }
        ]
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_discrete_track_defers_only_tracks_and_changes_audible_program() {
    let mut run = running_bus();
    let live_tracks = Arc::new(multiview_framestore::SwapCell::from_pointee(
        Vec::<String>::new(),
    ));
    let startup: multiview_config::AudioRouting =
        serde_json::from_value(serde_json::json!({"sample_rate_hz": 48000, "routes": []}))
            .expect("startup routing");
    let handle = Arc::new(
        AudioControlHandle::from_sources(
            std::collections::HashMap::new(),
            std::collections::HashMap::from([
                ("cnn".to_owned(), run.cnn),
                ("bbc".to_owned(), run.bbc),
            ]),
        )
        .with_startup(Some(&startup), ["cnn".to_owned(), "bbc".to_owned()])
        .with_declared_tracks(Some(&startup), Arc::clone(&live_tracks)),
    );
    run.slot.store(Some(Arc::clone(&handle)));
    run.handle = handle;
    let before = run.bus.tick_to(1);
    assert!((before.interleaved()[0] - 0.2).abs() < 1e-4);
    let mut doc = swap_doc(48_000);
    doc["routes"][1]["target_track"] = serde_json::json!("bbc-clean");
    let (status, header) = put_routing(Arc::clone(&run.slot), &doc).await;
    assert!(status.contains(" 200"), "{status}");
    assert_eq!(header.as_deref(), Some("partial"));
    assert!(
        live_tracks.load().is_empty(),
        "new output track must not rebind encoder"
    );
    assert!(run.handle.apply_pending(&mut run.bus) >= 2);
    for tick in 2..=4 {
        let frame = run.bus.tick_to(tick);
        if tick == 4 {
            let expected = 0.4 * 10f64.powf(3.0 / 20.0);
            let actual = f64::from(*frame.interleaved().last().unwrap());
            assert!(
                (actual - expected).abs() < 1e-3,
                "audible program {actual} != {expected}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_routing_put_changes_the_running_bus_gains_live_and_click_free() {
    let mut run = running_bus();
    // Before: the program carries cnn (0.2) only.
    let before = run.bus.tick_to(1);
    assert!((before.interleaved()[0] - 0.2).abs() < 1e-4);

    let (status, header) = put_routing(Arc::clone(&run.slot), &swap_doc(48_000)).await;
    assert!(status.contains(" 200"), "PUT must succeed, got {status}");
    assert_eq!(
        header.as_deref(),
        Some("live"),
        "the running bus can take it live"
    );

    // The bake consumer polls the mailbox before its next tick.
    assert!(
        run.handle.apply_pending(&mut run.bus) >= 2,
        "both gains applied"
    );
    let bbc_target = 10f64.powf(3.0 / 20.0);
    assert!(
        run.bus.program_gain(run.cnn).unwrap().abs() < 1e-9,
        "cnn excluded"
    );
    assert!((run.bus.program_gain(run.bbc).unwrap() - bbc_target).abs() < 1e-6);

    // The change is RAMPED (no hard step) and settled within 2 ticks (80 ms).
    let mut samples: Vec<f32> = before.interleaved().to_vec();
    for tick in 2..=4 {
        samples.extend_from_slice(run.bus.tick_to(tick).interleaved());
    }
    let left: Vec<f32> = samples.iter().step_by(2).copied().collect();
    let max_step = left
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0f32, f32::max);
    assert!(
        max_step < 0.01,
        "a gain change must ramp, never step (max sample delta {max_step})"
    );
    let settled = f64::from(*left.last().unwrap());
    let expected = 0.4 * bbc_target;
    assert!(
        (settled - expected).abs() < 1e-3,
        "after the ramp the program is bbc at +3 dB only: got {settled}, want {expected}"
    );
}

/// A source added while the program runs joins the RUNNING bus: at zero gain
/// until routed, then a routing PUT mixing it answers `live` and the bus gains
/// change. Removing it takes its bus point away (no restart in either step).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_source_added_at_runtime_routes_live_and_removal_drops_its_bus_point() {
    let mut run = running_bus();
    let strips_before = run.bus.live_strip_count();
    let live_store = steady_store(0.3, 4);

    // The hub registers the new source's store; the bake consumer picks it up.
    assert!(run
        .handle
        .set_source_audio("msnow", Some(Arc::clone(&live_store))));
    run.handle.apply_pending(&mut run.bus);
    assert_eq!(
        run.bus.live_strip_count(),
        strips_before + 1,
        "the running bus gained the source"
    );
    let unrouted = run.bus.tick_to(1);
    assert!(
        (unrouted.interleaved()[0] - 0.2).abs() < 1e-4,
        "a new source joins at ZERO gain: the program is still cnn alone"
    );

    let doc = serde_json::json!({
        "sample_rate_hz": 48_000,
        "routes": [
            { "input_id": "cnn", "channels": { "kind": "stereo" }, "include_in_program_bus": false },
            { "input_id": "msnow", "channels": { "kind": "stereo" }, "include_in_program_bus": true }
        ]
    });
    let (status, header) = put_routing(Arc::clone(&run.slot), &doc).await;
    assert!(status.contains(" 200"), "PUT must succeed, got {status}");
    assert_eq!(header.as_deref(), Some("live"));
    assert!(run.handle.apply_pending(&mut run.bus) >= 1);
    for tick in 2..=4 {
        run.bus.tick_to(tick);
    }
    let settled = run.bus.tick_to(5);
    assert!(
        (settled.interleaved()[0] - 0.3).abs() < 1e-3,
        "the program now carries the runtime source, got {}",
        settled.interleaved()[0]
    );

    // Removal: the bus point goes (after a click-free fade), and a route
    // naming it no longer applies live.
    assert!(run.handle.remove_source("msnow"));
    run.handle.apply_pending(&mut run.bus);
    for tick in 6..=9 {
        run.bus.tick_to(tick);
    }
    assert_eq!(run.bus.live_strip_count(), strips_before);
    let (_, header) = put_routing(Arc::clone(&run.slot), &doc).await;
    assert_eq!(header.as_deref(), Some("restart"));
}

/// A source with no audio path (solid colour, NDI on this build) exists on the
/// run: routing it is applied live as silence. A restart would change nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn routing_an_existing_source_without_audio_is_live() {
    let run = running_bus();
    assert!(run.handle.set_source_audio("slate", None));
    let doc = serde_json::json!({
        "sample_rate_hz": 48_000,
        "routes": [
            { "input_id": "slate", "channels": { "kind": "stereo" }, "include_in_program_bus": true }
        ]
    });
    let (_, header) = put_routing(Arc::clone(&run.slot), &doc).await;
    assert_eq!(header.as_deref(), Some("live"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mute_applies_live() {
    let mut run = running_bus();
    let doc = serde_json::json!({
        "sample_rate_hz": 48_000,
        "routes": [
            { "input_id": "cnn", "channels": { "kind": "stereo" }, "include_in_program_bus": true, "mute": true },
            { "input_id": "bbc", "channels": { "kind": "stereo" }, "include_in_program_bus": true }
        ]
    });
    let (_, header) = put_routing(Arc::clone(&run.slot), &doc).await;
    assert_eq!(header.as_deref(), Some("live"));
    run.handle.apply_pending(&mut run.bus);
    assert!(
        run.bus.program_gain(run.cnn).unwrap().abs() < 1e-9,
        "cnn muted"
    );
    assert!((run.bus.program_gain(run.bbc).unwrap() - 1.0).abs() < 1e-9);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_cannot_change_live_says_restart_and_leaves_the_bus_alone() {
    // A different working sample rate needs a new bus.
    let mut run = running_bus();
    let (status, header) = put_routing(Arc::clone(&run.slot), &swap_doc(44_100)).await;
    assert!(status.contains(" 200"), "still stored, got {status}");
    assert_eq!(header.as_deref(), Some("restart"));
    assert_eq!(run.handle.apply_pending(&mut run.bus), 0);
    assert!((run.bus.program_gain(run.cnn).unwrap() - 1.0).abs() < 1e-9);

    // Mixing an input with no audio store on this run needs a restart.
    let mut run = running_bus();
    let doc = serde_json::json!({
        "sample_rate_hz": 48_000,
        "routes": [
            { "input_id": "ndi1", "channels": { "kind": "stereo" }, "include_in_program_bus": true }
        ]
    });
    let (_, header) = put_routing(Arc::clone(&run.slot), &doc).await;
    assert_eq!(header.as_deref(), Some("restart"));
    assert_eq!(run.handle.apply_pending(&mut run.bus), 0);
    assert!((run.bus.program_gain(run.cnn).unwrap() - 1.0).abs() < 1e-9);

    // No program bus running (audio off): empty slot → restart.
    let empty = Arc::new(multiview_framestore::SwapCell::empty());
    let (_, header) = put_routing(empty, &swap_doc(48_000)).await;
    assert_eq!(header.as_deref(), Some("restart"));
}
