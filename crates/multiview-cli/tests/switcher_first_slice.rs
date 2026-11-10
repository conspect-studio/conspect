#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use multiview_cli::live_sources::shared_stores;
use multiview_cli::pipeline::Pipeline;
use multiview_cli::preview::{program_slot, CliPreviewProvider};
use multiview_compositor::pipeline::Nv12Image;
use multiview_config::MultiviewConfig;
use multiview_control::preview::PreviewProvider;
use multiview_control::EngineStateSnapshot;
use multiview_core::switcher::SourceRef;
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::{
    ScenePair, SwitcherCommand, SwitcherControl, SwitcherVerb,
};
use multiview_engine::{CompositorDrive, EnginePublisher, StopSignal};
use multiview_events::Event;
use std::sync::Arc;
use std::time::Duration;

/// A real run owns process-wide seams (the control-plane demand provider, the
/// source registry, libav's encoders) and spawns its own realtime clock +
/// producers, exactly like the binary — which runs ONE pipeline per process.
/// Building or running a second pipeline in one test process starves the first
/// one's preview taps, so every test here takes turns, the same way
/// `demand_lifecycle` serialises its runs.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn config(out: &std::path::Path) -> MultiviewConfig {
    let text = format!(
        r##"
schema_version=1
[canvas]
width=64
height=64
fps="25/1"
pixel_format="nv12"
background="#101014"
[canvas.color]
profile="sdr-bt709-limited"
[layout]
kind="grid"
columns=["1fr"]
rows=["1fr"]
areas=["a"]
[[sources]]
id="a"
kind="solid"
color="#101010"
[[sources]]
id="b"
kind="solid"
color="#f0f0f0"
[[cells]]
id="pgm"
area="a"
fit="fill"
[cells.source]
input_id="a"
[[outputs]]
kind="hls"
path="{}"
codec="mpeg2video"
segment_ms=1000
[switcher]
[[switcher.mix_effects]]
id="me1"
program="main"
program_source="a"
preview_source="b"
"##,
        out.display()
    );
    let cfg = MultiviewConfig::load_from_toml(&text).unwrap();
    cfg.validate().unwrap();
    cfg
}
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn attached_control_cut_changes_received_program_and_separate_pvw_monitor() {
    let _serial = SERIAL.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&dir.path().join("index.m3u8"));
    let mut pipeline = Pipeline::build(&cfg).unwrap();
    let preview = program_slot();
    let pvw = program_slot();
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
        .attach_switcher(handle.clone(), driver, Arc::clone(&pvw))
        .unwrap();
    let provider = CliPreviewProvider::new(
        Arc::clone(&preview),
        shared_stores(pipeline.preview_stores()),
    )
    .with_switcher_preview(Arc::clone(&pvw));
    let publisher = EnginePublisher::<EngineStateSnapshot, Event>::new(64);
    let stop = StopOnDrop::new();
    let relay_stop = stop.signal();
    let captured_preview = Arc::clone(&preview);
    // A failed relay assertion must not leave the output clock running (the
    // engine would never return and the whole binary would hang instead of
    // reporting the failure), so a bounded watchdog always raises the stop.
    let watchdog_stop = stop.signal();
    let watchdog = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        watchdog_stop.stop();
    });
    let relay = tokio::spawn(async move {
        for _ in 0..1500 {
            if handle.is_ready() && captured_preview.load().is_some() && pvw.load().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            handle.is_ready(),
            "real switcher produces state only after frame"
        );
        let before = captured_preview.load().unwrap().sample(8, 8).unwrap().0;
        let pvw_before = pvw.load().unwrap().sample(8, 8).unwrap().0;
        assert!(provider.switcher_preview_jpeg(80).is_some());
        assert_ne!(
            before, pvw_before,
            "PGM and PVW must be different real pictures"
        );
        let mut outcomes = handle.outcomes();
        handle
            .try_send(SwitcherCommand {
                operation_id: "cut-cli".into(),
                scene_version: 1,
                verb: SwitcherVerb::Cut,
            })
            .unwrap();
        let done = tokio::time::timeout(Duration::from_secs(4), outcomes.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(done.event.completed, "Cut reaches actual frame");
        let after = captured_preview.load().unwrap().sample(8, 8).unwrap().0;
        assert_ne!(before, after, "received program pixel changes");
        assert_eq!(after, pvw_before, "PGM now shows prior PVW");
        assert_eq!(provider.program_jpeg(80).unwrap().first(), Some(&0xff));
        relay_stop.stop();
    });
    let run_stop = stop.signal();
    let report = pipeline
        .run_until_serving(
            &run_stop,
            &publisher,
            &preview,
            |_: &mut CompositorDrive<Nv12Image>| {},
        )
        .await
        .unwrap();
    assert!(!report.faltered);
    watchdog.abort();
    relay.await.unwrap();
}

