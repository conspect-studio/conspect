//! The `multiview` daemon/CLI entrypoint.
//!
//! A thin shell over the [`multiview_cli`] library: it initializes structured
//! logging, parses the [`multiview_cli::cli::Cli`] grammar, and dispatches to the
//! `validate` / `run` subcommands. The user-facing report text is printed to
//! stdout here — the only place the workspace `print_stdout` ban is relaxed,
//! because this is the human-facing terminal surface, not engine or data-plane
//! code.
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    // reason: the `multiview` binary prints human-facing reports to stdout and
    // fatal startup errors (when `tracing` is unavailable) to stderr. This is
    // the terminal surface, not engine or data-plane code.
)]

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context as _;
use clap::Parser as _;
use multiview_cli::cli::{Cli, Command, NodeArgs, RunArgs, ValidateArgs};
use multiview_cli::control;
use multiview_cli::run::{RunReport, SoftwareEngine};
use multiview_cli::validate::validate_config;
use multiview_compositor::pipeline::Nv12Image;
use multiview_config::MultiviewConfig;
use multiview_control::{command_bus, EngineStateSnapshot};
#[cfg(feature = "ffmpeg")]
use multiview_engine::{ActorExit, Program, ProgramId, ProgramSet, RealtimePacer};
use multiview_engine::{CompositorDrive, EnginePublisher, StopSignal};
use multiview_events::Event;
use multiview_telemetry::tracing_init::SubscriberBuilder;

/// The debounce window for the ADR-W024 Running persister: at most one
/// `active.toml` write per window (~2 s), coalescing a burst of live changes.
const RUNNING_PERSIST_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(2);

/// The run's handle on the debounced ADR-W024 Running persister: the spawned
/// task plus the served [`multiview_control::AppState`] it persists from.
struct RunningPersist {
    /// The debounced persist task ([`multiview_control::boot_model::spawn_running_persist`]).
    task: tokio::task::JoinHandle<()>,
    /// The served control-plane state (one set of stores with the router).
    state: multiview_control::AppState,
}

impl RunningPersist {
    /// Stop the debounced persister at teardown: abort → **await the task's
    /// termination** → one final best-effort persist capturing changes
    /// younger than the debounce (review M2: the ordering restores the
    /// deterministic `.tmp` single-writer guarantee; fail-soft on error).
    async fn finish(self) {
        multiview_control::boot_model::finish_running_persist(self.task, &self.state).await;
    }
}

/// Await the operator's stop signal: `Ctrl-C` (SIGINT) **or** `SIGTERM`
/// (MINOR-D, ADR-W024 §3). A self-hosted daemon is stopped with `SIGTERM`
/// (`docker stop`, `systemctl stop`, a supervisor), so graceful teardown — the
/// final `active.toml` persist, the server drain — must run on both, not just
/// an interactive Ctrl-C. Returns the human name of whichever arrived (for the
/// log line). On non-Unix (not a deploy platform) only Ctrl-C is awaited.
async fn await_shutdown_signal() -> &'static str {
    #[cfg(unix)]
    {
        let mut term = match tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        ) {
            Ok(term) => term,
            Err(error) => {
                // Registering the SIGTERM handler failed (rare); fall back to
                // Ctrl-C alone rather than never stopping.
                tracing::warn!(error = %error, "could not install the SIGTERM handler; SIGTERM will not trigger graceful teardown");
                let _ = tokio::signal::ctrl_c().await;
                return "Ctrl-C";
            }
        };
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if result.is_err() {
                    // The Ctrl-C future erroring would busy-loop the select; wait
                    // on SIGTERM only from here.
                    term.recv().await;
                    return "SIGTERM";
                }
                "Ctrl-C"
            }
            _ = term.recv() => "SIGTERM",
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "Ctrl-C"
    }
}

/// The boxed per-tick command drain the engine applies at the frame boundary
/// (the control-plane command bus → live reconfiguration), shared by the
/// software-engine and full-pipeline run paths.
///
/// `Send` so the full-pipeline run can be driven on a spawned supervised task
/// under the engine `ProgramSet` (MP-1, ADR-0030 §2.2). The drain runs on the
/// output-clock loop and must be non-blocking (invariants #1 + #10).
type ControlDrain = Box<dyn FnMut(&mut CompositorDrive<Nv12Image>) + Send>;

