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
use multiview_cli::cli::{AcknowledgeFallbackArgs, Cli, Command, NodeArgs, RunArgs, ValidateArgs};
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

/// What an exit decision freezes (#367): the config-file watcher stops, and
/// the accepted state (`desired.json`) takes no further commit from any path
/// (the watcher, a REST mutation, a revert). Cloneable into the signal task and
/// the exit-durable thread; [`Self::freeze`] is wait-free and idempotent.
#[derive(Clone)]
struct ExitFreeze {
    watch: Option<multiview_cli::config_watch::ConfigWatchHandle>,
    state: Option<multiview_control::AppState>,
}

impl ExitFreeze {
    fn new(
        watch: Option<&multiview_cli::config_watch::ConfigWatchHandle>,
        state: Option<&multiview_control::AppState>,
    ) -> Self {
        Self {
            watch: watch.cloned(),
            state: state.cloned(),
        }
    }

    fn freeze(&self) {
        if let Some(state) = &self.state {
            state.freeze_accepted_state();
        }
        if let Some(watch) = &self.watch {
            watch.stop();
        }
    }

    /// Freeze, then wait (blocking, at most [`ACCEPTED_COMMIT_SETTLE`]) for a
    /// `desired.json` rename whose freeze check ran before the freeze. Call
    /// only off the async workers and after the exit watchdog is armed.
    fn freeze_and_settle(&self) {
        self.freeze();
        if let Some(state) = &self.state {
            if !state.await_accepted_commits(ACCEPTED_COMMIT_SETTLE) {
                tracing::warn!(
                    "a desired.json rename decided before the exit is still in flight; exiting without waiting further"
                );
            }
        }
    }

    /// [`Self::freeze_and_settle`] from async code: the freeze runs at once
    /// (wait-free), the bounded wait on the blocking pool.
    async fn freeze_and_settle_async(&self) {
        self.freeze();
        let this = self.clone();
        if let Err(error) = tokio::task::spawn_blocking(move || this.freeze_and_settle()).await {
            tracing::warn!(error = %error, "the exit-time accepted-state settle task failed");
        }
    }
}

/// How long an exit waits for a `desired.json` rename already past its
/// freeze check (#367). A rename is a metadata operation; the bound only
/// matters on a stalled filesystem, where the later `active.toml` persist
/// (same directory, same write serial) stalls as it did before #367 and the
/// exit watchdog (#309) bounds the exit.
const ACCEPTED_COMMIT_SETTLE: std::time::Duration = std::time::Duration::from_millis(500);

/// The run's handle on the debounced ADR-W024 Running persister: the spawned
/// task plus the served [`multiview_control::AppState`] it persists from.
struct RunningPersist {
    /// The debounced persist task ([`multiview_control::boot_model::spawn_running_persist`]).
    task: tokio::task::JoinHandle<()>,
    /// The served control-plane state (one set of stores with the router).
    state: multiview_control::AppState,
    /// The network front door (ADR-0107) when `[network]` is configured; kept
    /// for the run's lifetime and dropped at teardown (its runtime shuts down
    /// in the background, never blocking this one).
    front_door: Option<multiview_cli::frontdoor::FrontDoorRun>,
    /// The front-door ACME wiring (ADR-0107 section 5a) when `[network.tls]`
    /// is configured: the ACME service thread lives as long as the run.
    _tls: Option<TlsWiring>,
}

/// The run's ACME wiring: the service plus a shutdown signal for the task
/// that holds the QUIC lane until the certificate is serving.
struct TlsWiring {
    #[cfg(all(feature = "acme", unix, feature = "moq-output"))]
    context: Arc<multiview_control::tls_setup::TlsContext>,
    #[cfg(all(feature = "acme", unix))]
    boot: multiview_cli::tls_boot::TlsBoot,
    #[cfg(all(feature = "acme", unix))]
    shutdown: tokio_util_shutdown::Shutdown,
}

/// A cloneable one-shot shutdown flag (no extra dependency).
#[cfg(all(feature = "acme", unix))]
mod tokio_util_shutdown {
    /// Fires once; every clone observes it.
    #[derive(Clone)]
    pub(crate) struct Shutdown(std::sync::Arc<tokio::sync::watch::Sender<bool>>);
    impl Shutdown {
        pub(crate) fn new() -> Self {
            Self(std::sync::Arc::new(tokio::sync::watch::channel(false).0))
        }
        pub(crate) fn fire(&self) {
            self.0.send_replace(true);
        }
        #[cfg(feature = "moq-output")]
        pub(crate) async fn wait(self) {
            let mut rx = self.0.subscribe();
            let _ = rx.wait_for(|v| *v).await;
        }
    }
}

impl Drop for TlsWiring {
    fn drop(&mut self) {
        #[cfg(all(feature = "acme", unix))]
        {
            self.shutdown.fire();
            multiview_cli::tls_boot::install_tls_context(None);
            // `boot` (the ACME service) drops after this: bounded join (2 s).
            let _ = self.boot.service_finished();
        }
    }
}

/// Start the front-door ACME wiring when `[network.tls]` is configured.
#[allow(clippy::unnecessary_wraps)]
fn start_tls(
    config: &MultiviewConfig,
    publisher: &Arc<EnginePublisher<EngineStateSnapshot, Event>>,
) -> anyhow::Result<Option<TlsWiring>> {
    #[cfg(all(feature = "acme", unix))]
    {
        let Some(boot) = multiview_cli::tls_boot::TlsBoot::start(
            config,
            Some(Arc::clone(publisher)),
            &multiview_cli::tls_boot::BootOptions::default(),
        )?
        else {
            return Ok(None);
        };
        let context = Arc::clone(&boot.context);
        multiview_cli::tls_boot::install_tls_context(Some(Arc::clone(&context)));
        #[cfg(not(feature = "moq-output"))]
        drop(context);
        Ok(Some(TlsWiring {
            #[cfg(feature = "moq-output")]
            context,
            boot,
            shutdown: tokio_util_shutdown::Shutdown::new(),
        }))
    }
    #[cfg(not(all(feature = "acme", unix)))]
    {
        let _ = (config, publisher);
        Ok(None)
    }
}