// ---------------------------------------------------------------------------
// The real operator HTTP flow over the real launcher wiring
// ---------------------------------------------------------------------------

/// Same first-slice shape, but the Preview source is `on_demand`: it is COLD at
/// boot, so only the launcher's warm-on-arm lease can prime it.
fn http_config(out: &std::path::Path) -> MultiviewConfig {
    let text = format!(
        r##"schema_version = 1
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
activation = "on_demand"
[[cells]]
id = "pgm"
area = "a"
fit = "fill"
[cells.source]
input_id = "a"
[[outputs]]
kind = "hls"
path = "{}"
codec = "mpeg2video"
segment_ms = 1000
[switcher]
[[switcher.mix_effects]]
id = "me1"
program = "main"
program_source = "a"
preview_source = "b"
"##,
        out.display()
    );
    let cfg = MultiviewConfig::load_from_toml(&text).unwrap();
    cfg.validate().unwrap();
    cfg
}

/// One raw HTTP/1.0 exchange returning the whole response (headers + body), so
/// binary JPEG payloads survive byte-for-byte.
async fn http_bytes(addr: std::net::SocketAddr, request: String) -> Vec<u8> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    buf
}

fn split_response(raw: &[u8]) -> (String, Vec<u8>) {
    match raw.windows(4).position(|window| window == b"\r\n\r\n") {
        Some(end) => (
            String::from_utf8_lossy(&raw[..end]).into_owned(),
            raw[end + 4..].to_vec(),
        ),
        None => (String::from_utf8_lossy(raw).into_owned(), Vec::new()),
    }
}

fn status_code(head: &str) -> u16 {
    head.split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0)
}

