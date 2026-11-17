//! Issue #464 — live source add/edit on a SWITCHER run (invariants #1, #10).
//!
//! A real `Pipeline` with a declared M/E, the real demand manager, the real
//! live-source hub with the real ingest spawner and the real command drain:
//!
//! * a NEW HLS source (served by a loopback origin) added live reaches
//!   `ready` without a restart, and an M/E take puts it on Program;
//! * a NEW `youtube` source (a fake `yt-dlp` resolving to the same origin) is
//!   catalogued and reaches `ready` without a restart (`youtube` feature);
//! * a media edit of a source OFF the buses is applied live (it restarts and
//!   is ready again on the new definition);
//! * the run never falters.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines
)]

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command as OsCommand;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_cli::control;
use multiview_cli::live_sources::{shared_stores, LiveSourceHub};
use multiview_cli::pipeline::Pipeline;
use multiview_cli::preview::program_slot;
use multiview_compositor::pipeline::Nv12Image;
use multiview_config::MultiviewConfig;
use multiview_control::{command_bus, Command, EngineStateSnapshot, OperationId};
use multiview_core::switcher::SourceRef;
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::{
    ScenePair, SwitcherCommand, SwitcherControl, SwitcherVerb,
};
use multiview_engine::{CompositorDrive, DemandPhase, EnginePublisher, StopSignal};
use multiview_events::Event;

/// A bright MPEG-TS segment long enough to stay decoding for the test.
fn generate_segment(path: &Path) -> Vec<u8> {
    let status = OsCommand::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x64:rate=25:duration=12",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "mpeg2video",
            "-f",
            "mpegts",
        ])
        .arg(path)
        .status()
        .expect("spawn ffmpeg CLI to generate the segment");
    assert!(
        status.success(),
        "ffmpeg CLI failed to generate the segment"
    );
    std::fs::read(path).unwrap()
}