impl RunningPersist {
    /// Stop the debounced persister at teardown: abort → **await the task's
    /// termination** → one final best-effort persist capturing changes
    /// younger than the debounce (review M2: the ordering restores the
    /// deterministic `.tmp` single-writer guarantee; fail-soft on error).
    /// Returns the front door so the caller drops it **after** the control
    /// server has drained (review PR 315: sessions are cut last, not first).
    async fn finish(self) -> Option<multiview_cli::frontdoor::FrontDoorRun> {
        multiview_control::boot_model::finish_running_persist(self.task, &self.state).await;
        self.front_door
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

#[cfg(feature = "ffmpeg")]
struct LeaseCleanup(
    Option<Arc<multiview_control::source_demand::SwitcherPreviewLeases>>,
    StopSignal,
);
#[cfg(feature = "ffmpeg")]
impl Drop for LeaseCleanup {
    fn drop(&mut self) {
        self.1.stop();
        if let Some(leases) = &self.0 {
            leases.detach();
        }
    }
}

fn main() -> ExitCode {
    // Socket activation (slice 6, #292): adopt the sockets systemd passed and
    // clear `LISTEN_FDS`/`LISTEN_PID` BEFORE the runtime starts any thread
    // (mutating the environment is not thread-safe). The front door claims
    // them when `run` starts it; without activation this is a no-op.
    multiview_cli::frontdoor::set_inherited_sockets(multiview_frontdoor::ListenSockets::from_env());
    match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(async_main()),
        Err(err) => {
            eprintln!("multiview: failed to start the async runtime: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn async_main() -> ExitCode {
    if let Err(err) = init_tracing() {
        // Logging failed to initialize; report on stderr (tracing is unavailable)
        // and bail. This is startup code, not the data plane.
        eprintln!("multiview: failed to initialize logging: {err:#}");
        return ExitCode::FAILURE;
    }

    let cli = Cli::parse();
    // #309: this binary may be ended by the exit watchdog once an exit is
    // decided (a fatal output fault, a stop signal, a failed run).
    multiview_cli::exit_watchdog::enable();
    // Boxed: `dispatch` holds a whole `Pipeline` across awaits (clippy::large_futures).
    match Box::pin(dispatch(cli)).await {
        Ok(code) => code,
        Err(err) => {
            tracing::error!(error = %format!("{err:#}"), "command failed");
            eprintln!("multiview: error: {err:#}");
            // #309: the Tokio runtime is dropped after this returns; anything
            // still running on it is waited for there. Logged so the gap to the
            // process exit is measurable.
            tracing::info!("main returning; runtime shutdown next");
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
        // Heap-pinned once at startup: the run future owns the whole engine
        // + control-plane state and exceeds clippy's large-future bound.
        Command::Run(args) => Box::pin(run_run(args)).await,
        Command::Node(args) => run_node(&args),
        Command::Tls(command) => {
            println!("{}", multiview_cli::tls_cli::run(&command)?);
            Ok(ExitCode::SUCCESS)
        }
        Command::AcknowledgeFallback(args) => run_acknowledge_fallback(&args),
    }
}

/// The `acknowledge-fallback` subcommand (#324): remove the persisted
/// start-up fallback marker, silencing the current `accepted-state-rejected`
/// alarm. A later start that still cannot use the saved configuration raises
/// it again. The saved configuration is not touched.
fn run_acknowledge_fallback(args: &AcknowledgeFallbackArgs) -> anyhow::Result<ExitCode> {
    match multiview_cli::last_good::acknowledge(&args.config).map_err(anyhow::Error::msg)? {
        Some(message) => {
            println!("acknowledged the start-up fallback warning: {message}");
            println!(
                "the saved configuration is unchanged. The warning is raised again on every \
                 start that still cannot use it, until it is fixed or replaced; a start that \
                 uses it clears the warning by itself. A running engine's banner is cleared \
                 only through the API (POST /api/v1/health/accepted-state-rejected/ack)."
            );
        }
        None => println!("no start-up fallback warning is pending"),
    }
    Ok(ExitCode::SUCCESS)
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
    let start = multiview_cli::boot::resolve_start_config(boot, boot_text, &args.config)
        .map_err(anyhow::Error::msg)?;

    // A configured `[timing].ptp_phc` in a build without the `ptp` feature is
    // a capability this binary cannot provide: fail the run at startup with a
    // clear error (the DEV-B1 display-output fail-fast precedent) — never
    // silently ride the system clock while the config asks for a PHC.
    multiview_cli::timing_gate::ensure_ptp_phc_supported(start.running.timing.as_ref())
        .map_err(|reason| anyhow::anyhow!(reason))?;
    // `[network.tls]` in a build without `acme` cannot provide certificates:
    // a hard startup error, never a silent plain-HTTP front door.
    multiview_cli::tls_cli::ensure_acme_supported(&start.running)
        .map_err(|reason| anyhow::anyhow!(reason))?;

    if args.software {
        return Box::pin(run_software(&start, &args)).await;
    }

    // Run-owned: retain across configuration parses and replacement pipelines.
    #[cfg(feature = "moq-output")]
    let preview_budget = multiview_cli::preview_vertical::PreviewAdmissionBudget::new();
    Box::pin(run_pipeline(
        &start,
        &args,
        #[cfg(feature = "moq-output")]
        &preview_budget,
    ))
    .await
}

/// The FFmpeg-free software run: the output-clock + CPU compositor driving the
/// built-in test-pattern sources (the software end-to-end smoke of the
/// output-clock invariant), serving the API/WebUI just like the full build.
async fn run_software(
    start: &multiview_cli::boot::StartConfig,
    args: &RunArgs,
) -> anyhow::Result<ExitCode> {
    // Conspect entitlement plane (ADR-0050): assemble the shared lease store +
    // published ladder level from the environment, then gate the NEW engine build
    // (S1). A running engine is NEVER re-gated; this only refuses a *new* start at
    // the block-new-instance rung (the never-off-air promise).
    let plane = multiview_cli::licence::EntitlementPlane::from_env();
    // #317/#324: build from the start decision; a STATE-class build failure on
    // the accepted state falls back to a different last-good that builds, an
    // environment-class one (an expired lease) is retried and then fails. Only
    // AFTER a successful build is a planned fallback recorded (a kept copy +
    // the persisted marker; desired.json itself is never changed).
    let (start, mut engine) = multiview_cli::boot::build_with_fallback(
        start.clone(),
        &args.config,
        &multiview_cli::boot::RetryPolicy::startup(),
        |config| {
            SoftwareEngine::build_gated(config, plane.level()).map_err(|e| {
                multiview_cli::boot::BuildFailure::new(e.failure_class(), e.to_string())
            })
        },
    )
    .map_err(anyhow::Error::msg)?;
    let start =
        multiview_cli::boot::commit_fallback(start, &args.config).map_err(anyhow::Error::msg)?;
    let start = &start;
    let cadence = engine.cadence();
    let report = if let Some(ticks) = args.tick_budget(cadence) {
        tracing::info!(ticks, "software run: bounded");
        engine
            .run_for_realtime(ticks)
            .await
            .context("software bounded run")?
    } else {
        tracing::info!("software run: until Ctrl-C");
        {
            let run = run_software_until_ctrl_c(&mut engine, start, &plane, &args.config).await;
            if run.is_err() {
                // #309: a failed run is an exit decision.
                multiview_cli::exit_watchdog::arm(multiview_cli::exit_watchdog::ExitCause::Fatal);
            }
            run?
        }
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
    #[cfg(feature = "moq-output")]
    preview_budget: &multiview_cli::preview_vertical::PreviewAdmissionBudget,
) -> anyhow::Result<ExitCode> {
    use multiview_cli::pipeline::Pipeline;

    // The pipeline is built from the starting Running document (ADR-W024).
    // Conspect entitlement plane (ADR-0050): assemble the shared store + ladder
    // level from the environment, then gate the NEW pipeline build (S1) — refuse a
    // new start at the block-new-instance rung. A running pipeline is never
    // re-gated (the never-off-air promise).
    let plane = multiview_cli::licence::EntitlementPlane::from_env();
    multiview_cli::run::start_gate(plane.level()).map_err(|e| anyhow::anyhow!("{e}"))?;
    // #317/#324: build from the start decision; a STATE-class build failure on
    // the accepted state falls back to a different last-good that builds. An
    // environment-class one (`PipelineError::Io` classified by its OS error
    // kind — e.g. permission denied / a full volume creating the HLS dir — or a
    // refused thread spawn) is retried on the same state and then fails: a
    // fault that breaks every state never falls back. Only AFTER a successful
    // build is a planned fallback recorded (a kept copy + the persisted marker;
    // desired.json itself is never changed). No socket binds during the build:
    // the control-plane/MoQ listeners bind later, so EADDRINUSE there fails
    // start-up and never reaches this decision.
    let (start, built) = multiview_cli::boot::build_with_fallback(
        start.clone(),
        &args.config,
        &multiview_cli::boot::RetryPolicy::startup(),
        |config| {
            Pipeline::build(config).map_err(|e| {
                multiview_cli::boot::BuildFailure::new(
                    e.failure_class(),
                    format!("building the pipeline: {e}"),
                )
            })
        },
    )
    .map_err(anyhow::Error::msg)?;
    let start =
        multiview_cli::boot::commit_fallback(start, &args.config).map_err(anyhow::Error::msg)?;
    let start = &start;
    // Wire the wait-free tile-watermark signal (S3) into the pipeline's overlay
    // bake (a no-op without the `overlay` feature). The bake samples it off the
    // hot loop; it can never stall the output clock (invariant #1).
    let mut pipeline = built.with_watermark_signal(plane.signal.clone());
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
        // Boxed: the run future carries the whole control-plane wiring
        // (clippy::large_futures; the front-door handle grew it past 16 KiB).
        let run = Box::pin(run_pipeline_until_ctrl_c(
            pipeline,
            start,
            &plane,
            &args.config,
            #[cfg(feature = "moq-output")]
            preview_budget,
        ))
        .await;
        if run.is_err() {
            // #309: a failed run (startup or engine) is an exit decision; bound
            // the runtime drop that follows `main`.
            multiview_cli::exit_watchdog::arm(multiview_cli::exit_watchdog::ExitCause::Fatal);
        }
        run?
    };

    println!("{}", report.render());
    Ok(if report.faltered {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// Preserve the existing fail-soft boot snapshot persistence policy.
fn persist_loaded_best_effort(boot_model: &multiview_control::boot_model::BootModel) {
    if let Err(reason) = multiview_control::boot_model::persist_loaded(boot_model) {
        tracing::warn!(reason = %reason, "could not persist the Loaded snapshot (fail-soft; the in-memory snapshot is authoritative)");
    }
}

/// The per-run-path inputs the control plane is wired from: live preview taps,
/// shared producer stop registry, and supported live-apply capabilities.
/// Only the binary knows both compiled features and the selected run path.
struct ControlPlaneWiring {
    #[cfg(feature = "moq-output")]
    moq_factory: Option<Arc<dyn multiview_control::moq_provider::AuthorizedFeedFactory>>,
    #[cfg(feature = "moq-output")]
    moq_slot: Option<multiview_cli::output_window_registry::WindowSubscriptionSlot>,
    /// The shared program-frame slot the run loop fills for previews.
    program_slot: multiview_cli::preview::ProgramSlot,
    switcher_preview: Option<multiview_cli::preview::ProgramSlot>,
    switcher: Option<multiview_engine::switcher_control::SwitcherControl>,
    switcher_leases: Option<Arc<multiview_control::source_demand::SwitcherPreviewLeases>>,
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
    /// Read-only source event snapshots from the running ingest pipeline.
    source_events: Option<multiview_cli::source_event_runtime::RuntimeReader>,
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
#[cfg(feature = "moq-output")]
async fn start_moq_listener(
    factory: Option<Arc<dyn multiview_control::moq_provider::AuthorizedFeedFactory>>,
    front_door: Option<&mut multiview_cli::frontdoor::FrontDoorRun>,
    tls: Option<&TlsWiring>,
) -> anyhow::Result<Option<Arc<multiview_control::moq::MoqRegistry>>> {
    #[cfg(all(feature = "acme", unix))]
    if let (Some(door), Some(tls)) = (front_door.as_ref(), tls) {
        // ADR-0107 section 5a: with `[network.tls]` the front-door QUIC lane
        // serves the ACME certificate and is NOT started while in setup. The
        // lane is taken now (so nothing else binds it); MoQ starts from a
        // control-plane task once the source is serving. Until then every MoQ
        // lease answers the existing honest 503 (provider unavailable).
        let _ = door;
        let Some((endpoint, protocol)) =
            multiview_control::moq_startup::MoqListenerConfig::endpoint_from_env()
                .map_err(|_| anyhow::anyhow!("invalid explicit MoQ listener configuration"))?
        else {
            return Ok(None);
        };
        let door = front_door.ok_or_else(|| anyhow::anyhow!("front door unavailable"))?;
        let socket = door
            .take_moq_socket()
            .ok_or_else(|| anyhow::anyhow!("front-door QUIC lane unavailable"))?;
        let deferred = Arc::new(multiview_control::moq_startup::DeferredProvider::default());
        let registry = Arc::new(multiview_control::moq::MoqRegistry::new(
            deferred.clone(),
            std::time::Duration::from_secs(30),
            16,
        ));
        tokio::spawn(multiview_cli::frontdoor::start_moq_when_serving(
            tls.context.clone(),
            tls.shutdown.clone().wait(),
            endpoint,
            protocol,
            factory,
            socket,
            deferred,
        ));
        return Ok(Some(registry));
    }
    #[cfg(not(all(feature = "acme", unix)))]
    let _ = tls;
    if let Some(door) = front_door {
        // ADR-0107 slice 1: MoQ rides the front-door socket's QUIC lane.
        let Some(config) =
            multiview_control::moq_startup::MoqListenerConfig::from_env_shared(door.local_addr())
                .map_err(|_| anyhow::anyhow!("invalid explicit MoQ listener configuration"))?
        else {
            return Ok(None);
        };
        let socket = door
            .take_moq_socket()
            .ok_or_else(|| anyhow::anyhow!("front-door QUIC lane unavailable"))?;
        return Ok(Some(
            multiview_control::moq_startup::start_shared(config, factory, socket)
                .await
                .map_err(|_| anyhow::anyhow!("MoQ listener startup failed"))?,
        ));
    }
    match multiview_control::moq_startup::MoqListenerConfig::from_env()
        .map_err(|_| anyhow::anyhow!("invalid explicit MoQ listener configuration"))?
    {
        Some(config) => Ok(Some(
            multiview_control::moq_startup::start(config, factory)
                .await
                .map_err(|_| anyhow::anyhow!("MoQ listener startup failed"))?,
        )),
        None => Ok(None),
    }
}

// reason: `whep` (WHEP preview egress) and `whip` (WHIP ingest) are the canonical
// WebRTC protocol names — distinct transports, not a typo-similar pair; renaming
// either would obscure the spec mapping.
#[allow(clippy::similar_names)]
#[allow(clippy::too_many_lines)]
// reason: a flat, linear control-plane wiring sequence — one documented block per
// optional surface (switcher M/E, WHEP preview, WHIP ingest, WHEP-serve output,
// discovery, licence, mesh). The first-slice M/E wiring pushed it past the
// ceiling; extracting one-use helpers would only scatter the wiring.
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
        #[cfg(feature = "moq-output")]
        moq_factory,
        #[cfg(feature = "moq-output")]
        moq_slot,
        program_slot,
        switcher_preview,
        switcher,
        switcher_leases,
        stores,
        registry,
        ingest,
        live_apply,
        licence,
        mesh,
        source_events,
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
    // The off-clock wall publishes one newest picture for WHEP as well as its
    // optional MoQ rendition. The picture slot is never the Program slot.
    #[cfg(feature = "moq-output")]
    let wall_setup = if let (Some(wall), Some(slot)) = (
        multiview_cli::monitor_wall::WallConfig::from_env(),
        moq_slot.clone(),
    ) {
        let geometry = multiview_cli::preview::wall_slot();
        let picture = multiview_cli::preview::wall_picture_slot();
        match multiview_cli::monitor_wall::spawn(
            wall,
            config.canvas.fps.rational(),
            Arc::clone(&shared_stores),
            slot,
            Arc::clone(&geometry),
            Arc::clone(&picture),
        ) {
            Ok(worker) => Some((geometry, picture, worker)),
            Err(error) => {
                tracing::warn!(%error, "monitor wall worker could not start");
                None
            }
        }
    } else {
        None
    };
    #[cfg(all(feature = "moq-output", feature = "webrtc-native"))]
    let wall_picture = wall_setup
        .as_ref()
        .map(|(_, picture, _)| Arc::clone(picture));
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
    // The network front door (ADR-0107 slice 1): with `[network]`, ONE
    // dual-stack UDP socket for MoQ and WebRTC, on its own runtime. Without it,
    // both keep their legacy sockets unchanged.
    // Front-door ACME (ADR-0107 section 5a): bootstrap certificate + ACME
    // service + setup code, before the control plane binds. Startup only, on
    // this control runtime; the output clock never waits on it (the service
    // runs on its own thread + runtime).
    let tls = start_tls(config, publisher)?;
    #[allow(unused_mut)]
    let mut front_door = multiview_cli::frontdoor::FrontDoorRun::start(config);
    #[cfg(feature = "webrtc-native")]
    let (whep, whip, whep_output) = {
        let rtc_lane = front_door
            .as_mut()
            .and_then(|door| Some((door.take_rtc_lane()?, door.handle())));
        // ICE-TCP rides the front-door UDP lane's endpoint only.
        let ice_tcp_lane = if rtc_lane.is_some() {
            front_door
                .as_mut()
                .and_then(multiview_cli::frontdoor::FrontDoorRun::take_ice_tcp_lane)
        } else {
            None
        };
        let unified = multiview_cli::webrtc_unified::spawn_unified_webrtc(
            config,
            Arc::clone(&program_slot),
            Arc::clone(&shared_stores),
            program_audio,
            webrtc_registry,
            &egress_registry,
            multiview_cli::source_demand::installed_control_demand(),
            #[cfg(feature = "moq-output")]
            wall_picture,
            rtc_lane,
            ice_tcp_lane,
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
    let mut provider = multiview_cli::preview::CliPreviewProvider::new(program_slot, shared_stores);
    if let Some(preview) = switcher_preview {
        provider = provider.with_switcher_preview(preview);
    }
    // The shared monitor wall (ADR-0103): off unless `MULTIVIEW_MOQ_WALL=1`.
    // One off-clock worker composes and encodes it once for every viewer.
    #[cfg(feature = "moq-output")]
    if let Some((geometry, _picture, worker)) = wall_setup {
        provider = provider.with_wall(geometry, Box::new(worker));
    }
    let provider: multiview_control::SharedPreview = Arc::new(provider);
    // The Boot/Loaded/Running model (ADR-W024): Loaded is the immutable boot
    // snapshot; the model backs `GET /api/v1/config/boot-model` and the
    // revert-to-start/promote actions, and is the persister's target.
    let boot_model = Arc::new(start.to_boot_model(config_path));
    #[cfg(feature = "moq-output")]
    let moq = start_moq_listener(moq_factory, front_door.as_mut(), tls.as_ref()).await?;
    let (addr, handle, state) = control::bind_and_serve_with_moq(
        listen,
        config,
        Arc::clone(publisher),
        commands,
        provider,
        switcher,
        switcher_leases,
        whep,
        whip,
        whep_output,
        licence,
        mesh,
        live_apply,
        live_capability,
        Some(Arc::clone(&boot_model)),
        source_events,
        #[cfg(feature = "moq-output")]
        moq,
        #[cfg(feature = "moq-output")]
        moq_slot,
        front_door.as_ref(),
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
    persist_loaded_best_effort(&boot_model);
    // The debounced Running persister (ADR-W024 §3): waits on the ONE
    // `running_changed` choke-point signal `AppState::audit` fires, then writes
    // `active.toml` atomically. Control-plane file I/O only (inv #10).
    let persist = RunningPersist {
        task: multiview_control::boot_model::spawn_running_persist(
            state.clone(),
            RUNNING_PERSIST_DEBOUNCE,
        ),
        state: state.clone(),
        front_door,
        _tls: tls,
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
    let watch = start_config_watch(start, config_path, &state);
    Ok((handle, command_rx, hub, watch, persist))
}

fn start_config_watch(
    start: &multiview_cli::boot::StartConfig,
    config_path: &Path,
    state: &multiview_control::AppState,
) -> multiview_cli::config_watch::ConfigWatchHandle {
    let mut watch_options = multiview_cli::config_watch::WatchOptions::default()
        .with_initial_observed(start.boot_text.clone());
    if let Some(installed) = state.watch_handle() {
        watch_options = watch_options.with_handle(installed);
    }
    multiview_cli::config_watch::spawn(
        config_path.to_path_buf(),
        start.running.clone(),
        state.clone(),
        watch_options,
    )
}

/// Attach the run's declared first-slice Program/Preview M/E (`[switcher]`) and
/// relay its engine-owned outcomes onto the shared publisher. A refused boot
/// lease is a warning, never a failure: Program, the output clock and every other
/// output are unaffected, and Preview stays explicitly unavailable rather than
/// silently copying Program. Returns `None` when the document declares no M/E
/// (the legacy one-canvas run path, byte-identical to today).
#[cfg(feature = "ffmpeg")]
fn attach_run_switcher(
    pipeline: &mut multiview_cli::pipeline::Pipeline,
    demand: Option<multiview_control::SharedSourceDemand>,
    publisher: &Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    stop: &StopSignal,
) -> anyhow::Result<Option<multiview_cli::switcher_runtime::FirstSliceSwitcher>> {
    let Some(attached) = multiview_cli::switcher_runtime::attach_first_slice(pipeline, demand)?
    else {
        return Ok(None);
    };
    if let Some(warning) = &attached.boot_warning {
        tracing::warn!(
            me = %attached.me_id,
            program = %attached.program,
            warning = %warning,
            "switcher preview unavailable at boot"
        );
    }
    tokio::spawn(
        multiview_control::switcher_events::forward_switcher_outcomes(
            attached.control.outcomes(),
            Arc::clone(publisher),
            attached.me_id.clone(),
            attached.program.clone(),
            Arc::clone(&attached.leases),
        ),
    );
    // The correlated relay above is the primary lease resolver, but it can die
    // or lag; this off-clock, bounded sweep reconciles any accepted-but-
    // unresolved selection against the engine's own applied snapshot so a
    // preview demand can never be leaked forever. It stops with the run.
    tokio::spawn(multiview_control::switcher_events::sweep_switcher_leases(
        attached.control.clone(),
        Arc::clone(&attached.leases),
        stop.clone(),
        multiview_control::switcher_events::SWEEPER_INTERVAL,
    ));
    Ok(Some(attached))
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
    // `mut` on every build: the first-slice switcher attach (`attach_run_switcher`)
    // mutates the pipeline unconditionally, in addition to the `webrtc-native`
    // program-audio tap and the `moq-output` preview configuration.
    mut pipeline: multiview_cli::pipeline::Pipeline,
    start: &multiview_cli::boot::StartConfig,
    plane: &multiview_cli::licence::EntitlementPlane,
    config_path: &Path,
    #[cfg(feature = "moq-output")]
    preview_budget: &multiview_cli::preview_vertical::PreviewAdmissionBudget,
) -> anyhow::Result<multiview_cli::pipeline::PipelineReport> {
    // The Running document seeds the stores/drain; the boot file (config_path)
    // stays the watch + promote target (ADR-W024).
    let config = &start.running;
    let journal_limits = config.events.unwrap_or_default();
    multiview_cli::event_journal::start(
        multiview_control::boot_model::state_dir_for(config_path).join("events"),
        multiview_control::event_journal::JournalLimits {
            retention_days: journal_limits.journal_retention_days,
            max_bytes: journal_limits.journal_max_bytes,
        },
    );
    let stop = StopSignal::new();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let preview_slot = multiview_cli::preview::program_slot();
    // First-slice M/E wiring is installed once this run's demand provider exists
    // (the switcher's warm-on-arm PVW lease must target the same manager the
    // control plane serves), so the attach sits inside the control block below.
    // The `else` arm after it keeps a control-less run with `[switcher]` on the
    // same wiring minus the lease.
    let mut switcher_leases = None;
    let mut switcher_preview_slot = None;
    let mut switcher_attached = false;
    let mut lease_cleanup = LeaseCleanup(None, stop.clone());
    let mut attached_switcher: Option<multiview_cli::switcher_runtime::FirstSliceSwitcher> = None;
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

    let (shutdown_tx, outer_shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    // #309: the control server's drain starts at the end-of-run shutdown OR as
    // soon as an exit is decided (`early_drain`), whichever comes first, so the
    // drain gets the exit bound instead of starting after the source teardown.
    let early_drain = std::sync::Arc::new(tokio::sync::Notify::new());
    let (drain_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    {
        let early_drain = std::sync::Arc::clone(&early_drain);
        tokio::spawn(async move {
            tokio::select! {
                _ = outer_shutdown_rx => {}
                () = early_drain.notified() => {}
            }
            let _ = drain_tx.send(());
        });
    }
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
        // Audio routing applies to the RUNNING program bus (no restart): the
        // hook loads the run's audio control handle (published at drive start;
        // empty when this run carries no program audio → honest `restart`) and
        // hands it the new gain table, which the bake consumer ramps in at its
        // next tick. Wait-free: a SwapCell load + store (inv #1/#10).
        let live_apply = live_apply.with_audio_routing(
            multiview_cli::audio::routing_live_capability(pipeline.audio_route_slot()),
        );
        #[cfg(feature = "moq-output")]
        multiview_cli::moq_outputs::metadata::configure_optional_preview(
            &mut pipeline,
            preview_budget,
        )
        .map_err(|_| anyhow::anyhow!("explicit measured MoQ preview configuration rejected"))?;
        // Demand (docs/architecture/demand-driven-sources.md): build the manager
        // before the hub and drain so live edits catalogue instead of spawning,
        // and every consumer below takes leases from the same manager.
        let demand = pipeline.demand();
        // Every source lifecycle transition rides the existing event path as
        // `input.lifecycle` (topic `inputs`). The publisher is a bounded
        // drop-oldest ring, so a slow subscriber never stalls the manager.
        {
            let events = Arc::clone(&publisher);
            demand.set_transition_sink(Arc::new(move |event| {
                events.publish_event(multiview_cli::source_demand::lifecycle_event(event));
            }));
        }
        multiview_cli::output_routing_control::install_output_routing(Some(
            pipeline.output_routing(),
        ));
        multiview_cli::output_routing_control::install_program_metrics(Some(
            pipeline.program_metrics(),
        ));
        multiview_cli::source_demand::install_control_demand(Some(Arc::new(
            multiview_cli::source_demand::ControlDemand::new(demand, pipeline.catalogue()),
        )));
        // The declared first-slice M/E is attached HERE, after the demand
        // provider is installed, so its boot lease warms the configured PVW
        // source through the manager this run actually serves. A lease refusal
        // is a warning, never a startup failure: Program keeps composing.
        if let Some(attached) = attach_run_switcher(
            &mut pipeline,
            multiview_cli::source_demand::installed_control_demand(),
            &publisher,
            &stop,
        )? {
            lease_cleanup.0 = Some(Arc::clone(&attached.leases));
            switcher_leases = Some(Arc::clone(&attached.leases));
            switcher_preview_slot = Some(Arc::clone(&attached.preview_slot));
            switcher_attached = true;
            attached_switcher = Some(attached);
        }
        let (handle, command_rx, hub, watch, persist) = serve_control_plane(
            &cfg.listen,
            start,
            config_path,
            &publisher,
            ControlPlaneWiring {
                #[cfg(feature = "moq-output")]
                moq_factory: Some(pipeline.moq_feed_factory()),
                #[cfg(feature = "moq-output")]
                moq_slot: Some(pipeline.window_subscriptions()),
                program_slot: Arc::clone(&preview_slot),
                switcher_preview: switcher_preview_slot.clone(),
                switcher: pipeline.switcher_control(),
                switcher_leases: switcher_leases.clone(),
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
                source_events: Some(pipeline.source_event_reader()),
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
        // shared (`SwapCell`); the run publishes its handle into it at
        // drive start, and the drain reads it wait-free (inv #1/#10). Only under
        // `overlay` (without it the run renders no subtitles, so there is no layer).
        // The live overlay seam (ADR-W022) rides the same variant; the
        // live-source seam (ADR-W018) rides both.
        #[cfg(feature = "overlay")]
        let drain: ControlDrain = Box::new(control::command_drain_with_demand(
            command_rx,
            config.clone(),
            Arc::clone(&publisher),
            pipeline.subtitle_route_slot(),
            pipeline.audio_route_slot(),
            pipeline.overlay_apply_slot(),
            pipeline.tile_spec_apply_slot(),
            pipeline.audio_free_sources_slot(),
            hub.handle(),
            pipeline.player_mailboxes(),
            pipeline.display_layout_slot(),
            pipeline.drain_demand(),
        ));
        #[cfg(not(feature = "overlay"))]
        let drain: ControlDrain = Box::new(control::command_drain_with_audio_players_and_demand(
            command_rx,
            config.clone(),
            Arc::clone(&publisher),
            pipeline.audio_route_slot(),
            hub.handle(),
            pipeline.player_mailboxes(),
            pipeline.display_layout_slot(),
            pipeline.drain_demand(),
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
    // A run with `[switcher]` but no control listener still gets the same real
    // M/E wiring — the frame hook drives Program exactly as the API-served run
    // does, instead of silently falling back to a single bus. No lease owner or
    // monitor tap is published because nothing serves them; the boot lease is
    // skipped honestly (the configured PVW source is warmed by whoever selects
    // it, which in this run means nobody).
    if !switcher_attached {
        attached_switcher = attach_run_switcher(&mut pipeline, None, &publisher, &stop)?;
    }

    let stop_for_signal = stop.clone();
    // #367: the config-file watcher stops committing the moment an exit is
    // decided, not at the late tenant teardown: a boot-file edit seen during
    // shutdown must never be written over the accepted state.
    let freeze_for_signal = ExitFreeze::new(
        config_watch.as_ref(),
        running_persist.as_ref().map(|p| &p.state),
    );
    let signal = tokio::spawn(async move {
        let which = await_shutdown_signal().await;
        // #309: decide first, log second.
        stop_for_signal.stop();
        freeze_for_signal.freeze();
        multiview_cli::exit_watchdog::arm(multiview_cli::exit_watchdog::ExitCause::Stop);
        tracing::info!(
            signal = which,
            "stop signal received; stopping after the current frame"
        );
    });
    // #309: the durable shutdown work must land inside the exit bound, so it
    // starts the moment ANY exit is decided (a stop signal or a fatal output
    // fault), concurrently with the pipeline teardown — not after the slow
    // source teardown. The ordinary end-of-run steps still run if there is time
    // (the persist writes the same Running state again; idempotent).
    let early_persist_state = running_persist.as_ref().map(|p| p.state.clone());
    let decided_drain = std::sync::Arc::clone(&early_drain);
    let decided_freeze = ExitFreeze::new(
        config_watch.as_ref(),
        running_persist.as_ref().map(|p| &p.state),
    );
    // A dedicated OS thread with its own small runtime: the main runtime's
    // workers can be busy with the synchronous pipeline teardown (a task parked
    // behind it would miss the bound), and this work must not depend on them.
    let decided_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let decided_stop_flag = Arc::clone(&decided_stop);
    let decided = std::thread::Builder::new()
        .name("multiview-exit-durable".to_owned())
        .spawn(move || {
            while multiview_cli::exit_watchdog::armed().is_none() {
                if decided_stop_flag.load(std::sync::atomic::Ordering::Acquire) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            // #367: no config-file commit after an exit is decided (a fatal
            // output fault as well as a stop signal). This OS thread runs
            // after the watchdog is armed, so the bounded settle is safe here.
            decided_freeze.freeze_and_settle();
            // Begin the control plane's graceful drain now (the deadline cuts it).
            decided_drain.notify_one();
            // Journal counters first (bounded wait for the worker to apply it):
            // the persist below can be the slower step.
            if !multiview_cli::event_journal::flush_and_wait(std::time::Duration::from_millis(500))
            {
                tracing::warn!("the exit-time journal flush was not applied in time");
            }
            if let Some(state) = early_persist_state {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => match rt
                        .block_on(multiview_control::boot_model::persist_running_now(&state))
                    {
                        Ok(()) => tracing::info!("exit-time running-state persist written"),
                        Err(error) => tracing::warn!(
                            error = %error,
                            "the exit-time running-state persist was skipped (fail-soft)"
                        ),
                    },
                    Err(error) => {
                        tracing::warn!(error = %error, "no runtime for the exit-time persist");
                    }
                }
            }
        })
        .map_err(|error| {
            tracing::error!(
                error = %error,
                "could not start the exit-durable thread; the exit-time drain, persist and journal flush will not run early"
            );
        })
        .ok();

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
    // polls per-GPU load (the `LoadPoller → SwapCell` snapshot), observes the
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

    // MC-6 (ADR-0040): multicast networking is advisory, detected during startup
    // before the output clock is constructed and published via the drop-oldest lane.
    {
        let env = multiview_cli::network_warn::current_environment();
        let mismatch = (multiview_cli::network_warn::in_container(&env)
            && multiview_cli::network_warn::has_container_bridge_networking(&env)
            && multiview_cli::network_warn::config_uses_multicast(config))
        .then_some(multiview_control::NetworkMismatchView);
        let since_nanos = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        )
        .unwrap_or(i64::MAX);
        let _ = multiview_control::emit_network_warnings(publisher.as_ref(), mismatch, since_nanos);
    }
    // #317/#324: this start's fallback, or a persisted one not yet acknowledged.
    let _raised = multiview_cli::last_good::announce(config_path, start, publisher.as_ref());
    let _last_good = spawn_last_good_recorder(start, config_path, &publisher, &stop);

    // MP-1 (ADR-0030 §2.2): build the engine `ProgramSet` and drive this single
    // program (id "main") through it — behaviour-identical to today (one program,
    // the same drive/stop/publisher/preview/drain). See `drive_main_program_in_set`.
    let exit_freeze = ExitFreeze::new(
        config_watch.as_ref(),
        running_persist.as_ref().map(|p| &p.state),
    );
    let report =
        drive_main_program_in_set(pipeline, cadence, &stop, &publisher, &preview_slot, drain).await;
    // #367: the drive loop returned (a stop, a fatal fault, or an error): the
    // run is exiting, so no config-file commit may write the accepted state
    // from here on, whatever made it return. The settle (bounded) lets a
    // rename decided before the freeze finish before the teardown.
    exit_freeze.freeze_and_settle_async().await;
    let report = report?;
    let tenants_start = std::time::Instant::now();

    // The run is over: release this generation's preview demand explicitly
    // (the sweeper has stopped on the same signal, so nothing can re-adopt a
    // cancelled selection), then tear the shared tenants down.
    if let Some(attached) = &attached_switcher {
        attached.leases.detach();
    }

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
    tracing::info!(
        stage = "run-tenants",
        elapsed_ms = u64::try_from(tenants_start.elapsed().as_millis()).unwrap_or(u64::MAX),
        "teardown stage done"
    );
    // Ask the journal thread to persist pending repeat counters. Not joined, so
    // a counter still queued at process exit can be lost; its record is not.
    multiview_cli::event_journal::flush();
    // A clean, undecided finish: let the durable thread end (it never armed).
    decided_stop.store(true, std::sync::atomic::Ordering::Release);
    drop(decided);
    Ok(report)
}

/// #317: record the started accepted state as last-good after the first tick
/// (see [`multiview_cli::last_good::spawn_recorder`]).
fn spawn_last_good_recorder(
    start: &multiview_cli::boot::StartConfig,
    config_path: &Path,
    publisher: &Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    stop: &StopSignal,
) -> Option<tokio::task::JoinHandle<()>> {
    let started = start.desired.clone()?;
    let publisher = Arc::clone(publisher);
    Some(multiview_cli::last_good::spawn_recorder(
        config_path.to_path_buf(),
        started,
        move || publisher.state.sequence(),
        stop.clone(),
    ))
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
    let shutdown_start = std::time::Instant::now();
    programs.shutdown().await;
    tracing::info!(
        stage = "programset-shutdown",
        elapsed_ms = u64::try_from(shutdown_start.elapsed().as_millis()).unwrap_or(u64::MAX),
        "teardown stage done"
    );
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
    // #317: this readiness report starts nothing, so it never commits a
    // fallback; it must not report a clean "ready" for a saved state a real
    // start would reject.
    if let Some(reason) = start.pending_fallback_reason() {
        anyhow::bail!(
            "the accepted state would be rejected at start ({reason}); a real start would fall \
             back to the last-good copy and raise the accepted-state-rejected warning"
        );
    }
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
                #[cfg(feature = "moq-output")]
                moq_factory: None,
                #[cfg(feature = "moq-output")]
                moq_slot: None,
                program_slot: engine.program_preview(),
                switcher_preview: None,
                switcher: None,
                switcher_leases: None,
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
                source_events: None,
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
    // #367: the config-file watcher stops committing the moment an exit is
    // decided, not at the late tenant teardown: a boot-file edit seen during
    // shutdown must never be written over the accepted state.
    let freeze_for_signal = ExitFreeze::new(
        config_watch.as_ref(),
        running_persist.as_ref().map(|p| &p.state),
    );
    let signal = tokio::spawn(async move {
        let which = await_shutdown_signal().await;
        // #309: decide first, log second.
        stop_for_signal.stop();
        freeze_for_signal.freeze();
        multiview_cli::exit_watchdog::arm(multiview_cli::exit_watchdog::ExitCause::Stop);
        tracing::info!(
            signal = which,
            "stop signal received; stopping after the current frame"
        );
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
    // #317/#324: after the control plane's warning ingest subscribed — this
    // start's fallback, or a persisted one not yet acknowledged.
    let _raised = multiview_cli::last_good::announce(config_path, start, publisher.as_ref());
    let _last_good = spawn_last_good_recorder(start, config_path, &publisher, &stop);

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

    // #367: the software run has no exit-durable thread; freeze the accepted
    // state the moment ANY exit is decided (a stop, or a fatal arm anywhere),
    // not only once the engine loop has returned and its teardown finished.
    // Off the clock: a 5 ms poll of two atomics on a control-plane task.
    let decided = {
        let freeze = ExitFreeze::new(
            config_watch.as_ref(),
            running_persist.as_ref().map(|p| &p.state),
        );
        let stop = stop.clone();
        tokio::spawn(async move {
            while !stop.is_stopped() && multiview_cli::exit_watchdog::armed().is_none() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            freeze.freeze();
        })
    };
    let report = engine
        .run_until_stopped_with_control(&stop, publisher.as_ref(), drain)
        .await;
    decided.abort();
    // #367: the engine loop returned (a stop or an error): the run is exiting,
    // so no config-file commit may write the accepted state from here on.
    ExitFreeze::new(
        config_watch.as_ref(),
        running_persist.as_ref().map(|p| &p.state),
    )
    .freeze_and_settle_async()
    .await;
    if report.is_err() {
        // #309: an engine error is an exit decision.
        multiview_cli::exit_watchdog::arm(multiview_cli::exit_watchdog::ExitCause::Fatal);
    }
    let report = report.context("headless run until Ctrl-C")?;

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
    let front_door = match running_persist {
        Some(persist) => persist.finish().await,
        None => None,
    };
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
    // Last: the front door's sessions outlive the control-plane drain. Its
    // runtime shuts down on a blocking-pool thread, bounded, so this never
    // blocks a worker of this runtime.
    if let Some(door) = front_door {
        door.shutdown().await;
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