async fn get_raw(addr: std::net::SocketAddr, path: &str, auth: &str) -> (u16, Vec<u8>) {
    let raw = http_bytes(
        addr,
        format!("GET {path} HTTP/1.0\r\nHost: {addr}\r\n{auth}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    let (head, body) = split_response(&raw);
    (status_code(&head), body)
}

async fn get_json(addr: std::net::SocketAddr, path: &str, auth: &str) -> (u16, serde_json::Value) {
    let (code, body) = get_raw(addr, path, auth).await;
    let json = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    (code, json)
}

/// A monitor tap is sampled per output tick, so the endpoint answers an honest
/// `503 unavailable` whenever that tick's sample was empty. Wait (bounded) for an
/// actual picture instead of treating the first poll as authoritative.
async fn poll_picture(addr: std::net::SocketAddr, path: &str, auth: &str) -> Vec<u8> {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let (code, body) = get_raw(addr, path, auth).await;
        if code == 200 {
            return body;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{path} never served a picture (last status {code})"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Like [`poll_picture`], but for a picture that actually changed after a take.
async fn poll_changed_picture(
    addr: std::net::SocketAddr,
    path: &str,
    auth: &str,
    previous: &[u8],
) -> Vec<u8> {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let (code, body) = get_raw(addr, path, auth).await;
        if code == 200 && body != previous {
            return body;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{path} never changed after the applied take (last status {code})"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Ends the run when dropped, so a failed assertion can never leave the output
/// clock (and the runtime's blocking encode/HLS threads) alive inside the test
/// process — which would hang the whole binary instead of reporting the failure.
struct StopOnDrop(StopSignal);

impl StopOnDrop {
    fn new() -> Self {
        Self(StopSignal::new())
    }
    fn signal(&self) -> StopSignal {
        self.0.clone()
    }
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.stop();
    }
}

/// The end-to-end operator flow: a cold Preview source takes its boot lease, the
/// API exposes the switcher only after a real paired frame exists, and a Cut
/// issued over HTTP changes the picture the API serves on BOTH buses.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
// reason: one straight-line operator walk (readiness, both monitors, the HTTP
// Cut, the applied state, the changed pictures) whose value is reading it as the
// single flow an operator performs; splitting it would scatter the assertions.
async fn http_cut_after_cold_preview_lease_changes_received_program_and_monitor() {
    let _serial = SERIAL.lock().await;
    std::env::set_var("MULTIVIEW_CONTROL_TOKEN", "test-secret");
    let dir = tempfile::tempdir().unwrap();
    let cfg = http_config(&dir.path().join("index.m3u8"));
    let mut pipeline = Pipeline::build(&cfg).unwrap();
    // Exactly what the binary does before serving: the run's demand manager is
    // installed process-wide, so the route's lease and the launcher's boot lease
    // act on the same manager.
    multiview_cli::source_demand::install_control_demand(Some(Arc::new(
        multiview_cli::source_demand::ControlDemand::new(pipeline.demand(), pipeline.catalogue()),
    )));
    let attached = multiview_cli::switcher_runtime::attach_first_slice(
        &mut pipeline,
        multiview_cli::source_demand::installed_control_demand(),
    )
    .unwrap()
    .expect("the document declares an M/E");
    assert!(
        attached.boot_warning.is_none(),
        "the cold preview source must take its warm-on-arm lease at boot: {:?}",
        attached.boot_warning
    );
    let program = program_slot();
    let provider: multiview_control::SharedPreview = Arc::new(
        CliPreviewProvider::new(
            Arc::clone(&program),
            shared_stores(pipeline.preview_stores()),
        )
        .with_switcher_preview(Arc::clone(&attached.preview_slot)),
    );
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let mut terminal_events = publisher.subscribe();
    let relay = tokio::spawn(
        multiview_control::switcher_events::forward_switcher_outcomes(
            attached.control.outcomes(),
            Arc::clone(&publisher),
            attached.me_id.clone(),
            attached.program.clone(),
            Arc::clone(&attached.leases),
        ),
    );
    let (commands, _command_rx) = multiview_control::command_bus(8);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let (addr, _server, _state) = multiview_cli::control::bind_and_serve(
        "[::1]:0",
        &cfg,
        Arc::clone(&publisher),
        commands,
        provider,
        Some(attached.control.clone()),
        Some(Arc::clone(&attached.leases)),
        None, // whep
        None, // whip
        None, // whep_output
        None, // licence
        None, // mesh
        multiview_control::LiveApplyCaps::default(),
        multiview_control::LiveSourceCapability::synthetic_only(),
        None, // boot_model
        None, // source_events
        async move {
            let _ = shutdown_rx.await;
        },
    )
    .await
    .expect("control server binds");
    let stop = StopOnDrop::new();
    let run_stop = stop.signal();
    let run_publisher = Arc::clone(&publisher);
    let run_program = Arc::clone(&program);
    let run = tokio::spawn(async move {
        pipeline
            .run_until_serving(
                &run_stop,
                run_publisher.as_ref(),
                &run_program,
                |_: &mut CompositorDrive<Nv12Image>| {},
            )
            .await
    });
    let auth = "Authorization: Bearer admin.test-secret";

    // 1. Capability appears only once a genuinely paired frame exists. The cold
    //    PVW source is primed by the boot lease, so this converges on its own.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let (mut code, mut state) = (0, serde_json::Value::Null);
    while std::time::Instant::now() < deadline {
        (code, state) = get_json(addr, "/api/v1/switcher/me1", auth).await;
        if code == 200 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        code, 200,
        "the switcher is served only after real Program+Preview frames"
    );
    assert_eq!(state["program_source"], "a");
    assert_eq!(state["preview_source"], "b");

    // 2. Both monitors are real, distinct pictures before the take. A PVW JPEG
    //    here is itself the proof that the cold, on-demand source was started by
    //    the lease rather than by an operator Start.
    let pgm_before = poll_picture(addr, "/api/v1/preview/program.jpg", auth).await;
    let pvw_before = poll_picture(addr, "/api/v1/preview/switcher/me1/preview.jpg", auth).await;
    assert!(
        pvw_before.starts_with(&[0xff, 0xd8]),
        "PVW monitor is a JPEG"
    );
    assert_ne!(
        pvw_before, pgm_before,
        "PGM and PVW must be separate pictures, never one canvas served twice"
    );

    // 3. The operator Cut over the real HTTP surface answers 202, never applied.
    let cut = r#"{"me":"me1","program":"main","action":"cut"}"#;
    let raw = http_bytes(
        addr,
        format!(
            "POST /api/v1/switcher/commands/cut HTTP/1.0\r\nHost: {addr}\r\n{auth}\r\n\
             Content-Type: application/json\r\nIdempotency-Key: cut-http-1\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{cut}",
            cut.len()
        ),
    )
    .await;
    let (head, body) = split_response(&raw);
    assert_eq!(status_code(&head), 202, "{}", String::from_utf8_lossy(&raw));
    assert!(
        !String::from_utf8_lossy(&body).contains("applied"),
        "202 is a queue receipt, not a claim of on-air application"
    );
    let operation_id = serde_json::from_slice::<serde_json::Value>(&body).unwrap()["operation_id"]
        .as_str()
        .expect("accepted operation id")
        .to_owned();
    assert!(!operation_id.is_empty());
    let terminal = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let item = terminal_events
                .recv()
                .await
                .expect("live terminal event stream");
            if let Event::SwitcherOperation(outcome) = item.event.as_ref() {
                if outcome.operation_id == operation_id {
                    break outcome.clone();
                }
            }
        }
    })
    .await
    .expect("actual composited CUT publishes terminal completion");
    assert!(
        terminal.completed,
        "queued is not applied: {:?}",
        terminal.error
    );
    assert_eq!(terminal.me, "me1");
    assert_eq!(terminal.program, "main");

    // 4. The engine-owned state reflects the take at its own tick.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut applied = serde_json::Value::Null;
    while std::time::Instant::now() < deadline {
        let (code, state) = get_json(addr, "/api/v1/switcher/me1", auth).await;
        if code == 200 && state["program_source"] == "b" {
            applied = state;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        applied["program_source"], "b",
        "the Cut must reach the engine-owned state (not just be queued)"
    );
    assert_eq!(applied["preview_source"], "a", "Cut flip-flops PGM and PVW");

    // 5. The picture the API serves actually changed — on both buses.
    let pgm_after =
        poll_changed_picture(addr, "/api/v1/preview/program.jpg", auth, &pgm_before).await;
    assert_ne!(
        pgm_after, pgm_before,
        "the decoded Program picture must change on the applied take"
    );
    let pvw_after = poll_changed_picture(
        addr,
        "/api/v1/preview/switcher/me1/preview.jpg",
        auth,
        &pvw_before,
    )
    .await;
    assert_ne!(
        pvw_after, pvw_before,
        "the PVW monitor follows the flip-flop"
    );
    assert_ne!(
        pvw_after, pgm_after,
        "the monitor must remain a separate bus, never a copy of Program"
    );

    stop.signal().stop();
    let report = run.await.unwrap().unwrap();
    assert!(
        !report.faltered,
        "the output clock must not falter while the API is driven"
    );
    drop(shutdown_tx);
    relay.abort();
}

/// Six cells (the live multi-tile shape) mapped three-to-two across the three
/// declared sources: a genuinely multi-cell document.
const MULTI_CELLS: &str = r#"[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "a"
[[cells]]
id = "cell_b"
area = "b"
[cells.source]
input_id = "b"
[[cells]]
id = "cell_c"
area = "c"
[cells.source]
input_id = "c"
[[cells]]
id = "cell_d"
area = "d"
[cells.source]
input_id = "a"
[[cells]]
id = "cell_e"
area = "e"
[cells.source]
input_id = "b"
[[cells]]
id = "cell_f"
area = "f"
[cells.source]
input_id = "c"
"#;

/// Opting into `[switcher]` on a canvas this slice cannot drive fails loudly
/// instead of silently replacing the operator's multi-cell layout — and the very
/// same multi-cell document still builds unchanged when the M/E block is absent,
/// so an existing production multiview is never disturbed by the feature.
#[test]
fn opting_in_refuses_a_multi_cell_canvas_without_touching_the_legacy_path() {
    let _serial = SERIAL.blocking_lock();
    let dir = tempfile::tempdir().unwrap();
    // Six cells (the live multi-tile shape) mapped three-to-two across three
    // declared sources, so this is a genuinely multi-cell document.
    let text = format!(
        r##"schema_version = 1
[canvas]
width = 320
height = 180
fps = "25/1"
pixel_format = "nv12"
background = "#000000"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr", "1fr", "1fr"]
rows = ["1fr", "1fr"]
areas = ["a b c", "d e f"]
[[sources]]
id = "a"
kind = "solid"
color = "#101010"
[[sources]]
id = "b"
kind = "solid"
color = "#808080"
[[sources]]
id = "c"
kind = "solid"
color = "#f0f0f0"
{MULTI_CELLS}[[outputs]]
kind = "hls"
path = "{}"
codec = "mpeg2video"
segment_ms = 1000
"##,
        dir.path().join("index.m3u8").display()
    );
    let switched = format!(
        "{text}\n[switcher]\n[[switcher.mix_effects]]\nid = \"me1\"\nprogram = \"main\"\n\
         program_source = \"a\"\npreview_source = \"b\"\n"
    );
    let config = MultiviewConfig::load_from_toml(&switched).unwrap();
    config.validate().unwrap();
    let Err(error) = Pipeline::build(&config) else {
        panic!("a multi-cell canvas must not be replaced");
    };
    assert!(
        error.to_string().contains("one full-canvas source cell"),
        "typed refusal names the restriction: {error}"
    );
    let legacy = MultiviewConfig::load_from_toml(&text).unwrap();
    legacy.validate().unwrap();
    Pipeline::build(&legacy).expect("the same multi-cell document still builds without [switcher]");
}