/// A loopback HLS origin serving `/{name}.m3u8` (an endless-looking live
/// playlist of the one segment) and `/seg.ts` for every name.
fn hls_origin(segment: Vec<u8>) -> (u16, Arc<AtomicBool>) {
    let listener = TcpListener::bind("[::1]:0").expect("loopback HLS origin");
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    std::thread::spawn(move || {
        while !thread_stop.load(Ordering::Acquire) {
            let Ok((mut conn, _)) = listener.accept() else {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            let segment = segment.clone();
            std::thread::spawn(move || {
                conn.set_nonblocking(false).ok();
                conn.set_read_timeout(Some(Duration::from_secs(2))).ok();
                let mut request = [0_u8; 4096];
                let Ok(n) = conn.read(&mut request) else {
                    return;
                };
                let head =
                    String::from_utf8_lossy(request.get(..n).unwrap_or_default()).to_string();
                let path = head.split_whitespace().nth(1).unwrap_or_default();
                let playlist = b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:12\n#EXT-X-MEDIA-SEQUENCE:1\n#EXTINF:12.0,\nseg.ts\n#EXTINF:12.0,\nseg.ts\n#EXTINF:12.0,\nseg.ts\n";
                let is = |ext: &str| {
                    std::path::Path::new(path)
                        .extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case(ext))
                };
                let body: &[u8] = if is("m3u8") {
                    playlist
                } else if is("ts") {
                    &segment
                } else {
                    b"not found"
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = conn.write_all(response.as_bytes());
                let _ = conn.write_all(body);
            });
        }
    });
    (port, stop)
}

/// One-cell Program `a`, Preview `b` (both solid), on a switcher run.
fn config(out: &Path, yt_dlp: &Path) -> MultiviewConfig {
    let text = format!(
        r##"
schema_version = 1
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
id = "a"
kind = "solid"
color = "#101010"
[[sources]]
id = "b"
kind = "solid"
color = "#f0f0f0"
[[cells]]
id = "pgm"
area = "a"
fit = "fill"
[cells.source]
input_id = "a"
[[outputs]]
kind = "hls"
path = "{out}"
codec = "mpeg2video"
segment_ms = 1000
[system.youtube]
yt_dlp_path = "{yt}"
[switcher]
[[switcher.mix_effects]]
id = "me1"
program = "main"
program_source = "a"
preview_source = "b"
"##,
        out = out.display(),
        yt = yt_dlp.display(),
    );
    let cfg = MultiviewConfig::load_from_toml(&text).unwrap();
    cfg.validate().unwrap();
    cfg
}

/// A fake `yt-dlp -J` answering a live info-dict whose HLS master is `url`.
fn fake_yt_dlp(dir: &Path, url: &str) -> std::path::PathBuf {
    let path = dir.join("yt-dlp");
    let json = format!(
        r#"{{"live_status":"is_live","formats":[{{"protocol":"m3u8_native","manifest_url":"{url}"}}]}}"#
    );
    std::fs::write(&path, format!("#!/bin/sh\ncat <<'EOF'\n{json}\nEOF\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

fn upsert(json: serde_json::Value) -> Command {
    Command::UpsertSource {
        op: OperationId::new(),
        source: Box::new(serde_json::from_value(json).expect("valid source")),
    }
}

/// Poll `done` until it holds or `within` passes; the caller records `what`.
async fn eventually(_what: &str, within: Duration, mut done: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + within;
    while !done() {
        if Instant::now() >= end {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switcher_run_adds_and_edits_network_sources_live_and_takes_them() {
    let dir = tempfile::tempdir().unwrap();
    let segment = generate_segment(&dir.path().join("seg.ts"));
    let (port, origin_stop) = hls_origin(segment);
    let yt = fake_yt_dlp(dir.path(), &format!("http://[::1]:{port}/yt.m3u8"));
    let cfg = config(&dir.path().join("out/index.m3u8"), &yt);
    let mut pipeline = Pipeline::build(&cfg).expect("pipeline builds");

    let (handle, mut driver) = SwitcherControl::new(SwitcherState::new(
        SourceRef::new("a"),
        SourceRef::new("b"),
        vec![],
    ));
    driver.set_prime_stores(pipeline.preview_stores());
    handle
        .set_scenes(ScenePair {
            version: 1,
            program: pipeline.switcher_program_layout(),
            preview: pipeline.switcher_preview_layout("b"),
        })
        .unwrap();
    pipeline
        .attach_switcher(handle.clone(), driver, program_slot())
        .unwrap();
    // The binary's order: the demand manager exists before the hub and the
    // drain, so live adds catalogue instead of spawning outside demand.
    let demand = pipeline.demand();
    let hub = LiveSourceHub::start_with_ingest(
        pipeline.stop_registry(),
        shared_stores(pipeline.preview_stores()),
        Some(pipeline.live_ingest_spawner()),
    );
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (commands, command_rx) = command_bus(32);
    let mut drain = control::command_drain_with_live_sources_and_demand(
        command_rx,
        cfg.clone(),
        Arc::clone(&publisher),
        hub.handle(),
        pipeline.drain_demand(),
    );
    let catalogue = pipeline.catalogue();
    let stop = StopSignal::new();
    let preview = program_slot();

    let relay_stop = stop.clone();
    let watch = demand.clone();
    let relay = tokio::spawn(async move {
        let mut outcome = Vec::<String>::new();
        let mut check = |ok: bool, what: &str| {
            if !ok {
                outcome.push(what.to_owned());
            }
        };
        let ready = eventually("switcher ready", Duration::from_secs(20), || {
            handle.is_ready()
        })
        .await;
        check(ready, "switcher ready");

        // (1) A NEW hls source on a switcher run: catalogued, never refused.
        commands
            .try_submit(upsert(serde_json::json!({
                "id": "net1", "kind": "hls", "url": format!("http://[::1]:{port}/net1.m3u8")
            })))
            .unwrap();
        let catalogued = eventually("net1 catalogued", Duration::from_secs(10), || {
            catalogue.load().contains_key("net1")
        })
        .await;
        check(catalogued, "net1 catalogued live");
        // Start (what POST /sources/{id}/start does) -> ready, no restart.
        watch.start_source("net1");
        let ready = eventually("net1 ready", Duration::from_secs(20), || {
            watch.status("net1").phase == DemandPhase::Ready
        })
        .await;
        check(ready, "net1 ready without restart");

        // The M/E can take it: PVW <- net1, CUT -> Program is net1.
        let mut outcomes = handle.outcomes();
        for (op, verb) in [
            (
                "pvw-net1",
                SwitcherVerb::SelectPreview(SourceRef::new("net1")),
            ),
            ("cut-net1", SwitcherVerb::Cut),
        ] {
            handle
                .try_send(SwitcherCommand {
                    operation_id: op.into(),
                    scene_version: 1,
                    verb,
                    expect: None,
                })
                .unwrap();
            let done = tokio::time::timeout(Duration::from_secs(8), async {
                loop {
                    match outcomes.recv().await {
                        Ok(item) if item.event.operation_id == op => return Some(item.event),
                        Ok(_) => {}
                        Err(_) => return None,
                    }
                }
            })
            .await
            .ok()
            .flatten();
            let completed = done
                .as_ref()
                .is_some_and(|o| o.completed && o.error.is_none());
            check(completed, &format!("{op} completed: {done:?}"));
        }
        let on_air = eventually("Program is net1", Duration::from_secs(5), || {
            handle
                .snapshot()
                .is_some_and(|s| s.program.as_str() == "net1")
        })
        .await;
        check(on_air, "net1 on Program");

        // (2) A media edit of a source OFF the buses applies live: add net2,
        // start it, edit its URL, and it is ready again on the new definition.
        commands
            .try_submit(upsert(serde_json::json!({
                "id": "net2", "kind": "hls", "url": format!("http://[::1]:{port}/net2.m3u8")
            })))
            .unwrap();
        let catalogued = eventually("net2 catalogued", Duration::from_secs(10), || {
            catalogue.load().contains_key("net2")
        })
        .await;
        check(catalogued, "net2 catalogued");
        watch.start_source("net2");
        check(
            eventually("net2 ready", Duration::from_secs(20), || {
                watch.status("net2").phase == DemandPhase::Ready
            })
            .await,
            "net2 ready",
        );
        let before = watch.transitions();
        commands
            .try_submit(upsert(serde_json::json!({
                "id": "net2", "kind": "hls", "url": format!("http://[::1]:{port}/net2-edited.m3u8")
            })))
            .unwrap();
        check(
            eventually("net2 restarted", Duration::from_secs(20), || {
                watch.transitions() > before && watch.status("net2").phase == DemandPhase::Ready
            })
            .await,
            "net2 edited live and ready again",
        );

        // (3) A NEW youtube source (fake yt-dlp) starts live.
        #[cfg(feature = "youtube")]
        {
            commands
                .try_submit(upsert(serde_json::json!({
                    "id": "yt1", "kind": "youtube", "url": "https://www.youtube.com/watch?v=live"
                })))
                .unwrap();
            check(
                eventually("yt1 catalogued", Duration::from_secs(10), || {
                    catalogue.load().contains_key("yt1")
                })
                .await,
                "yt1 catalogued live",
            );
            watch.start_source("yt1");
            check(
                eventually("yt1 ready", Duration::from_secs(30), || {
                    watch.status("yt1").phase == DemandPhase::Ready
                })
                .await,
                "yt1 ready without restart",
            );
        }
        relay_stop.stop();
        outcome
    });

    let report = Box::pin(pipeline.run_until_serving(
        &stop,
        publisher.as_ref(),
        &preview,
        move |drive: &mut CompositorDrive<Nv12Image>| drain(drive),
    ))
    .await
    .expect("serving switcher run");
    let failures = relay.await.expect("relay");
    hub.shutdown();
    origin_stop.store(true, Ordering::Release);
    assert!(failures.is_empty(), "failed: {failures:?}");
    assert!(!report.faltered, "live adds/edits never falter the clock");
}