#[tokio::main]
async fn main() -> ExitCode {
    if let Err(err) = init_tracing() {
        // Logging failed to initialize; report on stderr (tracing is unavailable)
        // and bail. This is startup code, not the data plane.
        eprintln!("multiview: failed to initialize logging: {err:#}");
        return ExitCode::FAILURE;
    }

    let cli = Cli::parse();
    match dispatch(cli).await {
        Ok(code) => code,
        Err(err) => {
            tracing::error!(error = %format!("{err:#}"), "command failed");
            eprintln!("multiview: error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Initialize the structured `tracing` subscriber (stderr, env-overridable via
/// `RUST_LOG`, defaulting to `info`) **with** the resource-scoped log capture
/// layer (ADR-0060): every emitted event — ours and the libav bridge's — is
/// mirrored into a bounded drop-oldest `LogRing` the control plane serves over
/// `GET /api/v1/logs`. The ring is published into the library's process-global
/// slot ([`multiview_cli::set_log_ring`]) so `control::bind_and_serve` wires the
/// same `Arc` into the `AppState`.
fn init_tracing() -> anyhow::Result<()> {
    let ring = Arc::new(multiview_telemetry::LogRing::new(
        multiview_cli::LOG_RING_CAPACITY,
    ));
    // A per-process run id groups a process lifetime's records across restarts.
    let run_id = format!(
        "run-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    );
    SubscriberBuilder::new()
        .with_default_level("info")
        .with_env(true)
        .try_init_with_capture(Arc::clone(&ring), run_id)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    // Publish the ring for the control plane (set-once; ignore a redundant set).
    let _ = multiview_cli::set_log_ring(ring);
    Ok(())
}

/// Dispatch a parsed [`Cli`] to its subcommand, returning the process exit code.
async fn dispatch(cli: Cli) -> anyhow::Result<ExitCode> {
    match cli.command {
        Command::Validate(args) => run_validate(&args),
        Command::Run(args) => run_run(args).await,
        Command::Node(args) => run_node(&args),
    }
}

/// The `node` subcommand (DEV-B5 / ADR-0045): resolve + validate the display-
/// node bootstrap config and identity and print the plan. This slice ships the
/// `--plan-only` software surface; the live ingest → KMS scanout path is a
/// hardware follow-on, so the plan reports it as not-yet-wired rather than
/// silently doing nothing.
fn run_node(args: &NodeArgs) -> anyhow::Result<ExitCode> {
    let plan = multiview_cli::node::run_node(args)?;
    println!("{}", plan.render());
    Ok(ExitCode::SUCCESS)
}

/// The `validate` subcommand: validate one config and print its report. Exits
/// non-zero if the config is invalid.
fn run_validate(args: &ValidateArgs) -> anyhow::Result<ExitCode> {
    let report = validate_config(&args.config)?;
    println!("{}", report.render());
    Ok(if report.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// The `run` subcommand: validate the config, then either drive the FFmpeg-free
/// software engine (`--software`), the full libav\* pipeline (default, `ffmpeg`
/// feature), or — with neither available — report readiness.
async fn run_run(args: RunArgs) -> anyhow::Result<ExitCode> {
    let (boot_text, boot) = load_validated(&args.config)?;

    // Resolve the starting Running state (ADR-W024 §4): `start = "boot"` (the
    // default) runs the boot document; `start = "resume"` runs a valid
    // persisted `active.toml` (with the boot file's restart-only sections
    // spliced in), falling back to boot with a surfaced reason. Loaded stays
    // the boot snapshot in both modes (the revert-to-start target).
    let start = multiview_cli::boot::resolve_start_config(boot, boot_text, &args.config);

    // A configured `[timing].ptp_phc` in a build without the `ptp` feature is
    // a capability this binary cannot provide: fail the run at startup with a
    // clear error (the DEV-B1 display-output fail-fast precedent) — never
    // silently ride the system clock while the config asks for a PHC.
    multiview_cli::timing_gate::ensure_ptp_phc_supported(start.running.timing.as_ref())
        .map_err(|reason| anyhow::anyhow!(reason))?;

    if args.software {
        return run_software(&start, &args).await;
    }

    run_pipeline(&start, &args).await
}

/// The FFmpeg-free software run: the output-clock + CPU compositor driving the
/// built-in test-pattern sources (the software end-to-end smoke of the
/// output-clock invariant), serving the API/WebUI just like the full build.
async fn run_software(
    start: &multiview_cli::boot::StartConfig,
    args: &RunArgs,
) -> anyhow::Result<ExitCode> {
    // The engine is built from the starting Running document (ADR-W024).
    let config = &start.running;
    // Conspect entitlement plane (ADR-0050): assemble the shared lease store +
    // published ladder level from the environment, then gate the NEW engine build
    // (S1). A running engine is NEVER re-gated; this only refuses a *new* start at
    // the block-new-instance rung (the never-off-air promise).
    let plane = multiview_cli::licence::EntitlementPlane::from_env();
    let mut engine =
        SoftwareEngine::build_gated(config, plane.level()).map_err(|e| anyhow::anyhow!("{e}"))?;
    let cadence = engine.cadence();
    let report = if let Some(ticks) = args.tick_budget(cadence) {
        tracing::info!(ticks, "software run: bounded");
        engine
            .run_for_realtime(ticks)
            .await
            .context("software bounded run")?
    } else {
        tracing::info!("software run: until Ctrl-C");
        run_software_until_ctrl_c(&mut engine, start, &plane, &args.config).await?
    };

    println!("{}", report.render());
    Ok(if report.faltered {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// The full libav\* end-to-end pipeline (the `ffmpeg` feature): ingest →
/// composite → encode-once → fan out to the configured file/HLS outputs.
#[cfg(feature = "ffmpeg")]
async fn run_pipeline(
    start: &multiview_cli::boot::StartConfig,
    args: &RunArgs,
) -> anyhow::Result<ExitCode> {
    use multiview_cli::pipeline::Pipeline;

    // The pipeline is built from the starting Running document (ADR-W024).
    let config = &start.running;
    // Conspect entitlement plane (ADR-0050): assemble the shared store + ladder
    // level from the environment, then gate the NEW pipeline build (S1) — refuse a
    // new start at the block-new-instance rung. A running pipeline is never
    // re-gated (the never-off-air promise).
    let plane = multiview_cli::licence::EntitlementPlane::from_env();
    multiview_cli::run::start_gate(plane.level()).map_err(|e| anyhow::anyhow!("{e}"))?;
    // Wire the wait-free tile-watermark signal (S3) into the pipeline's overlay
    // bake (a no-op without the `overlay` feature). The bake samples it off the
    // hot loop; it can never stall the output clock (invariant #1).
    let mut pipeline = Pipeline::build(config)
        .context("building the pipeline")?
        .with_watermark_signal(plane.signal.clone());
    if let Some(subs) = &args.subtitles {
        let track = load_subtitles(subs)
            .with_context(|| format!("loading subtitles {}", subs.display()))?;
        tracing::info!(file = %subs.display(), cues = track.len(), "burning in subtitles");
        pipeline = pipeline.with_subtitles(track);
    }
    if args.program_audio {
        tracing::info!("program audio enabled: muxing an AAC program-audio stream");
        pipeline.enable_program_audio();
    }
    let cadence = pipeline.cadence();
    tracing::info!(
        sources = pipeline.source_count(),
        encoder = pipeline.encoder_name(),
        "pipeline built"
    );

    let report = if let Some(ticks) = args.tick_budget(cadence) {
        tracing::info!(ticks, "pipeline run: bounded");
        pipeline
            .run_for(ticks)
            .await
            .context("bounded pipeline run")?
    } else {
        tracing::info!("pipeline run: until Ctrl-C");
        // MP-1 (ADR-0030 §2.2): the daemon run path builds an engine `ProgramSet`
        // and drives this single program (id "main") through it — move the owned
        // pipeline in (the set spawns it on its own supervised task).
        run_pipeline_until_ctrl_c(pipeline, start, &plane, &args.config).await?
    };

    println!("{}", report.render());
    Ok(if report.faltered {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// The per-run-path inputs the control plane is wired from: the live preview
/// taps (the program slot the run loop fills + the per-source store map), the
/// producer stop registry the live-source hub shares with the run's startup
/// supervisors, and what this run path can take live (ADR-W022 — the binary
/// is the only place that knows both the compiled features and the path).
struct ControlPlaneWiring {
    /// The shared program-frame slot the run loop fills for previews.
    program_slot: multiview_cli::preview::ProgramSlot,
    /// The per-source last-good stores (the preview provider's initial map).
    stores: std::collections::HashMap<String, Arc<multiview_framestore::TileStore<Nv12Image>>>,
    /// The per-source producer stop registry (shared with the live-source hub).
    registry: multiview_cli::live_sources::StopRegistry,
    /// The run's decoded-ingest spawner (ADR-W018 level 2): the full-pipeline
    /// path passes `Pipeline::live_ingest_spawner` so network/file sources
    /// spawn the same supervised `ingest_loop` live; the software path passes
    /// `None`. The live-apply capability declared to the control plane is
    /// **derived from it** — `Some` ⇔ network kinds flip to `live`.
    ingest: Option<Arc<dyn multiview_cli::live_sources::IngestSpawner>>,
    /// What the running engine can take live (per-collection header honesty).
    live_apply: multiview_control::LiveApplyCaps,
    /// The Conspect local entitlement plane (ADR-0050): the verified-lease store
    /// the licence resource renders + the optional pinned issuer key the install
    /// path verifies against. `None` ⇒ the unlicensed-data default. Control-plane
    /// only (invariant #1/#10).
    licence: Option<multiview_control::LicenceState>,
    /// The Conspect local-mesh state (ADR-0051): the shared `MeshState` the
    /// always-on announce/browse loop maintains, so `/api/v1/mesh/*` serves live
    /// discovery. `None` ⇒ an empty, relay-declined default. Control-plane only
    /// (invariant #10).
    mesh: Option<Arc<multiview_mesh::MeshState>>,
    /// The shared WHIP publisher rendezvous (ADR-T014): the pipeline's registry,
    /// so the control plane's `WhipProvider` rendezvous a `POST`ed publisher to
    /// the matching webrtc source's `drive_webrtc` loop. Only under
    /// `webrtc-native`.
    #[cfg(feature = "webrtc-native")]
    webrtc_registry: multiview_cli::webrtc_ingest::WhipRegistry,
    /// The shared WebRTC **output** egress rendezvous (ADR-0049): the pipeline's
    /// per-output drop-oldest feeds, so the control plane builds the WHEP-serve
    /// provider (browser viewers) and the run path spawns the `whip_push` clients,
    /// both fed the encode-once program. Only under `webrtc-native`.
    #[cfg(feature = "webrtc-native")]
    egress_registry: multiview_cli::webrtc_outputs::EgressRegistry,
    /// The shared **program-audio preview tap** (ADR-P006 audio): the SAME
    /// [`ProgramAudioSlot`](multiview_cli::preview::ProgramAudioSlot) the run also
    /// hands the pipeline, so the live WHEP egress provider hears the post-loudnorm
    /// program PCM. `None` ⇒ this run has no program audio (WHEP negotiates
    /// video-only). Only under `webrtc-native`.
    #[cfg(feature = "webrtc-native")]
    program_audio: Option<multiview_cli::preview::ProgramAudioSlot>,
}

/// Bring up the management control plane for a run (one wiring for BOTH run
/// paths — ADR-W013/ADR-W018): the live-source hub over the run's per-source
/// stop registry + the shared (live-updatable) preview store map, the preview
/// provider, the bounded command bus, the bound server, and the ADR-W020
/// config-file watcher over `config_path` (external file edits hot-reload the
/// impacted parts through the same command bus; an invalid file changes
/// nothing).
///
/// Returns the server task handle, the engine-side [`multiview_control::CommandReceiver`]
/// (the caller builds its path-specific frame-boundary drain from it), the
/// [`multiview_cli::live_sources::LiveSourceHub`] (shut down after the run loop
/// returns), and the config-watch handle (stop it at teardown; its
/// `expect_write` seam suppresses server-side writes). The hub shares the
/// wiring's stop registry, so a live remove can tear down a startup producer
/// (generator or ingest thread) too.
// reason: `whep` (WHEP preview egress) and `whip` (WHIP ingest) are the canonical
// WebRTC protocol names — distinct transports, not a typo-similar pair; renaming
// either would obscure the spec mapping.
#[allow(clippy::similar_names)]
async fn serve_control_plane(
    listen: &str,
    start: &multiview_cli::boot::StartConfig,
    config_path: &Path,
    publisher: &Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    wiring: ControlPlaneWiring,
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) -> anyhow::Result<(
    tokio::task::JoinHandle<std::io::Result<()>>,
    multiview_control::CommandReceiver,
    multiview_cli::live_sources::LiveSourceHub,
    multiview_cli::config_watch::ConfigWatchHandle,
    RunningPersist,
)> {
    // The starting Running document drives the engine + seeds the stores; the
    // boot file stays the watch + promote target (ADR-W024).
    let config = &start.running;
    let ControlPlaneWiring {
        program_slot,
        stores,
        registry,
        ingest,
        live_apply,
        licence,
        mesh,
        #[cfg(feature = "webrtc-native")]
        webrtc_registry,
        #[cfg(feature = "webrtc-native")]
        egress_registry,
        #[cfg(feature = "webrtc-native")]
        program_audio,
    } = wiring;
    let (commands, command_rx) = command_bus(64);
    // The honesty keystone (ADR-W018): network kinds are declared live-appliable
    // exactly when the hub below carries a real ingest spawner. RIST is narrowed
    // to the build's actual `rist`-feature truth — an `ffmpeg`-without-`rist`
    // build wires a real ingest spawner (so rtsp/hls/ts/srt/rtmp/file flip to
    // `live`) but a RIST spawn would refuse (`librist` not built), so the header
    // must keep RIST on `restart` there rather than over-claim.
    let live_capability = if ingest.is_some() {
        multiview_control::LiveSourceCapability::synthetic_and_network()
            .with_rist(cfg!(feature = "rist"))
    } else {
        multiview_control::LiveSourceCapability::synthetic_only()
    };
    // The live-source hub (ADR-W018): owns runtime producer spawn/teardown +
    // the SHARED, live-updatable preview store map, off the clock thread.
    let shared_stores = multiview_cli::live_sources::shared_stores(stores);
    let hub = multiview_cli::live_sources::LiveSourceHub::start_with_ingest(
        registry,
        Arc::clone(&shared_stores),
        ingest,
    );
    // ALL native WebRTC roles ride ONE shared dual-stack UDP socket (ADR-0048 §4,
    // box-validation defect B): preview WHEP egress (ADR-P006), WHIP ingest
    // (ADR-T014), WHEP-serve + `whip_push` outputs (ADR-0049). One bind, one driver
    // task that demultiplexes every role by str0m's ufrag/peer demux; TURN media is
    // framed through the relay (defect C). Previously each role bound
    // `webrtc.udp_port` independently and the 2nd/3rd bind hit EADDRINUSE and
    // silently degraded — so with preview+WHIP+WHEP-serve in one config, ingest and
    // output-serve were dead. The preview encode runs on its own pump thread that
    // touches only bounded `SampleFeed`s (never the socket — inv #10). `None` on a
    // pure build (the SPA sheds to JPEG; the WHIP/WHEP routes answer 503).
    #[cfg(feature = "webrtc-native")]
    let (whep, whip, whep_output) = {
        let unified = multiview_cli::webrtc_unified::spawn_unified_webrtc(
            config,
            Arc::clone(&program_slot),
            Arc::clone(&shared_stores),
            program_audio,
            webrtc_registry,
            &egress_registry,
        );
        (unified.whep, unified.whip, unified.whep_output)
    };
    #[cfg(not(feature = "webrtc-native"))]
    let whep: Option<multiview_control::SharedWhep> = None;
    #[cfg(not(feature = "webrtc-native"))]
    let whip: Option<multiview_control::SharedWhip> = None;
    #[cfg(not(feature = "webrtc-native"))]
    let whep_output: Option<multiview_control::SharedWhepOutput> = None;
    // The live-preview provider reads the program slot the run loop fills + the
    // shared per-input store map — read-only for control (invariant #10).
    let provider: multiview_control::SharedPreview = Arc::new(
        multiview_cli::preview::CliPreviewProvider::new(program_slot, shared_stores),
    );
    // The Boot/Loaded/Running model (ADR-W024): Loaded is the immutable boot
    // snapshot; the model backs `GET /api/v1/config/boot-model` and the
    // revert-to-start/promote actions, and is the persister's target.
    let boot_model = Arc::new(start.to_boot_model(config_path));
    let (addr, handle, state) = control::bind_and_serve(
        listen,
        config,
        Arc::clone(publisher),
        commands,
        provider,
        whep,
        whip,
        whep_output,
        licence,
        mesh,
        live_apply,
        live_capability,
        Some(Arc::clone(&boot_model)),
        async move {
            let _ = shutdown_rx.await;
        },
    )
    .await
    .with_context(|| format!("binding the control plane on {listen}"))?;
    tracing::info!(listen = %addr, "control plane listening (OpenAPI/Scalar docs at /docs)");
    // Persist the Loaded snapshot to `loaded.toml` (forensics + the on-disk
    // revert-target record). Fail-soft: the in-memory snapshot is
    // authoritative; a write failure is warned, never fatal.
    if let Err(reason) = multiview_control::boot_model::persist_loaded(&boot_model) {
        tracing::warn!(
            reason = %reason,
            "could not persist the Loaded snapshot (fail-soft; the in-memory snapshot is authoritative)"
        );
    }
    // The debounced Running persister (ADR-W024 §3): waits on the ONE
    // `running_changed` choke-point signal `AppState::audit` fires, then writes
    // `active.toml` atomically. Control-plane file I/O only (inv #10).
    let persist = RunningPersist {
        task: multiview_control::boot_model::spawn_running_persist(
            state.clone(),
            RUNNING_PERSIST_DEBOUNCE,
        ),
        state: state.clone(),
    };
    // Watch the boot config file for external edits (ADR-W020): a valid write
    // hot-reloads the impacted parts through the SAME router state + command
    // bus; an invalid write warns and changes nothing. A control-plane tokio
    // tenant — it can never pace or stall the engine (inv #1/#10). The diff
    // baseline is the RUNNING document (the resumed one under resume), and the
    // boot-load TEXT seeds the last-observed content (ADR-W024 §4: the
    // unchanged boot file must never clobber a resumed baseline; an edit in the
    // boot window differs from this text and still applies).
    // The watcher drives the SAME handle `bind_and_serve` already installed
    // into the state BEFORE serving (ADR-W024 MAJOR-C1), so the `promote` route
    // found the suppression seam from the very first served request — no
    // startup window where a promote skips suppression. `bind_and_serve`
    // installs it for every config-file run (a boot model is wired), so it is
    // present here.
    let mut watch_options = multiview_cli::config_watch::WatchOptions::default()
        .with_initial_observed(start.boot_text.clone());
    if let Some(installed) = state.watch_handle() {
        watch_options = watch_options.with_handle(installed);
    }
    let watch = multiview_cli::config_watch::spawn(
        config_path.to_path_buf(),
        config.clone(),
        state.clone(),
        watch_options,
    );
    Ok((handle, command_rx, hub, watch, persist))
}

/// Drive the ingest/composite/encode pipeline until Ctrl-C while **also**
/// serving the control plane, the embedded web UI, and the live program/input
/// previews from the SAME run (when `[control]` is configured) — ingestion,
/// processing, output, and management are one integrated process. The control
/// plane shares the engine's outbound publisher (read-only) and the live-preview
/// slot, and submits to the non-blocking command bus the pipeline drains at each
/// frame boundary; none of it can back-pressure the output clock (inv #1 + #10).
#[cfg(feature = "ffmpeg")]
// This is the full-pipeline run-path orchestration entry point: it brings up
// the control plane (with the feature-gated overlay/seam drain selection), the
// off-hot-loop pollers (metrics/retention/timing) + the GPU capability probe,
// drives the program through the engine `ProgramSet`, then tears every tenant
// down. The cohesive sub-units (`spawn_metrics_retention`, `shutdown_run_tenants`)
// are already extracted; what remains is straight-line wiring whose value is in
// reading it top-to-bottom in one place. Splitting the remaining body would only
// scatter the run lifecycle across helpers without improving clarity, so the
// line-count lint is allowed here with justification (matching `bind_and_serve`).
#[allow(clippy::too_many_lines)]
async fn run_pipeline_until_ctrl_c(
    // `mut` is needed only under `webrtc-native` (the program-audio preview tap
    // below calls `pipeline.set_program_audio_preview`). Without that feature the
    // binding is never mutated, so the `mut` is conditional to stay
    // `unused_mut`-clean on the `ffmpeg`/`rist`-only feature legs.
    #[cfg(feature = "webrtc-native")] mut pipeline: multiview_cli::pipeline::Pipeline,
    #[cfg(not(feature = "webrtc-native"))] pipeline: multiview_cli::pipeline::Pipeline,
    start: &multiview_cli::boot::StartConfig,
    plane: &multiview_cli::licence::EntitlementPlane,
    config_path: &Path,
) -> anyhow::Result<multiview_cli::pipeline::PipelineReport> {
    // The Running document seeds the stores/drain; the boot file (config_path)
    // stays the watch + promote target (ADR-W024).
    let config = &start.running;
    let stop = StopSignal::new();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let preview_slot = multiview_cli::preview::program_slot();
    // The shared program-audio preview tap (ADR-P006 audio): wired ONLY when this
    // run carries program audio AND a live WHEP egress can consume it
    // (`webrtc-native`). The bake consumer pushes post-loudnorm program PCM into
    // it; the WHEP provider drains it to Opus-encode + send to the peer. The cli
    // shares the SAME slot here (the pipeline) and in the WHEP provider, so
    // program audio reaches a WHEP peer when (and only when) it is configured. A
    // preview tap off the bake-consumer thread — never the output clock (inv
    // #1/#10). `None` (no audio / no WHEP build) means the consumer pushes
    // nothing, byte-identical to today.
    #[cfg(feature = "webrtc-native")]
    let program_audio: Option<multiview_cli::preview::ProgramAudioSlot> =
        pipeline.has_program_audio().then(|| {
            let slot = multiview_cli::preview::ProgramAudioSlot::new();
            pipeline.set_program_audio_preview(slot.clone());
            slot
        });
    // This program's cadence (the legacy single program's canvas fps) for the
    // engine `ProgramSet` member metadata.
    let cadence = pipeline.cadence();

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    // Conspect local-mesh (ADR-0051 §2/§5): start the always-on discovery loop
    // (best-effort, never blocks — inv #10) before serving, so `/api/v1/mesh/peers`
    // reflects live neighbours. The shared `MeshState` is wired into the control
    // plane below regardless of the `mesh-mdns` feature.
    plane.spawn_mesh_discovery();
    // Conspect device heartbeat (CONSPECT-3 / ADR-0096): start the supervised
    // licensing phone-home (best-effort, never off air — inv #1/#10) when the
    // `heartbeat` feature is built AND the device is configured. It renews the
    // lease via the same `install_binding` convergence; a no-op otherwise.
    plane.spawn_heartbeat();
    let (server, drain, live_hub, config_watch, running_persist): (
        Option<_>,
        ControlDrain,
        Option<_>,
        Option<_>,
        Option<RunningPersist>,
    ) = if let Some(cfg) = config.control.as_ref() {
        // What THIS build + run path can take live (ADR-W022): with the
        // `overlay` feature the bake consumer renders the overlay working
        // set, so overlay documents the renderer draws (analog-face
        // clocks — `live_overlays::renders_live`, the same predicate the
        // drain warns by) apply live; without it nothing overlay-side
        // renders and the honest default (everything `restart`) stands.
        #[cfg(feature = "overlay")]
        let live_apply = multiview_control::LiveApplyCaps::default().with_overlays(
            multiview_control::OverlayLiveCapability::new(
                multiview_cli::live_overlays::renders_live,
            ),
        );
        #[cfg(not(feature = "overlay"))]
        let live_apply = multiview_control::LiveApplyCaps::default();
        let (handle, command_rx, hub, watch, persist) = serve_control_plane(
            &cfg.listen,
            start,
            config_path,
            &publisher,
            ControlPlaneWiring {
                program_slot: Arc::clone(&preview_slot),
                stores: pipeline.preview_stores(),
                registry: pipeline.stop_registry(),
                // The real decoded-ingest spawner (ADR-W018 level 2):
                // network/file sources live-apply through the SAME
                // supervised ingest_loop the startup path builds.
                ingest: Some(pipeline.live_ingest_spawner()),
                live_apply,
                licence: Some(multiview_control::LicenceState::new(
                    Arc::clone(&plane.store),
                    plane.pinned.clone(),
                )),
                mesh: Some(Arc::clone(&plane.mesh)),
                #[cfg(feature = "webrtc-native")]
                webrtc_registry: pipeline.webrtc_registry(),
                #[cfg(feature = "webrtc-native")]
                egress_registry: pipeline.egress_registry(),
                #[cfg(feature = "webrtc-native")]
                program_audio: program_audio.clone(),
            },
            shutdown_rx,
        )
        .await?;
        // Thread the run's live subtitle re-point seam (RT-10b) into the drain so a
        // `RouteSubtitle` (RT-11) reaches the running pipeline's layer. The slot is
        // shared (lock-free `ArcSwapOption`); the run publishes its handle into it at
        // drive start, and the drain reads it wait-free (inv #1/#10). Only under
        // `overlay` (without it the run renders no subtitles, so there is no layer).
        // The live overlay seam (ADR-W022) rides the same variant; the
        // live-source seam (ADR-W018) rides both.
        #[cfg(feature = "overlay")]
        let drain: ControlDrain = Box::new(control::command_drain_with_seams(
            command_rx,
            config.clone(),
            Arc::clone(&publisher),
            pipeline.subtitle_route_slot(),
            pipeline.overlay_apply_slot(),
            hub.handle(),
            pipeline.player_mailboxes(),
        ));
        #[cfg(not(feature = "overlay"))]
        let drain: ControlDrain = Box::new(control::command_drain_with_live_sources_and_players(
            command_rx,
            config.clone(),
            Arc::clone(&publisher),
            hub.handle(),
            pipeline.player_mailboxes(),
        ));
        (Some(handle), drain, Some(hub), Some(watch), Some(persist))
    } else {
        drop(shutdown_rx);
        (
            None,
            Box::new(|_d: &mut CompositorDrive<Nv12Image>| {}),
            None,
            None,
            None,
        )
    };

    let stop_for_signal = stop.clone();
    let signal = tokio::spawn(async move {
        let which = await_shutdown_signal().await;
        tracing::info!(
            signal = which,
            "stop signal received; stopping after the current frame"
        );
        stop_for_signal.stop();
    });

    // Sample CPU/host-memory/per-GPU load at ~1.3 Hz and PUSH `Event::SystemMetrics`
    // onto the SAME outbound publisher the control plane forwards to the WebUI
    // footer (the full-pipeline path serves the control plane, so the poller must
    // live here too — not only in the software-only run). The publish never
    // awaits/blocks a slow subscriber (inv #10); the task self-stops on `stop`.
    let metrics_task = multiview_cli::system_metrics::spawn(
        Arc::clone(&publisher),
        multiview_cli::system_metrics::default_load_source(),
        stop.clone(),
        None,
    );

    // GPU-5c: the live adaptive-placement execution loop. On a MULTI-GPU host it
    // polls per-GPU load (the `LoadPoller → arc_swap` snapshot), observes the
    // `PlacementController` one control tick (recording the placement counters),
    // and emits a `ShedLoad` when a sustained overload cannot be cured by
    // migrating — activating GPU-5b's inert controller. It runs entirely off the
    // output clock (a 1 Hz control task) so it can never stall the engine (inv
    // #1/#10); `spawn` returns `None` (and runs nothing) on a single-GPU/no-GPU
    // host, where there is no migration target. The placement counters register
    // against a run-local registry; the make-before-break crosspoint is the
    // engine RT-12 bridge the coordinator drives.
    let placement_metrics_registry = multiview_telemetry::metrics::MetricsRegistry::new();
    let placement_task = multiview_cli::placement::spawn(
        config,
        multiview_cli::placement::PlacementInputs {
            load_source: multiview_cli::system_metrics::default_load_source(),
            canvas: pipeline.canvas_resolution(),
            cadence,
            tile_count: pipeline.tile_count(),
            opens_encode_session: pipeline.opens_encode_session(),
        },
        Arc::clone(&publisher),
        &placement_metrics_registry,
        multiview_engine::OutputCrosspoint::new(),
        stop.clone(),
    );

    // CONSPECT engine-seam S5 (ADR-0052 §3): the consent-independent local-metrics
    // retention feed for the real libav pipeline.
    let (retention_store, retention_task) = spawn_metrics_retention(&publisher);

    // DEV-C1 (ADR-M010): the ~1 Hz outbound presentation-epoch publisher — one
    // `WallClockRef` per program as conflated `timing.status` on the control WS
    // plus the shared HLS-PDT cell every HLS sink stamps from. It binds lazily
    // to the run's tick-0 anchor (published when the clock seeds) and never
    // touches the engine (inv #1/#10); it self-stops on the run's StopSignal.
    let timing_cfg = config.timing.clone().unwrap_or_default();
    let timing_task = multiview_cli::timing_status::spawn(
        Arc::clone(&publisher),
        pipeline.epoch_anchor_slot(),
        pipeline.shared_epoch(),
        multiview_cli::timing_status::TimingStatusOptions {
            stream_id: multiview_config::ProgramId::MAIN.to_owned(),
            link_offset_ns: timing_cfg.link_offset_ns(),
            ptp_phc: timing_cfg.ptp_phc.clone(),
            ptp_utc_offset_ns: timing_cfg.ptp_utc_offset_ns(),
        },
        stop.clone(),
    );

    // SA-0 (ADR-0035): at build time, off the output-clock thread (the clock is
    // not yet constructed → inv #1), cross-check the wgpu compositor adapter
    // against discovered hardware. If a real GPU is present but compositing
    // resolved a software/CPU adapter (the silent fallback), emit a latched,
    // actionable `gpu-present-no-vulkan-adapter` warning through the SAME
    // drop-oldest publisher (inv #10) so the operator sees a banner + a
    // `GET /api/v1/health` entry instead of a silent CPU burn. Only under `gpu`
    // (without it the CPU composite is the intentional choice → nothing emitted).
    #[cfg(feature = "gpu")]
    {
        let since_nanos = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        )
        .unwrap_or(i64::MAX);
        let _ = multiview_cli::capability_warn::probe_and_emit(
            publisher.as_ref(),
            multiview_cli::system_metrics::default_load_source().as_ref(),
            since_nanos,
        );
    }

    // MP-1 (ADR-0030 §2.2): build the engine `ProgramSet` and drive this single
    // program (id "main") through it — behaviour-identical to today (one program,
    // the same drive/stop/publisher/preview/drain). See `drive_main_program_in_set`.
    let report =
        drive_main_program_in_set(pipeline, cadence, &stop, &publisher, &preview_slot, drain)
            .await?;

    // The pipeline loop returned; tear down every run tenant in one call.
    shutdown_run_tenants(RunTenants {
        metrics_task,
        placement_task,
        timing_task,
        retention_task,
        retention_store,
        config_watch,
        running_persist,
        live_hub,
        server,
        shutdown_tx,
        signal,
    })
    .await;
    Ok(report)
}

/// Drive the single legacy `"main"` program through an engine
/// [`ProgramSet`](multiview_engine::ProgramSet) (MP-1, ADR-0030 §2.2).
///
/// For the legacy single-program config the set has **exactly one** program (id
/// `"main"`) — behaviour-identical to driving the [`Pipeline`] directly: the same
/// `run_until_serving` drive, the same `StopSignal` (Ctrl-C reaches the program via
/// the supervisor's per-program stop handle), the same publisher/preview/drain. The
/// set owns the program's lifecycle (spawn on its own supervised task, stop, join)
/// and samples its **live** `ticks_emitted` off a shared counter the pipeline
/// increments per tick — exactly the N-concurrent-programs machinery, exercised
/// here at N=1. MP-5 routes the config's `[[programs]]` into the same
/// `ProgramSet::start` for N>1.
///
/// # Errors
///
/// Propagates a failure to admit/start the program, or the run's own error.
#[cfg(feature = "ffmpeg")]
async fn drive_main_program_in_set(
    pipeline: multiview_cli::pipeline::Pipeline,
    cadence: multiview_core::time::Rational,
    stop: &StopSignal,
    publisher: &Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    preview_slot: &multiview_cli::preview::ProgramSlot,
    drain: ControlDrain,
) -> anyhow::Result<multiview_cli::pipeline::PipelineReport> {
    // The shared monotonic reference every program in the set reads (its one program
    // reads it for its own clock's seed; identical to the inline `Monotonic` source
    // the pipeline built before).
    let mut programs: ProgramSet<RealtimePacer> =
        ProgramSet::new(Arc::new(multiview_engine::MonotonicTimeSource::new()));
    let program_id = ProgramId::new(ProgramId::MAIN).context("the reserved \"main\" program id")?;
    // The live per-tick counter the `ProgramSet` samples for "main": the pipeline
    // increments it once per emitted output tick (a single wait-free `fetch_add`),
    // so `programs.ticks_emitted("main")` is genuinely live, not fabricated.
    let main_ticks = Arc::new(std::sync::atomic::AtomicU64::new(0));
    // Recover the run's `PipelineReport` from the supervised task.
    let (report_tx, report_rx) =
        tokio::sync::oneshot::channel::<Result<multiview_cli::pipeline::PipelineReport, String>>();

    let run_stop = stop.clone();
    let run_publisher = Arc::clone(publisher);
    let run_preview = Arc::clone(preview_slot);
    let run_ticks = Arc::clone(&main_ticks);
    let program = Program::<RealtimePacer>::from_runner(
        program_id,
        cadence,
        run_stop.clone(),
        main_ticks,
        move || {
            Box::pin(async move {
                let mut pipeline = pipeline;
                let outcome = pipeline
                    .run_until_serving_observed(
                        &run_stop,
                        run_publisher.as_ref(),
                        &run_preview,
                        drain,
                        Some(run_ticks),
                    )
                    .await;
                let exit = if outcome.is_ok() {
                    ActorExit::Completed
                } else {
                    ActorExit::Failed
                };
                let _ = report_tx.send(outcome.map_err(|e| e.to_string()));
                exit
            })
        },
    );
    programs
        .start(program)
        .context("starting the \"main\" program in the ProgramSet")?;

    // Await the single program's NATURAL completion: its run returns when the
    // StopSignal is raised (Ctrl-C), at which point it sends its report. We await
    // that first, THEN `shutdown` (raise any still-running program's stop — a no-op
    // here — and join every supervised task so no task is left detached).
    let recovered = report_rx.await;
    programs.shutdown().await;
    match recovered {
        Ok(Ok(report)) => Ok(report),
        Ok(Err(reason)) => Err(anyhow::anyhow!(reason)).context("pipeline run until Ctrl-C"),
        Err(_) => Err(anyhow::anyhow!(
            "the \"main\" program task ended without reporting"
        )),
    }
}

/// Without the `ffmpeg` feature this build has no libav decoders, so external
/// ingest/encode is unavailable: build the software engine, report readiness,
/// and steer the operator to `--software` (or a build with `--features ffmpeg`)
/// rather than pretending a daemon is ingesting sources it cannot decode.
#[cfg(not(feature = "ffmpeg"))]
#[allow(clippy::unused_async)]
// reason: this is the no-`ffmpeg` half of an `async fn` pair; the `ffmpeg`
// counterpart awaits the full pipeline, so the signature must match for the one
// `run_pipeline(..).await` call site to compile under either feature set.
async fn run_pipeline(
    start: &multiview_cli::boot::StartConfig,
    args: &RunArgs,
) -> anyhow::Result<ExitCode> {
    let config = &start.running;
    if args.subtitles.is_some() {
        tracing::warn!(
            "--subtitles needs the `ffmpeg`+`overlay` features (full pipeline); ignoring"
        );
    }
    if args.program_audio {
        tracing::warn!("--program-audio needs the `ffmpeg` feature (full pipeline); ignoring");
    }
    // Conspect S1 startup gate: gate the NEW engine build on the published ladder
    // level (refuse at the block-new-instance rung).
    let plane = multiview_cli::licence::EntitlementPlane::from_env();
    let engine =
        SoftwareEngine::build_gated(config, plane.level()).map_err(|e| anyhow::anyhow!("{e}"))?;
    println!(
        "ready: built engine for {} source(s) at {}/{} fps; \
         this build has no `ffmpeg` feature, so an external ingest/encode run is unavailable — \
         use `--software` for the output-clock smoke, or rebuild with \
         `--features ffmpeg` (add `gpl-codecs` for software H.264/H.265).",
        engine.source_count(),
        engine.cadence().num,
        engine.cadence().den,
    );
    Ok(ExitCode::SUCCESS)
}

/// Drive the software engine until Ctrl-C, then return the run report.
///
/// A best-effort signal watcher raises the engine's stop flag on Ctrl-C; the
/// engine checks it once per tick and finishes the current frame cleanly. The
/// watcher cannot back-pressure the engine (invariant #10).
// reason: one straight-line run lifecycle — bring up the publisher, the
// optional control plane + Running persister + watcher, the off-hot-loop
// pollers, drive the engine, then tear them all down in order. The cohesive
// sub-units are already extracted (`serve_control_plane`,
// `shutdown_run_tenants`, `spawn_metrics_retention`); what remains reads best
// top-to-bottom in one place (matching `run_pipeline_until_ctrl_c`).
#[allow(clippy::too_many_lines)]
async fn run_software_until_ctrl_c(
    engine: &mut SoftwareEngine,
    start: &multiview_cli::boot::StartConfig,
    plane: &multiview_cli::licence::EntitlementPlane,
    config_path: &Path,
) -> anyhow::Result<RunReport> {
    // The Running document seeds the stores/drain; the boot file (config_path)
    // stays the watch + promote target (ADR-W024).
    let config = &start.running;
    let stop = StopSignal::new();

    // The engine's outbound publisher, shared read-only with the control plane
    // when it is enabled: the API/WebUI observe live engine state through the
    // wait-free latest-state slot + drop-oldest event broadcast, never able to
    // back-pressure the engine (invariant #10). 64 = the broadcast ring depth.
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));

    // Bring up the control server iff `[control]` is configured, and build the
    // per-tick command drain the engine applies at the frame boundary. The drain
    // is boxed so the run call is uniform: the live command-bus drain when the
    // control plane is up, a no-op otherwise. The server serves until
    // `shutdown_rx` resolves (once the engine loop returns).
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    // Conspect local-mesh (ADR-0051 §2/§5): start the always-on discovery loop
    // (best-effort, never blocks — inv #10) before serving. The shared `MeshState`
    // is wired into the control plane below regardless of the `mesh-mdns` feature.
    plane.spawn_mesh_discovery();
    // Conspect device heartbeat (CONSPECT-3 / ADR-0096): the supervised licensing
    // phone-home (best-effort, never off air — inv #1/#10), when the `heartbeat`
    // feature is built and the device is configured; a no-op otherwise.
    plane.spawn_heartbeat();
    let (server, drain, live_hub, config_watch, running_persist): (
        Option<_>,
        ControlDrain,
        Option<_>,
        Option<_>,
        Option<RunningPersist>,
    ) = if let Some(cfg) = config.control.as_ref() {
        let (handle, command_rx, hub, watch, persist) = serve_control_plane(
            &cfg.listen,
            start,
            config_path,
            &publisher,
            ControlPlaneWiring {
                program_slot: engine.program_preview(),
                stores: engine.preview_stores(),
                registry: engine.stop_registry(),
                // The software engine has no decoder: no ingest spawner, so
                // the capability (and the apply header) honestly stays
                // synthetic-only (ADR-W018 level 2).
                ingest: None,
                // The software engine has no bake stage: no overlay
                // document renders on this path, so the honest default
                // (everything `restart`) is the truth (ADR-W022).
                live_apply: multiview_control::LiveApplyCaps::default(),
                licence: Some(multiview_control::LicenceState::new(
                    Arc::clone(&plane.store),
                    plane.pinned.clone(),
                )),
                mesh: Some(Arc::clone(&plane.mesh)),
                // The software run path has no ingest pipeline, so no webrtc
                // source drive loop reads this registry — an empty one keeps
                // the WhipProvider present (publishes ride NO_SIGNAL on a
                // run with no webrtc tile). `webrtc-native` ⊃ `ffmpeg`, so in
                // practice the pipeline path is taken; this keeps the struct
                // total under the rare software+webrtc-native combination.
                #[cfg(feature = "webrtc-native")]
                webrtc_registry: multiview_cli::webrtc_ingest::WhipRegistry::new(),
                // No pipeline on this path ⇒ no encode-once fan-out to feed, so
                // an empty egress registry: the WHEP provider stays NoWhepOutput
                // and no whip_push clients spawn (honest — there is nothing to
                // publish without a running program).
                #[cfg(feature = "webrtc-native")]
                egress_registry: multiview_cli::webrtc_outputs::EgressRegistry::new(),
                // The software run path has no bake stage, so no program-audio
                // PCM is produced — a WHEP peer on this path is video-only.
                #[cfg(feature = "webrtc-native")]
                program_audio: None,
            },
            shutdown_rx,
        )
        .await?;
        // The software engine has no libav decode, so no media-player channels
        // run on this path — the plain 4-arg form (no player mailboxes).
        let drain: ControlDrain = Box::new(control::command_drain_with_live_sources(
            command_rx,
            config.clone(),
            Arc::clone(&publisher),
            hub.handle(),
        ));
        (Some(handle), drain, Some(hub), Some(watch), Some(persist))
    } else {
        drop(shutdown_rx);
        (
            None,
            Box::new(|_d: &mut CompositorDrive<Nv12Image>| {}),
            None,
            None,
            None,
        )
    };

    let stop_for_signal = stop.clone();
    let signal = tokio::spawn(async move {
        let which = await_shutdown_signal().await;
        tracing::info!(
            signal = which,
            "stop signal received; stopping after the current frame"
        );
        stop_for_signal.stop();
    });

    // The off-hot-path system-metrics poller: samples whole-system CPU + host
    // memory + per-GPU load at ~1.3 Hz and PUSHES `Event::SystemMetrics` onto the
    // engine's outbound event stream (the same drop-oldest broadcast the control
    // plane forwards to the WebUI footer). The publish never awaits/blocks a slow
    // subscriber (invariant #10), and the task self-stops on the run's StopSignal.
    // `program_fps` is left `None`: the software run exposes no live measured-fps
    // counter to this task, and the configured cadence is a target — not a
    // measured rate — so we do not fabricate one.
    let metrics_task = multiview_cli::system_metrics::spawn(
        Arc::clone(&publisher),
        multiview_cli::system_metrics::default_load_source(),
        stop.clone(),
        None,
    );

    // CONSPECT engine-seam S5 (ADR-0052 §3): the consent-independent local-metrics
    // retention feed for the software run.
    let (retention_store, retention_task) = spawn_metrics_retention(&publisher);

    // DEV-C1 (ADR-M010): the outbound presentation epoch publishes on the
    // software path too — `timing.status` per program on the same drop-oldest
    // broadcast. The software run has no HLS sinks, so its epoch cell has no
    // PDT consumer; the WS surface is identical to the full-pipeline path.
    let timing_cfg = config.timing.clone().unwrap_or_default();
    let timing_task = multiview_cli::timing_status::spawn(
        Arc::clone(&publisher),
        engine.epoch_anchor_slot(),
        multiview_output::SharedEpoch::new(),
        multiview_cli::timing_status::TimingStatusOptions {
            stream_id: multiview_config::ProgramId::MAIN.to_owned(),
            link_offset_ns: timing_cfg.link_offset_ns(),
            ptp_phc: timing_cfg.ptp_phc.clone(),
            ptp_utc_offset_ns: timing_cfg.ptp_utc_offset_ns(),
        },
        stop.clone(),
    );

    let report = engine
        .run_until_stopped_with_control(&stop, publisher.as_ref(), drain)
        .await
        .context("headless run until Ctrl-C")?;

    // The engine loop returned; tear down every run tenant in one call.
    shutdown_run_tenants(RunTenants {
        metrics_task,
        // The software (FFmpeg-free) run drives no GPU pipeline, so there is no
        // adaptive placement to execute.
        placement_task: None,
        timing_task,
        retention_task,
        retention_store,
        config_watch,
        running_persist,
        live_hub,
        server,
        shutdown_tx,
        signal,
    })
    .await;
    Ok(report)
}

/// Parse an external SRT/`WebVTT` subtitle file into a [`CueTrack`]. The format
/// is chosen by the extension (`.vtt`/`.webvtt` ⇒ `WebVTT`, otherwise `SubRip`).
#[cfg(feature = "ffmpeg")]
fn load_subtitles(path: &Path) -> anyhow::Result<multiview_overlay::subtitle::CueTrack> {
    use multiview_overlay::subtitle::CueTrack;
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading subtitles {}", path.display()))?;
    let is_vtt = path
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|e| e.eq_ignore_ascii_case("vtt") || e.eq_ignore_ascii_case("webvtt"));
    let track = if is_vtt {
        CueTrack::parse_vtt(&text)
    } else {
        CueTrack::parse_srt(&text)
    }
    .with_context(|| format!("parsing subtitles {}", path.display()))?;
    Ok(track)
}

/// Load and validate a config, failing with a clear error if it is invalid.
/// Non-fatal advisories (e.g. a clock setting both `timezone` and
/// `tz_offset_minutes`) are surfaced to the operator via `tracing::warn!`
/// before the engine starts — they never fail the load.
fn load_validated(path: &Path) -> anyhow::Result<(String, MultiviewConfig)> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    let config = MultiviewConfig::load_from_toml(&text)
        .with_context(|| format!("parsing config {}", path.display()))?;
    config
        .validate()
        .with_context(|| format!("validating config {}", path.display()))?;
    for warning in multiview_cli::validate::config_warnings(&config) {
        tracing::warn!(advisory = %warning, "config advisory");
    }
    // The raw boot-file TEXT is returned alongside the parsed config (ADR-W024
    // §4): it seeds the file watcher's initial last-observed content so a
    // resumed run does not reapply the unchanged boot file.
    Ok((text, config))
}

/// The control-plane + off-hot-loop tenants a run brings up alongside the
/// engine, gathered so a run path can tear them ALL down in one call
/// ([`shutdown_run_tenants`]) — identical for the software and full-pipeline
/// paths. None of these can back-pressure the engine (invariant #1/#10); this
/// is purely orderly shutdown after the drive loop has already returned.
struct RunTenants {
    /// The ~1.3 Hz system-metrics poller (self-stops on the `StopSignal` too).
    metrics_task: tokio::task::JoinHandle<()>,
    /// The GPU-5c adaptive-placement execution loop (multi-GPU hosts only;
    /// `None` when fewer than two GPUs are visible). Self-stops on the
    /// `StopSignal`.
    placement_task: Option<tokio::task::JoinHandle<()>>,
    /// The ~1 Hz presentation-epoch / `timing.status` publisher.
    timing_task: tokio::task::JoinHandle<()>,
    /// The CONSPECT S5 consent-independent local-metrics retention feed.
    retention_task: tokio::task::JoinHandle<()>,
    /// The retention store the feed wrote (summarised at shutdown).
    retention_store: Arc<multiview_telemetry::retention::RetentionStore>,
    /// The ADR-W020 config-file watcher, when the control plane is up.
    config_watch: Option<multiview_cli::config_watch::ConfigWatchHandle>,
    /// The ADR-W024 debounced Running persister, when the control plane is up:
    /// finished at teardown (abort → await → one final `active.toml` write
    /// capturing changes younger than the debounce).
    running_persist: Option<RunningPersist>,
    /// The ADR-W018 live-source hub (stops + joins every runtime producer).
    live_hub: Option<multiview_cli::live_sources::LiveSourceHub>,
    /// The control server task, when the control plane is up.
    server: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
    /// Signals the control server to begin its graceful shutdown.
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
    /// The Ctrl-C watcher task.
    signal: tokio::task::JoinHandle<()>,
}

/// Tear down every run tenant after the engine drive loop has returned: abort
/// the off-hot-loop pollers + the retention feed (each also self-stops on the
/// `StopSignal`), log the retained-metrics summary, stop the config-file
/// watcher, shut the live-source hub down, then signal + join the control
/// server. Order matters only for the graceful server join (signal → await).
async fn shutdown_run_tenants(tenants: RunTenants) {
    let RunTenants {
        metrics_task,
        placement_task,
        timing_task,
        retention_task,
        retention_store,
        config_watch,
        running_persist,
        live_hub,
        server,
        shutdown_tx,
        signal,
    } = tenants;
    metrics_task.abort();
    if let Some(task) = placement_task {
        task.abort();
    }
    timing_task.abort();
    retention_task.abort();
    log_retention_summary(&retention_store);
    // Finish the Running persister BEFORE stopping the watcher (ADR-W024 §3):
    // abort → await → one final best-effort `active.toml` write capturing
    // changes younger than the debounce, so a resume after this shutdown sees
    // the latest Running state. Fail-soft.
    if let Some(persist) = running_persist {
        persist.finish().await;
    }
    if let Some(watch) = config_watch {
        watch.stop();
    }
    if let Some(hub) = live_hub {
        hub.shutdown();
    }
    let _ = shutdown_tx.send(());
    if let Some(handle) = server {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(error = %e, "control server I/O error at shutdown"),
            Err(e) => tracing::warn!(error = %e, "control server task join error"),
        }
    }
    signal.abort();
}

/// Spawn the CONSPECT engine-seam S5 (ADR-0052 §3) consent-independent
/// local-metrics retention feed and return its store + task handle.
///
/// A read-only subscriber to the SAME outbound broadcast mirrors live
/// utilisation / per-input reconnect / incident events into the bounded,
/// drop-oldest on-box [`RetentionStore`] the §7.2 support bundle draws from —
/// independent of telemetry consent, never able to back-pressure the engine
/// (read-only + lagged-skip, invariant #10). The caller holds the returned
/// store in run scope (so it lives for the whole run, logged at shutdown via
/// [`log_retention_summary`]); the feed task self-terminates when the engine's
/// publish handles drop at shutdown.
fn spawn_metrics_retention(
    publisher: &Arc<EnginePublisher<EngineStateSnapshot, Event>>,
) -> (
    Arc<multiview_telemetry::retention::RetentionStore>,
    tokio::task::JoinHandle<()>,
) {
    let retention_store = Arc::new(multiview_telemetry::retention::RetentionStore::new());
    let retention_task = tokio::spawn(multiview_cli::metrics_retention::run_metrics_retention(
        publisher.subscribe(),
        Arc::clone(&retention_store),
    ));
    (retention_store, retention_task)
}

/// Log a one-line summary of what the consent-independent local-metrics retention
/// store (CONSPECT S5) accumulated over the run, across the full 7-day window.
///
/// This both surfaces the on-box diagnostics tally to the operator and confirms
/// the feed recorded from the live event stream. The store stays in scope until
/// here so it lived for the whole run; the §7.2 support-bundle endpoint that
/// *reads* it is a separate CONSPECT item (not part of this change).
fn log_retention_summary(store: &multiview_telemetry::retention::RetentionStore) {
    use multiview_telemetry::retention::RetentionWindow::LastWeek;
    let now = multiview_cli::metrics_retention::now_unix_seconds();
    let reconnects = store.reconnect_window(now, LastWeek).len();
    let incidents = store.incident_window(now, LastWeek).len();
    let util_minutes = store.utilisation_window(now, LastWeek).len();
    tracing::info!(
        reconnects,
        incidents,
        util_minutes,
        "consent-independent local metrics retained (7-day window) at shutdown"
    );
}
