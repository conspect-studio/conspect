//! Wiring the management control plane into `multiview run`.
//!
//! When the loaded config carries a `[control]` section, the run path binds that
//! address and serves the [`multiview_control`] router — REST + WebSocket + SSE,
//! the `OpenAPI`/Scalar docs at `/docs`, and (when the control plane is built with
//! `embed-web`) the web UI — alongside the engine, via
//! [`bind_and_serve`]. The server is a best-effort sibling task: it only reads
//! the engine's wait-free latest-state slot and drop-oldest event broadcast and
//! submits to the non-blocking command bus, so it is **physically incapable of
//! back-pressuring the engine** (invariant #10). It drains and stops gracefully
//! when the same shutdown signal the engine watches is raised.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use multiview_compositor::pipeline::Nv12Image;
use multiview_config::MultiviewConfig;
use multiview_control::{
    provision_admin_keys, run_warning_ingest, AppState, Command, CommandReceiver, CommandSender,
    EngineStateSnapshot, InMemoryRepository, InMemoryWarningStore, ResolvedLayout, SharedPreview,
    WarningRepository,
};
use multiview_engine::{
    CompositorDrive, EnginePublisher, RouteApplier, RouteIntent, RouteResolution,
};
use multiview_events::{
    Event, JobProgress, OutputRunState, OutputStatus, SalvoEvent, SalvoPhase, TallyEvent,
};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// Bind `listen` and serve the control plane over it on a background task,
/// shutting down gracefully when `shutdown` resolves.
///
/// Returns the **actual** bound [`SocketAddr`] (so a `:0` ephemeral bind can be
/// logged, or used by a test) and the server task's [`JoinHandle`]. The server
/// shares the engine's outbound `publisher` (read-only: the wait-free state slot
/// and drop-oldest event broadcast) and the inbound, non-blocking `commands` bus,
/// neither of which can stall the engine (invariant #10).
///
/// Access is provisioned with a bootstrap **admin** key
/// ([`provision_admin_keys`]): the unauthenticated surface (`/docs`,
/// `/api/v1/openapi.json`, and — with `embed-web` — the web UI shell) is always
/// reachable, while every API route requires the admin token. The admin secret
/// comes from the `MULTIVIEW_CONTROL_TOKEN` environment variable (stable across
/// restarts, no secret in config); if unset, a random token is generated and
/// **logged once** for first access. Finer-grained config-declared keys/roles
/// are a follow-up.
///
/// The loaded `config` seeds the control plane's Sources/Outputs/Overlays (and
/// the working layout) resource stores at startup
/// ([`multiview_control::seed_resources`]), so the web UI resource pages are
/// non-empty under a live run instead of starting blank. Seeding is one-shot,
/// off the engine hot loop, into read-mostly control-plane stores that can never
/// back-pressure the engine (invariant #10).
///
/// Every configured HLS/LL-HLS output additionally mounts its delivery surface
/// at `/hls/{output-id}/` on this same listener ([`hls_mounts`] +
/// [`multiview_output::hls::http::hls_router`], DEV-D1): playlists/segments/
/// init served with the ADR-0032 §6 header contract — explicit Content-Type,
/// Cache-Control tiers, Range/206, and Origin-reflecting CORS, so Cast
/// receivers and browser players fetch cross-origin without a fronting proxy.
///
/// Also returns a clone of the served [`AppState`] so a sibling control-plane
/// tenant — the ADR-W020 config-file watcher — reaches the SAME stores,
/// command bus, audit log, and watch-status slot the router serves (one set
/// of stores, never a parallel copy).
///
/// `live_apply` declares what the **running** engine can take live (ADR-W022):
/// the caller — the binary, the only place that knows both the compiled
/// features and the chosen run path — injects it so every mutation route's
/// `X-Multiview-Apply` header tells the truth per build. Pass
/// [`multiview_control::LiveApplyCaps::default()`] for a run path with no live
/// seams (everything honestly declares `restart`).
///
/// # Errors
/// Returns an I/O error from binding the `listen` address, or — wrapped as
/// [`std::io::ErrorKind::InvalidData`] — a failure to seed the resource stores
/// from `config` (not expected for a validated config).
// reason: this is the control-plane bind seam; each parameter (listen, config,
// publisher, commands, preview, the Conspect LicenceState + MeshState, and the
// shutdown future) is a distinct, independently-owned input the bind threads into
// the `AppState`. Bundling them into a struct would only move the arity behind a
// one-use builder without improving clarity.
#[allow(clippy::too_many_arguments)]
// reason: `whep` (WHEP preview egress, ADR-P006) and `whip` (WHIP ingest,
// ADR-T014) are the canonical, externally-named WebRTC protocol terms — distinct
// transports, not a typo-similar pair; renaming either to satisfy the lint would
// obscure the spec mapping.
#[allow(clippy::similar_names)]
// reason: this is a flat, linear control-plane wiring sequence — one documented
// `if let Some(provider)` / `match` block per optional surface (WHEP preview, WHIP
// ingest, WHEP-serve output, discovery, device pollers, licence, mesh). It crossed
// the 100-line ceiling by 2 when the WebRTC ingest/preview/output providers were
// each wired (#141/#143/#147). Extracting the blocks would only scatter the wiring
// behind one-use helpers around the `#[cfg(feature = "discovery")]` rebind without
// improving clarity; the length is inherent to the surface count, not complexity.
#[allow(clippy::too_many_lines)]
pub async fn bind_and_serve<F>(
    listen: &str,
    config: &MultiviewConfig,
    publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    commands: CommandSender,
    preview: SharedPreview,
    switcher: Option<multiview_engine::switcher_control::SwitcherControl>,
    switcher_leases: Option<Arc<multiview_control::source_demand::SwitcherPreviewLeases>>,
    // The WHEP focus transport (ADR-P006), already cap-decorated. `None` keeps
    // the default `NoWhep` (a pure build sheds every focus to the JPEG ladder).
    whep: Option<multiview_control::SharedWhep>,
    // The WHIP ingest provider (ADR-T014). `None` keeps the default `NoWhip`
    // (every publish answers `503`; routes stay present + authz-enforced).
    whip: Option<multiview_control::SharedWhip>,
    whep_output: Option<multiview_control::SharedWhepOutput>,
    licence: Option<multiview_control::LicenceState>,
    mesh: Option<Arc<multiview_mesh::MeshState>>,
    live_apply: multiview_control::LiveApplyCaps,
    // The run-path live-apply capability (ADR-W018): which source kinds the
    // RUNNING engine can take live. The binary derives it from the seams it
    // actually wired (network kinds iff the hub has an ingest spawner), so
    // the X-Multiview-Apply header never over-claims.
    live_sources: multiview_control::LiveSourceCapability,
    // The Boot/Loaded/Running model (ADR-W024): `Some` for a config-file run
    // (backs `GET /config/boot-model` + the revert/promote actions); `None`
    // for store-only/`--ticks` runs (those routes report `modeled: false`).
    boot_model: Option<Arc<multiview_control::boot_model::BootModel>>,
    // Read-only source event snapshots from the running pipeline. `None` keeps
    // the default unavailable provider for run paths without an event runtime.
    source_events: Option<crate::source_event_runtime::RuntimeReader>,
    shutdown: F,
) -> std::io::Result<(SocketAddr, JoinHandle<std::io::Result<()>>, AppState)>
where
    F: Future<Output = ()> + Send + 'static,
{
    // Boxed so a caller's future does not embed this large state machine
    // (clippy::large_futures, 16 KiB threshold).
    Box::pin(bind_and_serve_with_moq(
        listen,
        config,
        publisher,
        commands,
        preview,
        switcher,
        switcher_leases,
        whep,
        whip,
        whep_output,
        licence,
        mesh,
        live_apply,
        live_sources,
        boot_model,
        source_events,
        #[cfg(feature = "moq-output")]
        None,
        #[cfg(feature = "moq-output")]
        None,
        None,
        shutdown,
    ))
    .await
}

/// Serve the control plane with an explicit run-scoped `MoQ` authority.
/// # Errors
/// Returns listener, seed-resource or control server initialization errors.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::similar_names
)]
pub async fn bind_and_serve_with_moq<F>(
    listen: &str,
    config: &MultiviewConfig,
    publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    commands: CommandSender,
    preview: SharedPreview,
    switcher: Option<multiview_engine::switcher_control::SwitcherControl>,
    switcher_leases: Option<Arc<multiview_control::source_demand::SwitcherPreviewLeases>>,
    // The WHEP focus transport (ADR-P006), already cap-decorated. `None` keeps
    // the default `NoWhep` (a pure build sheds every focus to the JPEG ladder).
    whep: Option<multiview_control::SharedWhep>,
    // The WHIP ingest provider (ADR-T014). `None` keeps the default `NoWhip`
    // (every publish answers `503`; routes stay present + authz-enforced).
    whip: Option<multiview_control::SharedWhip>,
    whep_output: Option<multiview_control::SharedWhepOutput>,
    licence: Option<multiview_control::LicenceState>,
    mesh: Option<Arc<multiview_mesh::MeshState>>,
    live_apply: multiview_control::LiveApplyCaps,
    // The run-path live-apply capability (ADR-W018): which source kinds the
    // RUNNING engine can take live. The binary derives it from the seams it
    // actually wired (network kinds iff the hub has an ingest spawner), so
    // the X-Multiview-Apply header never over-claims.
    live_sources: multiview_control::LiveSourceCapability,
    // The Boot/Loaded/Running model (ADR-W024): `Some` for a config-file run
    // (backs `GET /config/boot-model` + the revert/promote actions); `None`
    // for store-only/`--ticks` runs (those routes report `modeled: false`).
    boot_model: Option<Arc<multiview_control::boot_model::BootModel>>,
    // Read-only source event snapshots from the running pipeline. `None` keeps
    // the default unavailable provider for run paths without an event runtime.
    source_events: Option<crate::source_event_runtime::RuntimeReader>,
    #[cfg(feature = "moq-output")] moq: Option<Arc<multiview_control::moq::MoqRegistry>>,
    #[cfg(feature = "moq-output")] moq_slot: Option<
        crate::output_window_registry::WindowSubscriptionSlot,
    >,
    // The network front door (ADR-0107 slice 4): when its TCP door runs, the
    // SAME router and serve options are attached to it as the site, draining
    // with this server's shutdown. `None` leaves the plain listener only.
    front_door: Option<&crate::frontdoor::FrontDoorRun>,
    shutdown: F,
) -> std::io::Result<(SocketAddr, JoinHandle<std::io::Result<()>>, AppState)>
where
    F: Future<Output = ()> + Send + 'static,
{
    let listener = TcpListener::bind(listen).await?;
    let addr = listener.local_addr()?;

    // Mirror the loaded config into the control-plane resource stores before the
    // router carries them, so `GET /api/v1/{sources,outputs,overlays}` reflect
    // the running config. Off the hot loop; isolation-safe (invariant #10).
    let seeded = multiview_control::seed_resources(config)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;

    // Admin secret from the environment (12-factor; never from the repo/config),
    // else a generated bootstrap token surfaced once below.
    let admin_secret = std::env::var("MULTIVIEW_CONTROL_TOKEN")
        .ok()
        .filter(|s| !s.is_empty());
    let (mut api_keys, bootstrap_token) = provision_admin_keys(admin_secret);
    if let Some(token) = bootstrap_token {
        tracing::warn!(
            token = %token,
            "no MULTIVIEW_CONTROL_TOKEN set — generated a bootstrap admin token \
             (use as `Authorization: Bearer <token>`); set MULTIVIEW_CONTROL_TOKEN \
             to a stable secret for production"
        );
    } else {
        tracing::info!("control admin key provisioned from MULTIVIEW_CONTROL_TOKEN");
    }

    // Config-declared scoped API keys (ADR-W026): register every `[[api.keys]]`
    // beside the bootstrap admin so the object/output/discovery authorization
    // axes have live principals. Each secret is read from the env var named by
    // `secret_env` (never inlined — rule 34); a key whose secret env is unset is
    // a hard startup error (an un-authenticatable scoped key is a latent
    // misconfiguration). The bootstrap admin above is unchanged and unscoped.
    if let Some(api) = &config.api {
        multiview_control::auth::register_config_api_keys(&mut api_keys, &api.keys, |name| {
            std::env::var(name).ok()
        })
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if !api.keys.is_empty() {
            tracing::info!(
                count = api.keys.len(),
                "registered config-declared scoped API keys ([[api.keys]], ADR-W026)"
            );
        }
    }

    // Optional, explicit, opt-in auth-disable for trusted/local deployments.
    // `MULTIVIEW_CONTROL_AUTH=disabled|off|none|0` opens the whole API + WS as a
    // local admin (no token). Secure default: anything else (incl. unset) keeps
    // auth ON. A loud warning is logged whenever it is off.
    let auth_disabled = std::env::var("MULTIVIEW_CONTROL_AUTH").is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "disabled" | "off" | "none" | "false" | "0"
        )
    });
    if auth_disabled {
        tracing::warn!(
            "MULTIVIEW_CONTROL_AUTH disables authentication — the control plane API \
             and realtime stream are OPEN (local-admin, no token). Use ONLY on a \
             trusted/local network; never expose this listener publicly"
        );
    }

    // Mirror engine health warnings (SA-0 / ADR-0035) into a store the router
    // reads over `GET /api/v1/health`. The ingest subscribes to the engine's
    // drop-oldest event broadcast and only ever reads (lagged-skip on overflow),
    // so it can never back-pressure the engine (invariant #10). Subscribe BEFORE
    // the publisher is moved into the AppState.
    let warnings: Arc<dyn WarningRepository> = Arc::new(InMemoryWarningStore::new());
    let warning_sub = publisher.subscribe();
    tokio::spawn(run_warning_ingest(warning_sub, Arc::clone(&warnings)));

    // The Cast delivery map (DEV-D2, ADR-M011): `control.cast_media_base` ×
    // the served HLS mounts. `None` (no base configured / base rejected) means
    // the cast-session routes refuse with an honest 409 and the poller
    // registry carries no cast member.
    let delivery = cast_delivery(config);

    // The outbound-dial SSRF policy (SEC-02/SEC-04, CWE-918): the non-breaking
    // LAN default, tightened by the operator's `control.device_dial_allow`
    // allowlist (or the legacy env var).
    let dial_policy = device_dial_policy(config.control.as_ref());

    // Resolve the compositor adapter for the system-capabilities snapshot
    // (ADR-W030): only a `gpu` build can read a wgpu adapter; other builds report
    // the CPU-composite tier (compositor class `none`). A one-shot startup read.
    #[cfg(feature = "gpu")]
    let compositor_adapter = crate::capability_warn::read_compositor_adapter();
    #[cfg(not(feature = "gpu"))]
    let compositor_adapter: Option<multiview_hal::AdapterReport> = None;

    // Probe `yt-dlp --version` once at startup for the `youtube` capability
    // report (ADR-0015 §6 / ADR-W030): the SAME explicit path precedence
    // (`MULTIVIEW_YOUTUBE_YT_DLP_PATH` else `[system.youtube] yt_dlp_path`) the
    // per-source resolver uses. Logged at INFO on success so an operator can
    // confirm the discovered path/version without reading the capability API.
    // Absent/failed ⇒ `None` — the capability honestly reports unavailable
    // (mirrors the NDI runtime-probe posture), never a startup failure.
    #[cfg(feature = "youtube")]
    let youtube_probe = {
        let resolver_config = match crate::youtube_resolver_path(config) {
            Some(path) => multiview_input::youtube::ResolverConfig::new(
                path,
                multiview_input::youtube::ResolverConfig::default().timeout,
            ),
            None => multiview_input::youtube::ResolverConfig::default(),
        };
        match multiview_input::youtube::probe_version(&resolver_config).await {
            Ok(version) => {
                tracing::info!(
                    yt_dlp_path = %resolver_config.yt_dlp_path,
                    yt_dlp_version = %version,
                    "discovered yt-dlp for youtube live ingest (ADR-0015)"
                );
                Some(version)
            }
            Err(e) => {
                tracing::info!(
                    yt_dlp_path = %resolver_config.yt_dlp_path,
                    error = %multiview_ffmpeg::sanitize_line(&e.to_string()),
                    "yt-dlp not found/usable; youtube sources will report unavailable until it is installed"
                );
                None
            }
        }
    };
    #[cfg(not(feature = "youtube"))]
    let youtube_probe: Option<String> = None;

    let api_keys = Arc::new(api_keys);
    #[cfg(feature = "moq-output")]
    let outgoing = match moq_slot {
        Some(slot) => crate::moq_outputs::factory::DynamicRegistryFactory(slot)
            .activate_from_env(api_keys.clone())
            .map_err(|_| std::io::Error::other("MoQ relay activation rejected"))?,
        None => None,
    };
    let mut state = AppState::new(
        publisher,
        commands,
        Arc::new(InMemoryRepository::new()),
        api_keys,
    )
    .with_seeded_resources(seeded)
    .with_base_document(
        serde_json::to_value(config)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?,
    )
    .with_preview(preview)
    .with_warning_store(warnings);
    #[cfg(unix)]
    {
        state = state.with_web_capture(multiview_control::web_capture_startup::from_environment()?);
    }
    #[cfg(feature = "moq-output")]
    if let Some(moq) = moq {
        state = state.with_moq(moq);
    }
    if let Some(switcher) = switcher {
        state = state.with_switcher(switcher);
        // Stored layouts as M/E sources (#307): installed by the run only
        // when a switcher is attached.
        #[cfg(feature = "ffmpeg")]
        if let Some(sources) = crate::me_layouts::installed_me_layouts() {
            state = state.with_me_layouts(sources);
        }
    }
    // Output feed routing (ADR-0108 slice 1): the run installs its routing
    // slot before the control plane starts; without one the routes answer
    // 409 `routing-unavailable`.
    #[cfg(feature = "ffmpeg")]
    if let Some(slot) = crate::output_routing_control::installed_program_metrics() {
        state = state.with_program_metrics(std::sync::Arc::new(
            crate::output_routing_control::ProgramMetricsAdapter(slot),
        ));
    }
    // Front-door ACME (ADR-0107 section 5a): the run started the ACME service
    // before binding; its context mounts `/api/v1/tls/*` on this listener.
    #[cfg(all(feature = "acme", unix))]
    if let Some(tls) = crate::tls_boot::installed_tls_context() {
        state = state.with_tls(tls);
    }
    #[cfg(feature = "ffmpeg")]
    if let Some(slot) = crate::output_routing_control::installed_output_routing() {
        state = state.with_output_routing(std::sync::Arc::new(
            crate::output_routing_control::OutputRoutingAdapter::new(slot),
        ));
    }
    // The custom Cast receiver app id (ADR-0111): `MULTIVIEW_CAST_APP_ID`
    // wins over `[control] cast_app_id`; an invalid value is a warning and
    // no custom receiver (the sender then uses the Default Media Receiver).
    state = state.with_cast_app_id(cast_app_id(config));
    // The running engine's cast outputs (ADR-0111): activation gates behind
    // a lock-free slot, so the control plane never waits on the engine.
    #[cfg(feature = "ffmpeg")]
    if let Some(slot) = crate::cast_output::installed_cast_outputs() {
        state = state.with_cast_engine(std::sync::Arc::new(crate::cast_output::CastEngineAdapter(
            slot,
        )));
    }
    // The network front door's live counters (ADR-0107 slice 3,
    // `GET /api/v1/metrics/frontdoor`); absent when `[network]` is not
    // configured this run (404, not an error).
    if let Some(source) = crate::frontdoor::installed_metrics_source() {
        state = state.with_frontdoor_metrics(source);
    }
    if let Some(leases) = switcher_leases {
        state.switcher_preview_lease = leases;
    }
    let mut break_provider: Option<multiview_control::source_events::SharedSourceBreakProvider> =
        None;
    if let Some(demand) = crate::source_demand::installed_control_demand() {
        state = state.with_source_demand(demand);
    }
    if let Some(reader) = source_events {
        let reader = Arc::new(reader);
        let provider: Arc<dyn multiview_control::source_events::SourceEventProvider> =
            reader.clone();
        break_provider = Some(reader.clone());
        let injector: Arc<dyn multiview_control::source_events::SourceEventTestInjector> = reader;
        state = state
            .with_source_events(provider)
            .with_source_event_test_injector(injector);
    }
    if let Some(reader) = crate::event_journal::reader() {
        state = state.with_event_journal(reader);
    }
    // Stored layouts persist across restarts (#176): seeding installs a fresh
    // in-memory store holding the config's working layout; move that layout into
    // the persistent store (if any) and serve from it.
    state.repository = persist_layouts(&state.repository, boot_model.as_deref()).await;
    if let Some(model) = boot_model.as_ref() {
        // `accepted()` loads the envelope once and keeps it as the in-memory
        // snapshot every later read serves (review R3-03).
        if let Some(desired) = model.accepted().map_err(std::io::Error::other)? {
            // Names and ETags continue from accepted intent; reseeding would
            // restart every counter at 1 and let a stale tag match again.
            state = multiview_control::boot_model::restore_accepted_stores(state, &desired)
                .map_err(|error| std::io::Error::other(error.to_string()))?;
        }
        model
            .restore_desired_layout_id()
            .map_err(std::io::Error::other)?;
    }
    if let Some(whep) = whep {
        state = state.with_whep(whep);
    }
    let mut state = state
    .with_device_pollers(device_poller_registry(delivery.as_ref(), &dial_policy))
    .with_dial_policy(Arc::clone(&dial_policy))
    .with_auth_disabled(auth_disabled)
    // The management-plane request-concurrency + rate caps (SEC-14 control-plane DoS
    // floor): map `control.limits` (secure defaults when the section is absent)
    // onto the served API. Config-driven; the guards shed `429`/`503` and never
    // touch the engine (invariant #10).
    .with_limits(
        &config
            .control
            .as_ref()
            .map(|control| control.limits.clone())
            .unwrap_or_default(),
    )
    // The realtime `Origin` allow-list (SEC-13 / CSWSH, ADR-RT011): map
    // `control.allowed_origins` (empty ⇒ same-origin only) onto the WS + SSE upgrade
    // gate. Enforced even when auth is disabled — a cross-origin handshake bypasses
    // SOP/CORS, so this is the CSWSH defense for the realtime firehose.
    .with_allowed_origins(
        config
            .control
            .as_ref()
            .map(|control| control.allowed_origins.clone())
            .unwrap_or_default(),
    )
    // The CORS allow-list for the WebRTC media-signalling routes (ADR-0048 §9):
    // map `[webrtc].cors_allow_origins` (default `["*"]`) onto the control plane
    // so a browser served from a web origin can WHIP-publish / WHEP-play.
    .with_cors_allow_origins(config.webrtc.cors_allow_origins.clone())
    // Viewer playback URL derivation (ADR-0101): map `control.public_host`
    // (absent ⇒ the route falls back to the request Host header) through.
    .with_public_host(
        config
            .control
            .as_ref()
            .and_then(|control| control.public_host.clone()),
    )
    // The `[control] public_scheme` override (ADR-0101 scheme derivation):
    // only needed when a front end does NOT set `X-Forwarded-Proto` (the
    // production nginx config does).
    .with_public_scheme(
        config
            .control
            .as_ref()
            .and_then(|control| control.public_scheme.clone()),
    )
    .with_live_apply(live_apply)
    // The run-path live-apply capability (ADR-W018): which source kinds the
    // running engine can take live — synthetic always, network/file only when
    // a real ingest spawner backs them. The header consults this before
    // promising `live` (never over-claims).
    .with_live_sources(live_sources)
    // The static system capability + licence snapshot (ADR-W030): the HAL probe
    // + compiled features + resolved compositor adapter that
    // `GET /api/v1/system/capabilities` serves. A one-shot snapshot, never an
    // engine channel (invariant #10).
    .with_capabilities(crate::system_capabilities::resolve_system_capabilities(
        compositor_adapter.as_ref(),
        youtube_probe,
        crate::frontdoor::installed_capability(),
    ))
    // The `[discovery]` browse configuration: the operator-configured
    // zowietek-control service type (the vendor's type is unverified — only a
    // configured string is ever recognised) plus any extra DNS-SD types.
    .with_discovery_config(config.discovery.clone().unwrap_or_default())
    // BOM weather: the management-plane poller (a tokio task, never an engine
    // thread) publishing into the process-global snapshot store the weather
    // source workers read. Idle until a location is configured.
    .with_weather(crate::weather_source::spawn_weather_service(config))
    // Graphics (CG): the process-global hub the graphics generators read,
    // seeded with the effective `[graphics]` (legacy `[weather]` locations
    // folded in as bom_weather data sources), plus the off-clock thumbnailer.
    .with_graphics(crate::graphics_source::seed_global_hub(config))
    .with_graphics_thumbnails(std::sync::Arc::new(crate::graphics_source::CliThumbnailer));
    // The Boot/Loaded/Running model (ADR-W024): installed for a config-file
    // run so the boot-model/revert/promote routes are live; absent ⇒ those
    // routes honestly report `modeled: false`.
    #[cfg(feature = "catalogue")]
    let catalogue_boot_path = boot_model.as_ref().map(|m| m.boot_path().to_path_buf());
    if let Some(boot_model) = boot_model {
        state = state.with_boot_model(boot_model);
    }
    // Serve the process log-tail ring the capture layer feeds (ADR-0060): the
    // same `Arc` `init_tracing` installed, so `GET /api/v1/logs` returns live
    // captured, resource-attributed records. If logging was not initialized
    // (never, in the binary), the AppState keeps its empty default ring.
    if let Some(logs) = crate::log_ring() {
        state = state.with_log_ring(logs);
    }
    if let Some(delivery) = delivery {
        state = state.with_cast_delivery(delivery);
    }
    // The WHIP ingest provider (ADR-T014): when the binary wired the native
    // endpoint, install it so a publish negotiates a real session; otherwise the
    // default `NoWhip` answers `503` (routes stay present + authz-enforced).
    if let Some(whip) = whip {
        state = state.with_whip(whip);
    }
    // The WHEP-serve output provider (ADR-0049): when the binary wired the native
    // endpoint, install it so a `POST /api/v1/whep/{output}` serves the real
    // encoded program to a browser viewer; otherwise the default `NoWhepOutput`
    // answers `503` (routes stay present + authz-enforced).
    if let Some(whep_output) = whep_output {
        state = state.with_whep_output(whep_output);
    }

    // IPTV-ORG source browser (ADR-M015): catalogue sync, link tracker and the
    // bounded deep probe, all on the control plane (invariants #1/#10). Wired only
    // when the `catalogue` feature is built; `[catalogues.iptv_org] enabled = false`
    // keeps the routes (answering "disabled") but spawns no sync task.
    #[cfg(feature = "catalogue")]
    let state = multiview_control::catalogue::install(
        state,
        &config
            .catalogues
            .as_ref()
            .and_then(|c| c.iptv_org.clone())
            .unwrap_or_default(),
        catalogue_boot_path.as_deref(),
        std::env::var("HOSTNAME").unwrap_or_else(|_| "this-server".to_owned()),
        config
            .network_diagnostics
            .as_ref()
            .is_none_or(|n| n.egress_lookup),
    );

    // Install the real mDNS browser when the `discovery` feature is built, so
    // `POST /api/v1/discovery/devices/scan` browses the LAN for Cast / NDI /
    // (configured) zowietek-control services. Discovery is untrusted inventory
    // requiring explicit confirm-adopt (ADR-0041) and the browse runs on a
    // bounded control-plane task — it can never back-pressure the engine
    // (invariant #10). Without the feature the default `NullBrowser` finds
    // nothing, so the endpoints answer with an empty inventory rather than
    // failing.
    #[cfg(feature = "discovery")]
    let state = match multiview_control::devices::discovery::MdnsBrowser::new() {
        Ok(browser) => state.with_discovery_browser(Arc::new(browser)),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "mDNS discovery daemon failed to start; device discovery is disabled \
                 for this run (scans return an empty inventory)"
            );
            state
        }
    };

    // Boot-seed: spawn a supervised driver poller for every config-declared
    // managed device (DEV-A4, ADR-M009). With the `zowietek` feature on, a
    // `zowietek` device logs in → probes → enumerates its three facets → polls
    // status → drives the lifecycle, so the projection routes return real data
    // and `set-mode` dispatches convergence — all on its own control-plane task
    // (invariant #10). Without the feature this is a no-op (the default no-op
    // factory spawns nothing). Off the engine hot loop.
    let spawned = state.seed_device_pollers(&config.devices);
    if spawned > 0 {
        tracing::info!(spawned, "DEV-A4: spawned supervised device poller(s)");
    }

    // The Conspect entitlement plane (ADR-0050): share the SAME lease store the
    // cli built (and the engine seams sample), so the API/`GET /api/v1/licence`,
    // the config-lock interceptor, the start gate, and the heartbeat-status surface
    // all render the same ladder state the engine reads — there is no second
    // opinion. `None` keeps the empty unlicensed default (a store-only run / test).
    let state = match licence {
        Some(licence) => state.with_licence(licence),
        None => state,
    };

    // The Conspect local-mesh plane (ADR-0051): share the SAME `MeshState` the cli
    // owns + (under `mesh-mdns`) the always-on announce/browse loop maintains, so
    // `/api/v1/mesh/*` renders + toggles the live discovery state. `None` keeps the
    // empty always-on default (a store-only run / test). Control-plane only; the
    // store holds no engine handle and can never back-pressure the engine (inv #10).
    let mut state = match mesh {
        Some(mesh) => state.with_mesh(mesh),
        None => state,
    };

    // Mount each configured HLS/LL-HLS output's delivery surface under
    // `/hls/{output-id}/` (DEV-D1): the ADR-0032 §6 router serving that
    // output's playlist/segment/init files with the Cache-Control tiers,
    // Range/206, and Origin-reflecting CORS — so a Cast receiver (a browser
    // app on a Google origin) or any browser player fetches cross-origin
    // straight off this listener. Deliberately OUTSIDE `/api/v1`, so it is
    // unauthenticated like `/docs` (media devices cannot send Bearer tokens).
    // Isolation-safe (inv #10): the handlers only read files the segmenter
    // already published to disk — never an engine channel or lock.
    // ADR-W024 MAJOR-C1 (startup TOCTOU): for a config-file run (a boot model
    // is wired) the config-file watcher's suppression handle MUST be installed
    // into the shared state BEFORE the router serves a single request — else a
    // `POST /config/promote` in the window between serving and the watcher
    // spawning would find no handle, skip suppression, write the boot file, and
    // the watcher would later re-apply the promoted file as an external edit.
    // The handle is created and installed here; the caller reads it back via
    // `state.watch_handle()` and drives the SAME handle from the spawned watcher
    // (`WatchOptions::with_handle`). Store-only runs (no boot model) install no
    // handle — promote there correctly skips suppression (nothing to suppress).
    if state.boot_model.is_some() {
        state.install_watch_handle(multiview_control::config_watch::ConfigWatchHandle::new());
        // Boot is a repair point: a fault that outlived the last run heals here.
        multiview_control::layout_projection::request(&state);
    }
    // ADR-0183 ad-break automation executor: a management-plane tokio task that
    // polls the source-event service and issues ordinary bounded bus commands.
    // It never runs on (or blocks) the output clock; every rule is disarmed
    // until an operator arms it.
    if let Some(provider) = break_provider {
        state = start_event_automation(state, provider);
    }
    // IPTV guide refresher (epg-playlist): one control-plane task that only
    // fetches while `[iptv] enabled`; never touches the engine (inv #1/#10).
    drop(multiview_control::iptv::spawn_refresher(state.clone()));
    #[cfg(unix)]
    let cloudflare = configured_cloudflare()?;
    #[cfg(unix)]
    let reconciler = cloudflare
        .as_ref()
        .map(|provider| AbortTask(provider.clone().spawn_reconciler()));
    #[cfg(unix)]
    let state = if let Some(provider) = cloudflare {
        state.with_cloudflare(provider)
    } else {
        state
    };
    #[cfg(not(unix))]
    let reconciler: Option<AbortTask> = configured_cloudflare()?.map(|never| match never {});
    // Keep the abort guard inside the server's shutdown future. It also runs if
    // the server future is dropped, so no management task survives the server.
    let shutdown = async move {
        let _reconciler = reconciler;
        #[cfg(feature = "moq-output")]
        let outgoing = outgoing;
        shutdown.await;
        #[cfg(feature = "moq-output")]
        if let Some(outgoing) = outgoing {
            outgoing.shutdown().await;
        }
    };
    install_ui_dir(config)?;
    let app = mount_hls(multiview_control::router(state.clone()), config);
    // TLS-0 (ADR-W029): serve over rustls when `[control.tls]` is configured, else
    // plain HTTP. The TLS material is loaded HERE (startup), so a missing/garbage
    // certificate aborts `multiview run` loudly rather than failing inside the
    // spawned server task; a `[control.tls]` config in a build WITHOUT the `tls`
    // feature is a hard startup error — never a silent plain-HTTP downgrade.
    // SEC-14 #126 (ADR-W028 F-A): the header-read timeout (slowloris guard) is a
    // serve-loop property, not a router layer, so it is threaded into the serve
    // helper. It applies to every served connection regardless of the rate/concurrency
    // `enabled` flag. Absent `[control.limits]` ⇒ the secure default (20 s).
    let serve_options = match config.control.as_ref() {
        Some(control) => {
            let limits = &control.limits;
            // The header-read timeout applies to every connection regardless of the
            // shed-layer `enabled` flag (a generous timeout has no downside for a
            // legitimate client). Reuse the same budget for the TLS handshake timeout —
            // both bound "time to get your request in", pre- and post-handshake — so a
            // client stalled mid-handshake recycles its accept-level cap slot too.
            let header_read = std::time::Duration::from_secs(limits.header_read_timeout_secs);
            // The accept-level population caps ARE shed-layer controls (like the
            // request-concurrency + rate caps): enforced only while `enabled`.
            let (max_connections, max_connections_per_ip) = if limits.enabled {
                (
                    Some(limits.max_connections),
                    Some(limits.max_connections_per_ip),
                )
            } else {
                (None, None)
            };
            multiview_control::ServeOptions::default()
                .with_header_read_timeout(Some(header_read))
                .with_tls_handshake_timeout(Some(header_read))
                .with_max_connections(max_connections)
                .with_max_connections_per_ip(max_connections_per_ip)
        }
        None => multiview_control::ServeOptions::default(),
    };
    // The front door's TCP door serves the same app (slice 4). Its connections
    // observe this server's shutdown through a watch flipped when `shutdown`
    // resolves (or dropped with the server task). With `[network.tls]` (slice
    // 5) the door serves the gated front-door router instead: in `setup` only
    // the certificate setup surface answers. This listener stays ungated.
    let site_app = door_site_app(&state, config, &app);
    let site_options = serve_options.clone();
    // One static certificate source for both listeners, reloaded when certbot
    // renews the files in place (slice 6, #292): no restart for a renewal.
    #[cfg(all(feature = "tls", unix))]
    let static_source = static_tls_source(config.control.as_ref().and_then(|c| c.tls.clone()))?;
    // The door's own static pair (`[network] cert_file/key_file`) wins over
    // `[control.tls]` for the door; the loopback listener keeps `[control.tls]`
    // (or plain HTTP).
    #[cfg(all(feature = "tls", unix))]
    let door_source = match static_tls_source(
        config
            .network
            .as_ref()
            .and_then(multiview_config::NetworkConfig::static_site_tls),
    )? {
        Some(pair) => Some(pair),
        None => static_source.clone(),
    };
    #[cfg(all(feature = "tls", unix))]
    let shutdown = {
        let mut reloads = Vec::new();
        for (tls, source) in static_source.iter().chain(door_source.iter()) {
            if reloads.iter().any(|(t, _)| t == tls) {
                continue;
            }
            reloads.push((
                tls.clone(),
                AbortTask(multiview_control::spawn_static_certificate_reload(
                    tls.clone(),
                    Arc::clone(source),
                    multiview_control::STATIC_CERTIFICATE_POLL,
                )),
            ));
        }
        async move {
            let _reloads = reloads;
            shutdown.await;
        }
    };
    let shutdown = attach_front_door_site(
        front_door,
        config,
        &state,
        #[cfg(all(feature = "tls", unix))]
        door_source.as_ref().map(|(_, source)| Arc::clone(source)),
        site_app,
        move |app, watch| multiview_control::ConnectionServer::new(app, &site_options, watch),
        shutdown,
    )?;
    let handle = match config
        .control
        .as_ref()
        .and_then(|control| control.tls.as_ref())
    {
        None => tokio::spawn(multiview_control::serve_router_with(
            listener,
            app,
            serve_options,
            shutdown,
        )),
        #[cfg(feature = "tls")]
        Some(tls) => {
            #[cfg(unix)]
            let material = match static_source.as_ref() {
                Some((_, source)) => {
                    let source: Arc<dyn multiview_control::certificate_source::CertificateSource> =
                        source.clone();
                    multiview_control::rustls_material_from_source(source)
                }
                None => multiview_control::load_tls_material(tls),
            };
            #[cfg(not(unix))]
            let material = multiview_control::load_tls_material(tls);
            let material = material
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
            tokio::spawn(multiview_control::serve_router_tls_with(
                listener,
                app,
                material,
                serve_options,
                shutdown,
            ))
        }
        #[cfg(not(feature = "tls"))]
        Some(_tls) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "control.tls is configured but this `multiview` build lacks the `tls` feature; \
                 rebuild with --features tls (or a preset), or remove [control.tls]",
            ));
        }
    };
    Ok((addr, handle, state))
}

/// Nest the `/hls/{output}` directory mounts of `config` into `router`.
fn mount_hls(
    mut router: multiview_control::ControlRouter,
    config: &MultiviewConfig,
) -> multiview_control::ControlRouter {
    for mount in hls_mounts(config) {
        router = router.nest(
            &mount.route,
            multiview_output::hls::http::hls_router(mount.dir),
        );
    }
    router
}

/// The router the front door's TCP door serves. With a TLS context
/// (`[network.tls]`, slice 5) it is
/// [`multiview_control::tls_setup::front_door_router_with`]: the control
/// router plus the HLS mounts behind the setup gate. Otherwise it is the
/// plain listener's `app` (slice 4, unchanged).
fn door_site_app(
    state: &multiview_control::AppState,
    config: &MultiviewConfig,
    app: &multiview_control::ControlRouter,
) -> multiview_control::ControlRouter {
    #[cfg(all(feature = "acme", unix))]
    if state.tls.is_some() {
        return multiview_control::tls_setup::front_door_router_with(state.clone(), |router| {
            mount_hls(router, config)
        });
    }
    let _ = (state, config);
    app.clone()
}

/// The door's TLS configurations: the site, and the TLS-ALPN-01 responder.
type DoorTls = (
    Option<multiview_frontdoor::SiteTlsConfig>,
    Option<multiview_frontdoor::SiteTlsConfig>,
);

/// The door's TLS configurations from the ACME context (`[network.tls]`,
/// slice 5): the site over the ACME certificate source (bootstrap in setup,
/// the issued certificate once serving, renewals on the next handshake) and
/// the `acme-tls/1` responder over the challenge resolver. `None` without a
/// context. `[network.tls]` wins over `[control.tls]` for the door.
#[cfg(all(feature = "acme", unix))]
fn acme_door_tls(
    state: &multiview_control::AppState,
    config: &MultiviewConfig,
) -> std::io::Result<Option<DoorTls>> {
    let Some(tls) = state.tls.as_ref() else {
        return Ok(None);
    };
    let invalid = |e: multiview_control::TlsSetupError| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
    };
    if config.control.as_ref().is_some_and(|c| c.tls.is_some()) {
        tracing::warn!(
            "both [network.tls] and [control.tls] are configured: the front door serves the \
             ACME certificate; [control.tls] applies to the [control] listen listener only"
        );
    }
    let site =
        multiview_control::site_server_config(Arc::clone(&tls.certificates)).map_err(invalid)?;
    let acme = multiview_control::acme_challenge_server_config(Arc::clone(&tls.resolver))
        .map_err(invalid)?;
    Ok(Some((Some(site), Some(acme))))
}

/// Attach the control-plane `app` to the front door's TCP door as its site
/// (ADR-0107 slice 4, issue #290) and return the server's shutdown future,
/// extended to signal the door's connections. With `[network.tls]` (slice 5)
/// the site certificate is the ACME source and TLS-ALPN-01 is answered on the
/// door ([`acme_door_tls`]). Otherwise it is the `[control.tls] mode =
/// "static"` pair through the `CertificateSource` seam (`tls` builds on unix),
/// and without either the door runs ICE-TCP only, with a warning.
///
/// # Errors
/// A `[control.tls]` certificate that cannot be loaded (the same startup
/// failure the plain TLS listener reports).
// reason: without `tls` (or off unix) the certificate-load error path is
// compiled out, so the `Result` is only unnecessary in that build.
#[cfg_attr(not(all(feature = "tls", unix)), allow(clippy::unnecessary_wraps))]
fn attach_front_door_site<F>(
    front_door: Option<&crate::frontdoor::FrontDoorRun>,
    config: &MultiviewConfig,
    state: &multiview_control::AppState,
    #[cfg(all(feature = "tls", unix))] static_source: Option<
        Arc<multiview_control::certificate_source::StaticCertificateSource>,
    >,
    site_app: multiview_control::ControlRouter,
    server: impl FnOnce(
        multiview_control::ControlRouter,
        tokio::sync::watch::Receiver<bool>,
    ) -> multiview_control::ConnectionServer,
    shutdown: F,
) -> std::io::Result<std::pin::Pin<Box<dyn Future<Output = ()> + Send + 'static>>>
where
    F: Future<Output = ()> + Send + 'static,
{
    let Some(door) = front_door else {
        return Ok(Box::pin(shutdown));
    };
    #[cfg(all(feature = "acme", unix))]
    let acme = acme_door_tls(state, config)?;
    #[cfg(not(all(feature = "acme", unix)))]
    let acme: Option<DoorTls> = {
        let _ = state;
        None
    };
    let deferred = acme.is_some();
    let (site, challenge) = match acme {
        Some(door_tls) => door_tls,
        None => (
            static_door_site(
                config,
                #[cfg(all(feature = "tls", unix))]
                static_source,
            )?,
            None,
        ),
    };
    let (signal, watch) = tokio::sync::watch::channel(false);
    // Slice 2: the SAME router (gated by setup in ACME mode) over HTTP/3 on
    // the QUIC lane, sharing this shutdown watch. With `[network.tls]` the
    // endpoint waits for `serving` (no QUIC in setup).
    door.start_h3(
        Arc::new(multiview_control::h3_site::H3Site::new(
            site_app.clone(),
            watch.clone(),
        )),
        h3_tls(state, site.clone(), deferred),
    );
    if door.has_tcp_door() {
        // Alt-Svc on the TCP door only (never the plain `[control]` listener);
        // the switch is off until the h3 endpoint is up.
        let app = multiview_control::h3_site::alt_svc_layer(site_app, door.alt_svc());
        let responder = door.install_site(server(app, watch), site, challenge);
        // The ACME engine started before the control plane was built and waits
        // for this before a TLS-ALPN-01 order: until now `acme-tls/1` hellos
        // were closed, so a validation would have failed at once.
        #[cfg(all(feature = "acme", unix))]
        if responder {
            if let Some(tls) = state.tls.as_ref() {
                tls.resolver.mark_responder_ready();
            }
        }
        #[cfg(not(all(feature = "acme", unix)))]
        let _ = responder;
    }
    Ok(Box::pin(async move {
        shutdown.await;
        let _ = signal.send(true);
    }))
}

/// The h3 endpoint's certificate: the door's site configuration (ALPN and
/// 0-RTT are overridden by `multiview-h3`); with `[network.tls]` held until
/// the ACME source is `serving`.
fn h3_tls(
    state: &multiview_control::AppState,
    site: Option<multiview_frontdoor::SiteTlsConfig>,
    deferred: bool,
) -> crate::frontdoor::H3Tls {
    #[cfg(all(feature = "acme", unix))]
    if deferred {
        if let Some(tls) = state.tls.as_ref() {
            // No shutdown arm needed: `FrontDoorRun::shutdown` aborts the
            // starter, and `wait_serving` returns `false` once the source is
            // dropped.
            let certificates = Arc::clone(&tls.certificates);
            return crate::frontdoor::H3Tls {
                config: site,
                ready: Some(Box::pin(crate::tls_boot::wait_serving(
                    certificates,
                    std::future::pending::<()>(),
                ))),
                source: "[network.tls] (ACME)",
            };
        }
    }
    let _ = (state, deferred);
    crate::frontdoor::H3Tls {
        config: site,
        ready: None,
        source: "the door's static certificate",
    }
}

/// The door site over a static certificate: `[network] cert_file/key_file`,
/// else `[control.tls]` (slice 4/6), reloaded on change by the caller's task.
// reason: as `attach_front_door_site`.
#[cfg_attr(not(all(feature = "tls", unix)), allow(clippy::unnecessary_wraps))]
fn static_door_site(
    config: &MultiviewConfig,
    #[cfg(all(feature = "tls", unix))] static_source: Option<
        Arc<multiview_control::certificate_source::StaticCertificateSource>,
    >,
) -> std::io::Result<Option<multiview_frontdoor::SiteTlsConfig>> {
    #[cfg(not(all(feature = "tls", unix)))]
    if config
        .network
        .as_ref()
        .is_some_and(|n| n.static_site_tls().is_some())
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "network.cert_file/key_file is set but this `multiview` build lacks the `tls` \
             feature (or the platform is not unix); rebuild with --features tls",
        ));
    }
    #[cfg(all(feature = "tls", unix))]
    let _ = config;
    #[cfg(all(feature = "tls", unix))]
    let site = match static_source {
        Some(source) => Some(
            multiview_control::site_server_config(source)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?,
        ),
        None => None,
    };
    #[cfg(not(all(feature = "tls", unix)))]
    let site: Option<multiview_frontdoor::SiteTlsConfig> = {
        let _ = config;
        None
    };
    Ok(site)
}

/// Load `[control] ui_dir` (slice 6, #292) so the SPA fallback serves that
/// UI build instead of the embedded one. Read once, into memory; a build
/// without the embedded UI (`web` feature off) refuses the key loudly.
///
/// # Errors
/// An unreadable or invalid UI directory (startup fails, never a blank UI).
fn install_ui_dir(config: &MultiviewConfig) -> std::io::Result<()> {
    let Some(dir) = config.control.as_ref().and_then(|c| c.ui_dir.as_ref()) else {
        return Ok(());
    };
    #[cfg(feature = "web")]
    {
        let ui = multiview_control::spa::UiDir::load(dir)?;
        let files = ui.len();
        if multiview_control::spa::install_ui_dir(ui) {
            tracing::info!(dir = %dir.display(), files, "serving the web UI from control.ui_dir");
        }
        Ok(())
    }
    #[cfg(not(feature = "web"))]
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "control.ui_dir ({}) is set but this `multiview` build has no web UI (`web` feature)",
                dir.display()
            ),
        ))
    }
}

/// A `mode = "static"` pair (`[control.tls]`, or the door's `[network]
/// cert_file/key_file`) loaded once into a reloadable source (slice 6).
///
/// # Errors
/// A certificate that cannot be loaded (startup fails loudly, as before).
#[cfg(all(feature = "tls", unix))]
#[allow(clippy::type_complexity)]
fn static_tls_source(
    tls: Option<multiview_config::TlsConfig>,
) -> std::io::Result<
    Option<(
        multiview_config::TlsConfig,
        Arc<multiview_control::certificate_source::StaticCertificateSource>,
    )>,
> {
    let Some(tls) = tls else {
        return Ok(None);
    };
    let source = multiview_control::static_certificate_source(&tls)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    Ok(Some((tls, Arc::new(source))))
}

/// Abort the off-clock management task when the server exits or is cancelled.
struct AbortTask(tokio::task::JoinHandle<()>);
impl Drop for AbortTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Managed Cloudflare needs the POSIX owner-only credential vault; on Windows
/// configuring it is refused explicitly instead of silently ignored.
#[cfg(not(unix))]
fn configured_cloudflare() -> std::io::Result<Option<std::convert::Infallible>> {
    let configured = std::env::var_os("MULTIVIEW_CLOUDFLARE_STATE_DIR").is_some()
        || std::env::var_os("MULTIVIEW_CLOUDFLARE_MASTER_KEY_FILE").is_some();
    if configured {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "managed Cloudflare is not supported on this platform",
        ));
    }
    Ok(None)
}

/// Opt-in startup configuration references a private key file, never a raw key
/// argument or environment value. Errors deliberately omit secret paths/data.
#[cfg(unix)]
fn configured_cloudflare(
) -> std::io::Result<Option<Arc<multiview_control::cloudflare_management::ManagedCloudflare>>> {
    use std::io::{Error, ErrorKind, Read};
    let dir = std::env::var_os("MULTIVIEW_CLOUDFLARE_STATE_DIR");
    let key_path = std::env::var_os("MULTIVIEW_CLOUDFLARE_MASTER_KEY_FILE");
    let (dir, key_path) = match (dir, key_path) {
        (None, None) => return Ok(None),
        (Some(dir), Some(key)) => (dir, key),
        _ => {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Cloudflare requires state directory and private master-key file",
            ))
        }
    };
    let path = std::path::Path::new(&key_path);
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            "Cloudflare master-key file must be regular",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "Cloudflare master-key file must be private",
            ));
        }
    }
    let mut key = [0u8; 32];
    let mut file = std::fs::File::open(path)?;
    let opened = file.metadata()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.dev() != metadata.dev()
            || opened.ino() != metadata.ino()
            || opened.mode() & 0o077 != 0
            || !opened.is_file()
        {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "Cloudflare master-key file changed during open",
            ));
        }
    }
    if let Err(error) = file.read_exact(&mut key) {
        key.fill(0);
        return Err(error);
    }
    let mut trailing = [0u8; 1];
    if file.read(&mut trailing)? != 0 {
        key.fill(0);
        return Err(Error::new(
            ErrorKind::InvalidData,
            "Cloudflare master key must contain exactly 32 bytes",
        ));
    }
    let provider = multiview_control::cloudflare_management::ManagedCloudflare::from_master_key(
        std::path::Path::new(&dir),
        key,
    );
    key.fill(0);
    provider
        .map(|provider| Some(Arc::new(provider)))
        .map_err(|_| {
            Error::new(
                ErrorKind::InvalidData,
                "Cloudflare private state unavailable",
            )
        })
}

/// Build the runtime device-poller registry for the control plane (DEV-A4/D2).
///
/// With the `devices-net` feature on (which forwards
/// `multiview-control/devices-net` = `zowietek` + `cast`), the registry carries
/// a [`CompositePollerFactory`](multiview_control::devices::CompositePollerFactory)
/// over both live drivers:
///
/// * the reqwest-backed
///   [`ReqwestPollerFactory`](multiview_control::devices::ReqwestPollerFactory),
///   so boot-seed/adopt spawn a **live** supervised poller per `zowietek`
///   device, resolving credentials from its `auth.secret_ref` via
///   [`resolve_device_credentials`];
/// * the [`CastSessionFactory`](multiview_control::devices::cast::runtime::CastSessionFactory)
///   over the live [`TlsCastConnector`](multiview_control::devices::cast::net::TlsCastConnector)
///   and the [`cast_delivery`] map, so a `driver = cast` device (config-declared
///   or an ad-hoc `/api/v1/cast/sessions` start) gets a supervised CASTV2
///   session actor (DEV-D2, ADR-M011). Installed only when a delivery map
///   exists (`control.cast_media_base` set) — without one no device-reachable
///   media URL can be derived, so the cast member is honestly absent and the
///   session routes refuse with `409`. A rustls-config build failure (a broken
///   crypto provider — never expected) is logged and likewise leaves the cast
///   member out rather than panicking.
///
/// Without the feature it is the default no-op registry (no live transport →
/// no actor spawned; the projection routes stay honestly empty), so the
/// default build pulls no socket.
#[cfg(feature = "devices-net")]
fn device_poller_registry(
    cast_delivery: Option<&std::sync::Arc<multiview_control::devices::cast::media::CastDelivery>>,
    dial_policy: &std::sync::Arc<multiview_config::device::net_guard::DialPolicy>,
) -> Arc<multiview_control::devices::DevicePollerRegistry> {
    use multiview_control::devices::cast::net::TlsCastConnector;
    use multiview_control::devices::cast::runtime::CastSessionFactory;
    use multiview_control::devices::cast::session::CastSessionConfig;
    use multiview_control::devices::{
        CompositePollerFactory, DevicePollerFactory, DevicePollerRegistry, ReqwestPollerFactory,
    };
    // A 5s per-request timeout: generous for a LAN appliance, bounded so a hung
    // device times out into the supervised-reconnect path rather than wedging
    // the poller task. Both live transports screen every resolved dial IP
    // against the SSRF `dial_policy` (SEC-02/SEC-04).
    let zowietek = ReqwestPollerFactory::new(
        std::time::Duration::from_secs(5),
        resolve_device_credentials,
        Arc::clone(dial_policy),
    );
    let mut members: Vec<Arc<dyn DevicePollerFactory>> = vec![Arc::new(zowietek)];
    if let Some(delivery) = cast_delivery {
        match TlsCastConnector::new(Arc::clone(dial_policy)) {
            Ok(connector) => members.push(Arc::new(CastSessionFactory::new(
                Arc::new(connector),
                Arc::clone(delivery),
                CastSessionConfig::default(),
            ))),
            Err(e) => tracing::warn!(
                error = %e,
                "cast TLS connector did not build; running without the cast driver"
            ),
        }
    }
    Arc::new(DevicePollerRegistry::with_factory(Arc::new(
        CompositePollerFactory::new(members),
    )))
}

/// The default no-op poller registry (no `devices-net` feature): no live device
/// transport, so no poller is spawned and the projection routes stay honestly
/// empty — exactly the pre-DEV-A4 behaviour. The unused delivery map keeps the
/// call site feature-free.
#[cfg(not(feature = "devices-net"))]
fn device_poller_registry(
    _cast_delivery: Option<&std::sync::Arc<multiview_control::devices::cast::media::CastDelivery>>,
    _dial_policy: &std::sync::Arc<multiview_config::device::net_guard::DialPolicy>,
) -> Arc<multiview_control::devices::DevicePollerRegistry> {
    Arc::new(multiview_control::devices::DevicePollerRegistry::new())
}

/// Build the outbound-dial SSRF policy (SEC-02/SEC-04, CWE-918) an operator sets
/// to **tighten** the dial to their own device subnet(s) — a list of CIDRs (e.g.
/// `["192.168.0.0/16", "fd00:db8::/32"]`).
///
/// Precedence: the validated [`control.device_dial_allow`](multiview_config::ControlConfig::device_dial_allow)
/// config field is the config-as-code home for the allowlist; the
/// `MULTIVIEW_DEVICE_DIAL_ALLOW` env var (comma/whitespace-separated) is the
/// legacy source, kept as a fallback during the migration off it. Absent from
/// both ⇒ the non-breaking default: LAN devices (private/ULA/carrier) stay
/// dialable while the never-legitimate ranges (loopback, link-local incl. the
/// cloud-metadata IP, unspecified, multicast/broadcast) stay blocked. An
/// unparseable entry logs a warning and falls back — fail-closed — to denying
/// every allowlistable range (globally-routable hosts only), never silently
/// widening back to the LAN default the operator was tightening away from.
/// Build, bind and spawn the ADR-0183 automation executor; returns the state
/// carrying it so the routes can read status and trigger manual restores.
fn start_event_automation(
    state: AppState,
    provider: multiview_control::source_events::SharedSourceBreakProvider,
) -> AppState {
    use multiview_control::automation::{
        http_webhook_sink, AutomationExecutor, CommandSink, StateConfigProvider,
    };
    let config = Arc::new(StateConfigProvider::new());
    config.bind(state.clone());
    let audit_state = state.clone();
    let audit: multiview_control::automation::AuditSink = Arc::new(move |id, detail| {
        audit_state.audit(
            "automation",
            multiview_control::audit::AuditAction::Command,
            "source",
            id,
            Some(detail),
        );
    });
    let sink: Arc<dyn CommandSink> = Arc::new(state.commands.clone());
    let executor = Arc::new(AutomationExecutor::new(
        provider,
        sink,
        config,
        audit,
        Some(http_webhook_sink(Arc::clone(&state.dial_policy))),
    ));
    let _task = executor.spawn();
    state.with_automation(executor)
}

fn device_dial_policy(
    control: Option<&multiview_config::ControlConfig>,
) -> Arc<multiview_config::device::net_guard::DialPolicy> {
    use multiview_config::device::net_guard::DialPolicy;

    // The config field wins when present (it is validated at config load).
    if let Some(cidrs) = control.and_then(|c| c.device_dial_allow.as_deref()) {
        let policy = match DialPolicy::from_cidrs(cidrs.iter().map(String::as_str)) {
            Ok(policy) => {
                tracing::info!(
                    entries = cidrs.len(),
                    "device-dial allowlist loaded from control.device_dial_allow"
                );
                policy
            }
            // Rejected already at config load; fail closed here too rather than
            // dialling wide, in case an unvalidated config ever reaches this.
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "control.device_dial_allow has an invalid CIDR; denying all internal \
                     dial targets (only globally-routable hosts are dialled)"
                );
                DialPolicy::deny_all_allowlistable()
            }
        };
        return Arc::new(policy);
    }

    let policy = match std::env::var("MULTIVIEW_DEVICE_DIAL_ALLOW") {
        Ok(raw) if !raw.trim().is_empty() => {
            let cidrs: Vec<&str> = raw
                .split([',', ' ', '\t', '\n'])
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .collect();
            match DialPolicy::from_cidrs(cidrs.iter().copied()) {
                Ok(policy) => {
                    tracing::info!(
                        entries = cidrs.len(),
                        "device-dial allowlist loaded from MULTIVIEW_DEVICE_DIAL_ALLOW"
                    );
                    policy
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "MULTIVIEW_DEVICE_DIAL_ALLOW has an invalid CIDR; denying all internal \
                         dial targets (only globally-routable hosts are dialled)"
                    );
                    DialPolicy::deny_all_allowlistable()
                }
            }
        }
        _ => DialPolicy::allow_lan(),
    };
    Arc::new(policy)
}

/// Resolve a managed device's `(username, password)` from its
/// `auth.secret_ref` (DEV-A4). The secret reference is read from the secret
/// store the deployment configures (1Password `op://…`, an environment
/// variable, etc.) — credentials never live in the config model.
///
/// This build resolves a `secret_ref` of the form `env:VAR_USER:VAR_PASS` from
/// the environment (no extra dependency, works in every deployment); an
/// `op://…` ref or any other scheme returns `None` (the device is then not
/// polled — it cannot be logged into — and rides ADOPTING until a resolvable
/// credential is configured). A device with no `auth` block returns `None`.
#[cfg(feature = "devices-net")]
fn resolve_device_credentials(device: &multiview_config::Device) -> Option<(String, String)> {
    let secret_ref = device.auth.as_ref().map(|a| a.secret_ref.as_str())?;
    // `env:USER_VAR:PASS_VAR` — read the username/password from two env vars.
    let rest = secret_ref.strip_prefix("env:")?;
    let (user_var, pass_var) = rest.split_once(':')?;
    let username = std::env::var(user_var).ok()?;
    let password = std::env::var(pass_var).ok()?;
    Some((username, password))
}

/// One HLS delivery mount derived from a configured HLS/LL-HLS output: the
/// route prefix on the control listener and the on-disk directory it serves
/// (the configured playlist's parent — where the segmenter writes), plus the
/// identity the Cast delivery map ([`cast_delivery`], DEV-D2) joins on — the
/// output id this mount serves and the playlist file name under the mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsMount {
    /// Route prefix, e.g. `/hls/program`.
    pub route: String,
    /// The served directory (the output playlist's parent directory).
    pub dir: std::path::PathBuf,
    /// The configured output's stable id this mount serves.
    pub output_id: String,
    /// The playlist file name under [`route`](Self::route) (the configured
    /// path's final component), e.g. `multiview.m3u8`. [`None`] when the
    /// configured path has no usable file name (a directory-shaped path) —
    /// the mount still serves, but no Cast media URL can be derived from it.
    pub playlist: Option<String>,
}

/// Derive the `/hls/{output-id}` delivery mounts for every HLS/LL-HLS output
/// in `config`.
///
/// The mount segment is the output's stable id ([`multiview_config::Output::id`])
/// sanitised to URL-segment-safe characters (alphanumerics and `-`/`_`/`.`/`~`
/// kept, everything else mapped to `-`; a segment that sanitises to nothing
/// usable becomes `out`). Distinct resolved ids that collide *after*
/// sanitisation are deduplicated with a deterministic `-2`, `-3`, … suffix in
/// declaration order, so every configured output stays reachable.
///
/// Delegates the segment/dedup/playlist resolution to
/// [`multiview_config::playback::resolve_hls_mounts`] — the SAME function the
/// control-plane's `GET /api/v1/outputs/{id}/playback` route uses (ADR-0101),
/// so a viewer playback URL can never name a mount this router does not
/// actually serve.
#[must_use]
pub fn hls_mounts(config: &MultiviewConfig) -> Vec<HlsMount> {
    use multiview_config::Output;
    let hls_outputs: Vec<(String, &str)> = config
        .outputs
        .iter()
        .filter_map(|output| match output {
            Output::Hls { path, .. } | Output::LlHls { path, .. } => {
                Some((output.id(), path.as_str()))
            }
            _ => None,
        })
        .collect();
    let refs: Vec<(&str, &str)> = hls_outputs
        .iter()
        .map(|(id, path)| (id.as_str(), *path))
        .collect();
    let resolved = multiview_config::playback::resolve_hls_mounts(&refs);
    hls_outputs
        .into_iter()
        .filter_map(|(output_id, path)| {
            let route = resolved.get(&output_id)?;
            let dir = match std::path::Path::new(path).parent() {
                Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
                // A bare filename (or a root path): serve the process working dir
                // (where such a playlist would be written).
                _ => std::path::PathBuf::from("."),
            };
            Some(HlsMount {
                route: route.route.clone(),
                dir,
                output_id,
                playlist: route.playlist.clone(),
            })
        })
        .collect()
}

/// The Cast receiver app id this run advertises (ADR-0111):
/// `MULTIVIEW_CAST_APP_ID` (when set and non-empty) wins over
/// `[control] cast_app_id`; either must be 8 hex characters (used
/// upper-case), else a startup warning and `None`.
fn cast_app_id(config: &MultiviewConfig) -> Option<String> {
    let env = std::env::var("MULTIVIEW_CAST_APP_ID")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let (raw, source) = match env {
        Some(value) => (value, "MULTIVIEW_CAST_APP_ID"),
        None => (
            config.control.as_ref()?.cast_app_id.clone()?,
            "control.cast_app_id",
        ),
    };
    let normalized = multiview_config::normalize_cast_app_id(&raw);
    if normalized.is_none() {
        tracing::warn!(
            source,
            "the Cast receiver app id is not 8 hexadecimal characters; no custom receiver is advertised"
        );
    }
    normalized
}

/// Build the Cast **delivery map** (DEV-D2, ADR-M011): the validated
/// `control.cast_media_base` × the [`hls_mounts`] this listener serves, giving
/// output id → the device-reachable playlist URL a Cast session `LOAD`s.
///
/// [`None`] when no `cast_media_base` is configured (the cast-session routes
/// then refuse with an honest `409`) or when the configured base fails the
/// driver's host rules (loopback / `.local` / bare LAN name — warned loudly,
/// Cast delivery disabled rather than handing devices an unreachable URL).
/// The segment format is MPEG-TS for every mount: the DEV-D1 run-path
/// segmenter writes `.ts` segments only (see
/// `multiview_output::hls::live::LivePlaylist`); signal `fmp4` here once a
/// rendition actually serves CMAF.
#[must_use]
pub fn cast_delivery(
    config: &MultiviewConfig,
) -> Option<std::sync::Arc<multiview_control::devices::cast::media::CastDelivery>> {
    use multiview_control::devices::cast::media::{
        CastDelivery, CastMediaBase, CastMediaTarget, HlsSegmentFormat,
    };
    let base = config.control.as_ref()?.cast_media_base.as_deref()?;
    let base = match CastMediaBase::parse(base) {
        Ok(base) => base,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "control.cast_media_base rejected; Cast delivery is disabled \
                 (cast sessions will refuse with 409)"
            );
            return None;
        }
    };
    let mut delivery = CastDelivery::new().with_base(base.clone());
    for mount in hls_mounts(config) {
        let Some(playlist) = mount.playlist else {
            tracing::warn!(
                output = %mount.output_id,
                route = %mount.route,
                "HLS output path has no playlist file name; not castable"
            );
            continue;
        };
        // A configured discrete track has a separate audio-only rendition.
        // Cast LOAD must use the master to expose its player audio menu;
        // the legacy media playlist stays reachable unchanged.
        let selectable = config
            .outputs
            .iter()
            .find(|output| output.id() == mount.output_id)
            .and_then(multiview_config::Output::effective_audio)
            .is_some_and(|selection| {
                let names: Vec<&str> = config
                    .audio
                    .as_ref()
                    .map(|audio| {
                        audio
                            .routes
                            .iter()
                            .filter_map(|route| route.target_track.as_deref())
                            .collect()
                    })
                    .unwrap_or_default();
                !selection.resolve(&names).1.is_empty()
            });
        let playback = if selectable { "master.m3u8" } else { &playlist };
        delivery.insert(
            &mount.output_id,
            CastMediaTarget {
                url: base.join(&mount.route, playback),
                format: HlsSegmentFormat::MpegTs,
            },
        );
    }
    Some(std::sync::Arc::new(delivery))
}

/// Project a composited program frame into the compact JSON snapshot the control
/// plane republishes from the wait-free latest-state slot (`EngineStateSnapshot`
/// is an opaque `serde_json::Value`, so the engine state shape stays decoupled
/// from the control plane). Kept intentionally small — schema tag, tick, output
/// PTS, and canvas geometry — so the per-tick serialization stays cheap on the
/// hot loop. Richer per-tile state is fed sparsely over the event stream as it
/// changes, not dumped here every frame.
#[must_use]
pub fn state_snapshot(tick: u64, pts_ns: i64, width: u32, height: u32) -> EngineStateSnapshot {
    serde_json::json!({
        "v": 1,
        "tick": tick,
        "pts_ns": pts_ns,
        "canvas": { "width": width, "height": height },
    })
}

/// Fold each input's [`StreamInventory`] into the conflated engine-state snapshot
/// blob under `inputs.<id>.streams` (RT-3, ADR-0034 §9).
///
/// This is the **off-engine** publish path for the read-only stream-inventory
/// discovery surface: the control plane's `GET /api/v1/inputs/{id}/streams`
/// reads exactly this fragment out of the conflated snapshot (inv #10). The
/// inventory is built by the ingest at `open()` — off the output-clock thread —
/// so threading it here only serialises an already-computed, static-after-open
/// value into the blob; it does **not** probe or block anything on the hot loop.
///
/// An empty `inventories` map leaves `snapshot` byte-identical (no `inputs` key
/// is added), so the synthetic / no-probe path keeps the minimal base blob.
///
/// `snapshot` must be a JSON object (the base [`state_snapshot`] blob); a
/// non-object value is left unchanged (defensive — the caller always passes the
/// base blob).
pub fn fold_input_inventories(
    snapshot: &mut EngineStateSnapshot,
    inventories: &std::collections::BTreeMap<String, multiview_core::stream::StreamInventory>,
) {
    let Some(fragment) = input_inventories_fragment(inventories) else {
        return;
    };
    if let Some(obj) = snapshot.as_object_mut() {
        obj.insert("inputs".to_owned(), fragment);
    }
}

/// Pre-serialise the per-input inventories into the `inputs` JSON fragment the
/// snapshot carries (`{ "<id>": { "streams": <StreamInventory> }, … }`), or
/// `None` when the map is empty.
///
/// Built **once** off the hot loop so the per-tick projection only has to
/// clone-and-insert this immutable fragment rather than re-serialise every
/// inventory each frame (the inventory is static after open). The control plane
/// reads `inputs.<id>.streams` straight back out as a [`StreamInventory`].
#[must_use]
pub fn input_inventories_fragment(
    inventories: &std::collections::BTreeMap<String, multiview_core::stream::StreamInventory>,
) -> Option<serde_json::Value> {
    if inventories.is_empty() {
        return None;
    }
    let mut inputs = serde_json::Map::with_capacity(inventories.len());
    for (id, inventory) in inventories {
        // `StreamInventory` is plain derived `Serialize` (no non-string map keys,
        // no failing path); the guardrails forbid `unwrap`/`expect`, so a
        // serialisation fault degrades to a `null` streams entry rather than
        // panicking on the publish path. In practice this never fires.
        let streams = serde_json::to_value(inventory).unwrap_or(serde_json::Value::Null);
        inputs.insert(id.clone(), serde_json::json!({ "streams": streams }));
    }
    Some(serde_json::Value::Object(inputs))
}

/// Insert a **pre-built** [`input_inventories_fragment`] into a snapshot blob
/// under `inputs` (the per-tick hot-loop projection path).
///
/// Cheaper than [`fold_input_inventories`] on the hot loop: the fragment is
/// serialised once at build time and only **cloned + inserted** here, so the
/// per-tick cost is one map clone of a tiny static value (no inventory
/// re-serialisation). A `None` fragment (no inputs probed) is a no-op, leaving
/// the blob unchanged (inv #10 — the publish never blocks anything).
pub fn insert_input_fragment(
    snapshot: &mut EngineStateSnapshot,
    fragment: Option<&serde_json::Value>,
) {
    let (Some(fragment), Some(obj)) = (fragment, snapshot.as_object_mut()) else {
        return;
    };
    obj.insert("inputs".to_owned(), fragment.clone());
}

/// Serialise the live push-output link records into the `outputs` snapshot
/// fragment: `{ "<id>": { "link": { state, mode, transport, url, clients,
/// sessions, dropped, discarded_no_client, discarded_awaiting_keyframe,
/// dropped_slow_peer, dropped_upstream, written, peers, last_error?,
/// last_session_error? } } }`, or `None` when the run has no push outputs.
/// `dropped` is the total of the four reason counters; `last_error` is current
/// link health, `last_session_error` is history. Reads atomics + lock-free
/// cells only (inv #10).
#[must_use]
pub fn push_links_fragment(
    links: &[(
        String,
        std::sync::Arc<multiview_output::push_link::PushLinkStatus>,
    )],
) -> Option<serde_json::Value> {
    if links.is_empty() {
        return None;
    }
    let mut outputs = serde_json::Map::with_capacity(links.len());
    for (id, link) in links {
        let snap = link.snapshot();
        let mut obj = serde_json::json!({
            "state": snap.state.as_str(),
            "mode": snap.mode,
            "transport": snap.transport,
            "url": snap.url,
            "clients": snap.clients,
            "sessions": snap.sessions,
            "dropped": snap.dropped,
            "discarded_no_client": snap.discarded_no_client,
            "discarded_awaiting_keyframe": snap.discarded_awaiting_keyframe,
            "dropped_slow_peer": snap.dropped_slow_peer,
            "dropped_upstream": snap.dropped_upstream,
            "written": snap.written,
            "peers": snap.peers,
        });
        if let Some(map) = obj.as_object_mut() {
            if let Some(err) = snap.last_error {
                map.insert("last_error".to_owned(), serde_json::Value::String(err));
            }
            if let Some(err) = snap.last_session_error {
                map.insert(
                    "last_session_error".to_owned(),
                    serde_json::Value::String(err),
                );
            }
        }
        outputs.insert(id.clone(), serde_json::json!({ "link": obj }));
    }
    Some(serde_json::Value::Object(outputs))
}

/// Insert a pre-built [`push_links_fragment`] under `outputs` (no-op on `None`).
pub fn insert_output_fragment(
    snapshot: &mut EngineStateSnapshot,
    fragment: Option<&serde_json::Value>,
) {
    let (Some(fragment), Some(obj)) = (fragment, snapshot.as_object_mut()) else {
        return;
    };
    obj.insert("outputs".to_owned(), fragment.clone());
}

/// Fold each source's current lifecycle state into the conflated engine-state
/// snapshot blob as `tiles: [{ "id", "state" }, …]`, sorted by id, using the
/// SAME [`multiview_events::LifecycleState`] wire strings the `tile.state`
/// events carry (`LIVE`/`STALE`/`RECONNECTING`/`NO_SIGNAL`).
///
/// The control plane reads this fragment back out at client connect to emit
/// the `tiles` `$snapshot` baseline (realtime-api §5), so a fresh page shows
/// the current per-tile state without waiting for the next sparse delta.
///
/// Per-tick cost: one small Vec build + sort over the source map (tiles are
/// few) into the wait-free, conflated latest-state slot — never a channel a
/// client can fill (inv #10). An empty map still inserts `tiles: []` so a
/// connected client rebuilds to an EMPTY cache, never a stale one. A
/// non-object `snapshot` is left unchanged (defensive — the caller always
/// passes the base [`state_snapshot`] blob).
pub fn fold_tile_states<S: std::hash::BuildHasher>(
    snapshot: &mut EngineStateSnapshot,
    source_states: &std::collections::HashMap<String, multiview_core::traits::SourceState, S>,
) {
    let Some(obj) = snapshot.as_object_mut() else {
        return;
    };
    // Sort by id: HashMap iteration order is non-deterministic and the wire
    // (and golden tests) must not be.
    let mut entries: Vec<(&str, multiview_core::traits::SourceState)> = source_states
        .iter()
        .map(|(id, &state)| (id.as_str(), state))
        .collect();
    entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
    let offsets = crate::av_sync::offsets();
    // The last `youtube` resolve-failure reason per source (ADR-0015), empty on
    // a build without the `youtube` feature or when nothing has ever failed.
    #[cfg(feature = "youtube")]
    let youtube_reasons = crate::youtube_status::last_errors();
    let tiles: Vec<serde_json::Value> = entries
        .into_iter()
        .map(|(id, state)| {
            // `LifecycleState` is a plain unit-variant enum: serialising it is
            // infallible in practice; the guardrails forbid `unwrap`, so a
            // (never-occurring) fault degrades to a `null` state the control
            // plane skips rather than panicking on the publish path.
            let state = serde_json::to_value(multiview_events::LifecycleState::from(state))
                .unwrap_or(serde_json::Value::Null);
            let mut tile = serde_json::json!({ "id": id, "state": state });
            // On-air lip-sync error of a live source (video raw PTS − audio raw
            // PTS at the same output instant, ms; + = audio late). Absent until
            // measured.
            if let (Some(o), Some(obj)) = (offsets.get(id), tile.as_object_mut()) {
                obj.insert("av_offset_ms".to_owned(), serde_json::json!(o));
            }
            // The honest `youtube` resolve-failure reason (e.g. "not a live
            // stream (status: upcoming)", "yt-dlp not found on the server"),
            // under the documented `reason` field (realtime-api.md §3's tiles
            // `$snapshot`) — the same field name a client already defensively
            // parses. Absent for every non-`youtube` source and for a healthy
            // one.
            #[cfg(feature = "youtube")]
            if let (Some(reason), Some(obj)) = (youtube_reasons.get(id), tile.as_object_mut()) {
                obj.insert("reason".to_owned(), serde_json::json!(reason));
            }
            tile
        })
        .collect();
    obj.insert("tiles".to_owned(), serde_json::Value::Array(tiles));
}

/// Build one `input.streams` realtime event per input from its [`StreamInventory`]
/// (RT-3): the delta clients see when an input's inventory first appears or
/// changes on re-probe.
///
/// Deterministic order (the `BTreeMap` is id-sorted) and exactly one event per
/// input — no duplicates. Each event rides the existing `inputs` topic
/// ([`multiview_control::realtime::topic_for_event`]); the engine publishes them
/// through the wait-free drop-oldest broadcast, never a channel a client can
/// fill (inv #10).
#[must_use]
pub fn input_streams_events(
    inventories: &std::collections::BTreeMap<String, multiview_core::stream::StreamInventory>,
) -> Vec<Event> {
    inventories
        .iter()
        .map(|(id, inventory)| {
            Event::InputStreams(multiview_events::InputStreams::new(
                id.clone(),
                inventory.clone(),
            ))
        })
        .collect()
}

/// Rebind the cell identified by `tile` to source `source` in `config`, in place.
///
/// Returns `true` if a cell with that id existed and was rebound (so the caller
/// re-solves + applies), `false` if no such cell — an unknown tile id is ignored
/// rather than an error (the command simply has no effect). The new binding is
/// validated downstream by [`MultiviewConfig::solve_layout`], so a `source` that
/// is not a declared input is rejected there (the layout is never swapped to an
/// invalid one).
fn apply_swap_source(config: &mut MultiviewConfig, tile: &str, source: &str) -> bool {
    let Some(cell) = config.cells.iter_mut().find(|c| c.id == tile) else {
        return false;
    };
    cell.source.input_id = Some(source.to_owned());
    cell.source.kind = None;
    cell.source.name = None;
    cell.source.url = None;
    true
}

/// With a switcher M/E declared, the Program drive's one cell (id and slate),
/// from the same [`MultiviewConfig::program_canvas`] `Pipeline::build` uses
/// (#307). `None` without an M/E, or when the document does not solve (the
/// build would have refused it).
fn switcher_program_cell(
    config: &MultiviewConfig,
) -> Option<(String, multiview_config::FailoverSlate)> {
    config.switcher.as_ref()?.mix_effects.first()?;
    let program = config.program_canvas(config.solve_layout().ok()?).ok()?;
    Some((program.cell_id?, program.slate))
}

/// Re-solve the working `config` and hot-swap it onto `drive`, returning `true`
/// on a successful apply.
///
/// Mirrors the existing [`Command::SwapSource`] apply path: a re-solve failure or
/// a compositor rejection logs `tracing::warn!` and keeps the last-good layout
/// (`set_layout` retains it on error), so the output clock never adopts a bad one
/// and never stalls (invariants #1 + #10). Panic-free: no `unwrap`/indexing.
fn resolve_and_apply(config: &MultiviewConfig, drive: &mut CompositorDrive<Nv12Image>) -> bool {
    match config.solve_layout() {
        Ok(layout) => match drive.set_layout(Arc::new(layout)) {
            Ok(()) => true,
            Err(e) => {
                // The compositor rejected the re-solved layout; keep the
                // last-good one (set_layout retains it on error) and log.
                tracing::warn!(error = %e, "rejected a control-plane layout swap");
                false
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "control-plane command produced an invalid layout; ignored");
            false
        }
    }
}

/// Build the engine's per-tick control hook that drains the command bus and
/// applies operational commands to the running compositor at the frame boundary,
/// emitting each command's outcome on the realtime event stream.
///
/// Returned as an `FnMut(&mut CompositorDrive<Nv12Image>)` wrapping a
/// [`CommandDrain`]: each tick it [`try_drain`](CommandReceiver::try_drain)s the
/// **non-blocking** queue (usually empty — O(pending), never awaits), classifies
/// each command, and publishes an outcome [`Event`] via
/// [`EnginePublisher::publish_event`] — which is **drop-oldest and never awaits a
/// client**, so emitting an outcome can never back-pressure the engine
/// (invariant #10). Applying at the frame boundary keeps the output clock
/// unstalled (invariant #1): the drain only mutates the active binding and emits
/// drop-oldest events; it never blocks.
///
/// Per command:
/// * [`Command::Start`]/[`Command::Stop`] flip the `running` flag and emit an
///   [`Event::OutputStatus`] (`Running` / `Idle`). There is no output server wired
///   in the software engine yet, so this is the running-state echo, not a measured
///   sink status.
/// * [`Command::SwapSource`] / [`Command::RouteVideo`] are VIDEO→cell re-points:
///   each is desugared via [`Command::route_intent`] into a
///   [`RouteIntent::Video`] and applied through the canonical, engine-tested
///   [`RouteApplier::apply_video`] → **O(1)** [`CompositorDrive::rebind_cell`] (no
///   `solve_layout`/`validate` re-solve), batched + capped at
///   [`MAX_REPOINTS_PER_TICK`] per tick. `SwapSource` is the desugared alias of
///   `RouteVideo{…,Video,Best}`, so the two apply identically (back-compat). No
///   dedicated swap event exists in [`Event`], so the observable outcome is the
///   binding change plus a `tracing` log.
/// * [`Command::RouteSubtitle`] re-points a subtitle **layer** to another source's
///   cues via the run's live [`SubtitleRouteHandle`](crate::captions::SubtitleRouteHandle)
///   seam (RT-10b), threaded in by [`command_drain_with_seams`]. The seam applies
///   the re-point at the bake consumer's sample boundary (the engine
///   [`SubtitleLayer::repoint`](multiview_overlay::SubtitleLayer) the
///   [`RouteApplier`] drives in-engine). Without a seam (the software-engine path,
///   which renders no subtitles) the route is a logged held action, never a panic.
/// * [`Command::RouteAudio`] resolves a configured channel and source's live
///   audio store, then publishes to the bounded audio control slot. The consumer
///   calls `ProgramBus::repoint_crossfade` before the next audio tick. Without
///   program audio or a known store/target, it is a logged held action.
/// * [`Command::ApplyLayout`] **with a route-resolved document** (ADR-W019)
///   swaps the STORED layout in at the frame boundary — geometry, bindings,
///   cell ids, and per-cell `on_loss` slates — in O(cells) with no I/O and no
///   re-solve (the route already solved + validated it), mirrors it into the
///   working config, and emits a `job.progress` outcome. A pinned-canvas
///   mismatch (Class-2, ADR-R004 — compared by VALUE via `Canvas::same_signal`,
///   so an equivalent non-reduced cadence applies) or a compositor rejection is
///   held — warned AND surfaced as a `job.progress` `apply_layout_held` outcome
///   — never adopted. **Without a document** (back-compat) it re-solves +
///   re-applies the working layout iff `layout` matches the solved working
///   layout's name; any other id is a failure — logged via `tracing::warn!`,
///   never a panic.
/// * [`Command::ArmSalvo`] stages a named salvo and emits [`Event::SalvoArmed`];
///   [`Command::TakeSalvo`] enqueues the named-or-armed salvo's source recalls as
///   coalesced re-points (one capped pass, O(1) each) and emits
///   [`Event::SalvoTaken`]; [`Command::CancelSalvo`] discards the staged salvo and
///   emits [`Event::SalvoCancelled`]. Only the salvo's `sources` are applied; the
///   layout/tally/umd sub-recalls are a follow-up.
/// * [`Command::SetTallyOverride`] has no tally arbiter in the software engine
///   yet, so it emits an [`Event::TallyState`] echo (the forced colour, or the
///   `Off`/default state when cleared) rather than silently no-op'ing.
///
/// Every arm is panic-free: no `unwrap`/`expect`/indexing. An unknown cell,
/// layout, or salvo logs `tracing::warn!` and emits nothing (or a tally echo),
/// never panics.
pub fn command_drain(
    commands: CommandReceiver,
    config: MultiviewConfig,
    publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
) -> impl FnMut(&mut CompositorDrive<Nv12Image>) {
    let mut drain = CommandDrain::new(commands, config, publisher);
    move |drive: &mut CompositorDrive<Nv12Image>| {
        let _applied = drain.apply(drive);
    }
}

/// Build the per-tick control hook **with the live-source seam** (ADR-W018)
/// threaded in, so `UpsertSource`/`RemoveSource` apply to the running engine:
/// the drain registers/unregisters the source's frame store + route key at the
/// frame boundary (cheap binding mutations) and hands every heavy step
/// (producer spawn/teardown, preview registry) to the
/// [`LiveSourceHub`](crate::live_sources::LiveSourceHub) behind `live` over a
/// bounded, non-blocking channel (invariants #1 + #10). The binary wires this
/// on the software-engine run path.
pub fn command_drain_with_live_sources(
    commands: CommandReceiver,
    config: MultiviewConfig,
    publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    live: crate::live_sources::LiveSourceHandle,
) -> impl FnMut(&mut CompositorDrive<Nv12Image>) {
    command_drain_with_live_sources_and_players(
        commands,
        config,
        publisher,
        live,
        std::collections::HashMap::new(),
    )
}

/// As [`command_drain_with_live_sources`], plus the media-player transport seam
/// (ADR-0057 / ADR-0097): the per-player [`TransportMailbox`](crate::player::TransportMailbox)
/// map the command drain submits a `MediaTransport`/exit verb to. The production
/// **non-overlay** full-engine path wires this so player channels are drivable
/// even without subtitle/overlay rendering; the existing 4-arg form (no players)
/// is unchanged for callers that have none.
// The map always originates from `Pipeline::player_mailboxes()` (the default
// hasher); generalizing over the hasher buys no caller anything here.
#[allow(clippy::implicit_hasher)]
pub fn command_drain_with_live_sources_and_players(
    commands: CommandReceiver,
    config: MultiviewConfig,
    publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    live: crate::live_sources::LiveSourceHandle,
    player_mailboxes: std::collections::HashMap<
        String,
        std::sync::Arc<crate::player::TransportMailbox>,
    >,
) -> impl FnMut(&mut CompositorDrive<Nv12Image>) {
    let mut drain = CommandDrain::new(commands, config, publisher)
        .with_live_sources(live)
        .with_player_mailboxes(player_mailboxes);
    move |drive: &mut CompositorDrive<Nv12Image>| {
        let _applied = drain.apply(drive);
    }
}

/// Build the non-overlay full run with its live audio slot as well as source
/// and player seams. The legacy form remains for software-only callers.
#[cfg(feature = "ffmpeg")]
#[allow(clippy::implicit_hasher)]
pub fn command_drain_with_audio_and_players(
    commands: CommandReceiver,
    config: MultiviewConfig,
    publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    audio_route: Arc<multiview_framestore::SwapCell<Option<Arc<crate::audio::AudioControlHandle>>>>,
    live: crate::live_sources::LiveSourceHandle,
    player_mailboxes: std::collections::HashMap<String, Arc<crate::player::TransportMailbox>>,
    display_layout: crate::display_size::DisplayLayoutSlot,
) -> impl FnMut(&mut CompositorDrive<Nv12Image>) {
    let mut drain = CommandDrain::new(commands, config, publisher)
        .with_display_layout(display_layout)
        .with_audio_route(audio_route)
        .with_live_sources(live)
        .with_player_mailboxes(player_mailboxes);
    move |drive: &mut CompositorDrive<Nv12Image>| {
        let _applied = drain.apply(drive);
    }
}

/// Build the per-tick control hook **with the live overlay seam** (ADR-W022)
/// threaded in, so `UpsertOverlay`/`RemoveOverlay` apply to the running
/// engine: the drain mirrors the document into the working overlay set and
/// publishes the set (generation-bumped) through the lock-free
/// [`OverlayApplySlot`](crate::live_overlays::OverlayApplySlot) the bake
/// consumer re-derives from at its next frame — pure data mutation at the
/// frame boundary, no rasterization, no I/O (invariants #1 + #10).
pub fn command_drain_with_live_overlays(
    commands: CommandReceiver,
    config: MultiviewConfig,
    publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    overlays: crate::live_overlays::OverlayApplySlot,
) -> impl FnMut(&mut CompositorDrive<Nv12Image>) {
    let mut drain = CommandDrain::new(commands, config, publisher).with_live_overlays(overlays);
    move |drive: &mut CompositorDrive<Nv12Image>| {
        let _applied = drain.apply(drive);
    }
}

/// Build the per-tick control hook **with the live run-side routing seams**
/// threaded in, so per-stream routing commands reach their live crosspoints in the
/// real run (RT-11 / ADR-0034).
///
/// Identical to [`command_drain`] but also accepts the running pipeline's shared
/// **subtitle re-point slot**
/// ([`Pipeline::subtitle_route_slot`](crate::pipeline::Pipeline::subtitle_route_slot)):
/// a [`Command::RouteSubtitle`] drained here drives a breakaway into the running
/// pipeline through that slot's live
/// [`SubtitleRouteHandle`](crate::captions::SubtitleRouteHandle) (RT-10b) — the
/// run applies it at the next sample boundary via the engine
/// [`SubtitleLayer::repoint`](multiview_overlay::SubtitleLayer). Reading the slot
/// is a `SwapCell` load and publishing a re-point is wait-free +
/// bounded drop-oldest, so neither can pace or stall the output clock
/// (invariants #1/#10).
///
/// It also threads in the pipeline's **live overlay slot**
/// ([`Pipeline::overlay_apply_slot`](crate::pipeline::Pipeline::overlay_apply_slot),
/// ADR-W022) so `UpsertOverlay`/`RemoveOverlay` re-derive the running bake.
///
/// The binary wires this on the full libav\* path (`run_pipeline_until_ctrl_c`),
/// where the pipeline has a subtitle router; the software-engine path (no subtitle
/// rendering) wires the plain [`command_drain`].
// The player-mailbox map always originates from `Pipeline::player_mailboxes()`
// (the default hasher); generalizing over the hasher buys no caller anything.
#[allow(clippy::implicit_hasher, clippy::too_many_arguments)]
#[cfg(all(feature = "ffmpeg", feature = "overlay"))]
pub fn command_drain_with_seams(
    commands: CommandReceiver,
    config: MultiviewConfig,
    publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    subtitle_route: Arc<
        multiview_framestore::SwapCell<Option<Arc<crate::captions::SubtitleRouteHandle>>>,
    >,
    audio_route: Arc<multiview_framestore::SwapCell<Option<Arc<crate::audio::AudioControlHandle>>>>,
    overlays: crate::live_overlays::OverlayApplySlot,
    tiles: Arc<multiview_framestore::SwapCell<Arc<Vec<crate::overlays::TileSpec>>>>,
    audio_free: crate::pipeline::AudioFreeSources,
    live: crate::live_sources::LiveSourceHandle,
    player_mailboxes: std::collections::HashMap<
        String,
        std::sync::Arc<crate::player::TransportMailbox>,
    >,
    display_layout: crate::display_size::DisplayLayoutSlot,
) -> impl FnMut(&mut CompositorDrive<Nv12Image>) {
    let mut drain = CommandDrain::new(commands, config, publisher)
        .with_display_layout(display_layout)
        .with_subtitle_route(subtitle_route)
        .with_audio_route(audio_route)
        .with_live_overlays(overlays)
        .with_tile_spec_apply(tiles)
        .with_audio_free_sources(audio_free)
        .with_live_sources(live)
        .with_player_mailboxes(player_mailboxes);
    move |drive: &mut CompositorDrive<Nv12Image>| {
        let _applied = drain.apply(drive);
    }
}

/// [`command_drain_with_seams`] plus the demand seam, so binding changes take
/// and release source leases. The production `overlay` run path wires this.
#[cfg(all(feature = "ffmpeg", feature = "overlay"))]
#[allow(clippy::too_many_arguments, clippy::implicit_hasher)]
pub fn command_drain_with_demand(
    commands: CommandReceiver,
    config: MultiviewConfig,
    publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    subtitle_route: Arc<
        multiview_framestore::SwapCell<Option<Arc<crate::captions::SubtitleRouteHandle>>>,
    >,
    audio_route: Arc<multiview_framestore::SwapCell<Option<Arc<crate::audio::AudioControlHandle>>>>,
    overlays: crate::live_overlays::OverlayApplySlot,
    tiles: Arc<multiview_framestore::SwapCell<Arc<Vec<crate::overlays::TileSpec>>>>,
    audio_free: crate::pipeline::AudioFreeSources,
    live: crate::live_sources::LiveSourceHandle,
    player_mailboxes: std::collections::HashMap<String, Arc<crate::player::TransportMailbox>>,
    display_layout: crate::display_size::DisplayLayoutSlot,
    demand: Option<crate::source_demand::DrainDemand<Nv12Image>>,
) -> impl FnMut(&mut CompositorDrive<Nv12Image>) {
    let mut drain = CommandDrain::new(commands, config, publisher)
        .with_display_layout(display_layout)
        .with_subtitle_route(subtitle_route)
        .with_audio_route(audio_route)
        .with_live_overlays(overlays)
        .with_tile_spec_apply(tiles)
        .with_audio_free_sources(audio_free)
        .with_live_sources(live)
        .with_player_mailboxes(player_mailboxes)
        .with_demand(demand);
    move |drive: &mut CompositorDrive<Nv12Image>| {
        let _applied = drain.apply(drive);
    }
}

/// [`command_drain_with_live_sources`] plus the demand seam.
#[cfg(feature = "ffmpeg")]
pub fn command_drain_with_live_sources_and_demand(
    commands: CommandReceiver,
    config: MultiviewConfig,
    publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    live: crate::live_sources::LiveSourceHandle,
    demand: Option<crate::source_demand::DrainDemand<Nv12Image>>,
) -> impl FnMut(&mut CompositorDrive<Nv12Image>) {
    let mut drain = CommandDrain::new(commands, config, publisher)
        .with_live_sources(live)
        .with_demand(demand);
    move |drive: &mut CompositorDrive<Nv12Image>| {
        let _applied = drain.apply(drive);
    }
}

/// [`command_drain_with_audio_and_players`] plus the demand seam (the
/// non-overlay production run path).
#[cfg(feature = "ffmpeg")]
#[allow(clippy::implicit_hasher, clippy::too_many_arguments)]
pub fn command_drain_with_audio_players_and_demand(
    commands: CommandReceiver,
    config: MultiviewConfig,
    publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    audio_route: Arc<multiview_framestore::SwapCell<Option<Arc<crate::audio::AudioControlHandle>>>>,
    live: crate::live_sources::LiveSourceHandle,
    player_mailboxes: std::collections::HashMap<String, Arc<crate::player::TransportMailbox>>,
    display_layout: crate::display_size::DisplayLayoutSlot,
    demand: Option<crate::source_demand::DrainDemand<Nv12Image>>,
) -> impl FnMut(&mut CompositorDrive<Nv12Image>) {
    let mut drain = CommandDrain::new(commands, config, publisher)
        .with_display_layout(display_layout)
        .with_audio_route(audio_route)
        .with_live_sources(live)
        .with_player_mailboxes(player_mailboxes)
        .with_demand(demand);
    move |drive: &mut CompositorDrive<Nv12Image>| {
        let _applied = drain.apply(drive);
    }
}

/// The maximum number of VIDEO→cell re-points applied in **one** frame-boundary
/// pass (RT-6 / ADR-0034 cap-per-tick).
///
/// A pathological salvo storm of K re-points cannot blow the per-tick budget: at
/// most this many are applied per tick (each an O(1) `rebind_cell`), the rest
/// stay in a bounded backlog and are applied on subsequent ticks (or dropped
/// once the backlog itself is full — bounded memory, drop-oldest, never grows).
/// Sized generously relative to any plausible single-tick operator action while
/// still bounding the worst case.
pub const MAX_REPOINTS_PER_TICK: usize = 32;

/// Hard cap on the deferred-re-point backlog. Beyond this the **oldest** pending
/// re-point is dropped (the newest binding for a cell is what the operator wants;
/// an old superseded one being shed is harmless). Bounded data-plane-adjacent
/// memory (safety rule §5: queues drop, never grow).
const MAX_REPOINT_BACKLOG: usize = 256;

/// Map a control-plane [`MediaTransportVerb`](multiview_control::MediaTransportVerb)
/// onto the CLI player's [`TransportVerb`](crate::player::TransportVerb) (the two
/// are structurally identical but live in different crates — the control crate
/// owns the wire/REST shape, the cli owns the ingest-thread transport core).
fn map_transport_verb(
    verb: &multiview_control::MediaTransportVerb,
) -> crate::player::TransportVerb {
    use crate::player::TransportVerb as V;
    use multiview_control::MediaTransportVerb as W;
    match verb {
        W::Load { asset } => V::Load {
            asset: asset.clone(),
        },
        W::Cue { frame } => V::Cue { frame: *frame },
        W::Play => V::Play,
        W::Pause => V::Pause,
        W::Stop => V::Stop,
        // A `Seek` with an explicit frame seeks there.
        W::Seek { frame: Some(f) } => V::Seek { frame: *f },
        // A `Seek { None }` (seek to the declared in-point) maps to the executor's
        // in-point cue; likewise any future `#[non_exhaustive]` verb this build
        // does not know about maps to a harmless re-cue rather than panicking.
        _ => V::Cue { frame: None },
    }
}

/// The per-tick command-drain machine: it owns the non-blocking command bus, the
/// working config, the outbound publisher, and the across-tick state, and applies
/// drained commands to the running [`CompositorDrive`] at the frame boundary.
///
/// Per-stream routing commands (`SwapSource`/`RouteVideo`, `RouteAudio`,
/// `RouteSubtitle`) are desugared via [`Command::route_intent`] into engine-native
/// [`RouteIntent`]s and applied through the **canonical engine apply primitives**
/// (RT-11 / ADR-0034):
///
/// * **video** → [`RouteApplier::apply_video`] → O(1) [`CompositorDrive::rebind_cell`]
///   (no `solve_layout`/`validate` re-solve), **batched + capped** at
///   [`MAX_REPOINTS_PER_TICK`] per tick with the excess held in a bounded backlog
///   (RT-6);
/// * **subtitle** → the run's live [`SubtitleRouteHandle`](crate::captions::SubtitleRouteHandle)
///   seam (RT-10b), when one is threaded in via [`command_drain_with_seams`];
/// * **audio** → a bounded, wait-free audio mailbox drained by the bake consumer
///   immediately before the next sample-aligned `ProgramBus` tick.
///
/// Geometry-changing commands (`ApplyLayout`) still re-solve, exactly as before.
pub struct CommandDrain {
    commands: CommandReceiver,
    config: MultiviewConfig,
    publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    state: DrainState,
    /// Pending VIDEO→cell route intents awaiting application (bounded, drop-oldest).
    pending: std::collections::VecDeque<RouteIntent>,
    /// The engine-native resolution context the [`RouteApplier`] consults to turn a
    /// video `StreamRef` into its `CompositorDrive` store key. In the run the store
    /// key **is** the source id (the `rebind_cell` argument), so a video route's
    /// store key is registered as `source.input_id` when the route is drained.
    resolution: RouteResolution,
    /// The live run-side subtitle re-point seam (RT-10b), when wired
    /// ([`command_drain_with_seams`]). A `RouteSubtitle` drives a breakaway through
    /// it; the run applies it at the next sample boundary. `None` on the
    /// software-engine path (no subtitle rendering) — a `RouteSubtitle` is then a
    /// logged held action.
    #[cfg(all(feature = "ffmpeg", feature = "overlay"))]
    subtitle_route: Option<
        Arc<multiview_framestore::SwapCell<Option<Arc<crate::captions::SubtitleRouteHandle>>>>,
    >,
    /// Non-blocking audio re-point seam, published at run startup.
    #[cfg(feature = "ffmpeg")]
    audio_route:
        Option<Arc<multiview_framestore::SwapCell<Option<Arc<crate::audio::AudioControlHandle>>>>>,
    /// The live-source producer seam (ADR-W018), when wired
    /// ([`command_drain_with_live_sources`] / [`command_drain_with_seams`]): the
    /// bounded, non-blocking handle to the off-thread
    /// [`LiveSourceHub`](crate::live_sources::LiveSourceHub) that owns producer
    /// spawn/teardown + the preview registry. `None` ⇒ `UpsertSource`/
    /// `RemoveSource` are surfaced held actions (never a silent drop).
    live_sources: Option<crate::live_sources::LiveSourceHandle>,
    /// The APPLIED producer identity per source id (review R4-01): the source
    /// document whose producer the hub actually accepted (or the startup path
    /// spawned), never merely the requested mirror in `config`. Absent means
    /// unknown. One entry per admitted source (bounded by the source cap);
    /// cleared on remove.
    applied_media: std::collections::HashMap<String, multiview_config::Source>,
    /// Sources whose latest producer submission was shed (`Full`/`Gone`): the
    /// mirror runs ahead of the producer, so no upsert may be judged
    /// policy-only until a submission lands. Bounded like `applied_media`.
    unapplied: std::collections::HashSet<String>,
    /// The live overlay working-set seam (ADR-W022), when wired
    /// ([`command_drain_with_live_overlays`] / [`command_drain_with_seams`]):
    /// the lock-free slot the bake consumer re-derives its overlay render
    /// state from. `None` ⇒ `UpsertOverlay`/`RemoveOverlay` are surfaced held
    /// actions (never a silent drop).
    live_overlays: Option<crate::live_overlays::OverlayApplySlot>,
    /// Published active tile placements for the off-clock overlay consumer.
    #[cfg(all(feature = "ffmpeg", feature = "overlay"))]
    tile_spec_apply:
        Option<Arc<multiview_framestore::SwapCell<Arc<Vec<crate::overlays::TileSpec>>>>>,
    #[cfg(all(feature = "ffmpeg", feature = "overlay"))]
    audio_free_sources: Option<crate::pipeline::AudioFreeSources>,
    /// The media-player transport seam (ADR-0057 / ADR-0097): the per-player
    /// bounded [`TransportMailbox`](crate::player::TransportMailbox) the running
    /// player ingest thread drains between frames, keyed by player id. A
    /// `MediaTransport`/`ArmMediaExit`/`TakeMediaExit`/`CancelMediaExit` command
    /// submits its verb to the addressed player's mailbox (a short mutex-guarded
    /// push into a bounded two-class queue — state verbs conflated latest-wins,
    /// targeted verbs a bounded drop-oldest FIFO; the engine never awaits the
    /// player — inv #1/#10). An empty map (no players configured) ⇒ those
    /// commands are surfaced held (warned), never a silent drop.
    player_mailboxes:
        std::collections::HashMap<String, std::sync::Arc<crate::player::TransportMailbox>>,
    /// Where the live layout is published for the decode threads to follow.
    /// `None` on runs with no decoders.
    display_layout: Option<crate::display_size::DisplayLayoutSlot>,
    /// One-shot: the drive's cell-id → index map is established the first tick.
    cell_ids_set: bool,
    /// With a switcher M/E declared (#307): the Program drive's one cell (id
    /// and failover slate), from [`multiview_config::MultiviewConfig::program_canvas`].
    /// The M/E owns that drive: a stored layout updates only the document
    /// mirror in `config`, never the drive; cell edits are dropped (#307 M1).
    switcher_program: Option<(String, multiview_config::FailoverSlate)>,
    /// One-shot: a cell edit (`SwapSource`/`RouteVideo`/a cell-recalling
    /// salvo) reaching the drain under a switcher has been warned about
    /// (#307 review M1). The control plane refuses them; the backstop drops
    /// them without touching the mirror and warns once, then at debug.
    switcher_cell_edit_warned: bool,
    /// The demand seam (docs/architecture/demand-driven-sources.md): the drain
    /// republishes the layout consumers when bindings change. Wait-free.
    #[cfg(feature = "ffmpeg")]
    demand: Option<crate::source_demand::DrainDemand<Nv12Image>>,
    /// The drive's layout generation the demand seam last published for.
    #[cfg(feature = "ffmpeg")]
    demand_generation: Option<u64>,
    /// The subtitle routes applied so far (layer → source), for demand.
    #[cfg(all(feature = "ffmpeg", feature = "overlay"))]
    subtitle_routes: std::collections::BTreeMap<String, String>,
    /// Test-only spy counting how many times this drain calls `solve_layout`.
    #[cfg(test)]
    resolve_spy: Option<Arc<std::sync::atomic::AtomicUsize>>,
}

impl CommandDrain {
    /// Build a drain over `commands` for the working `config`, publishing outcomes
    /// through `publisher`.
    #[must_use]
    pub fn new(
        commands: CommandReceiver,
        config: MultiviewConfig,
        publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    ) -> Self {
        // The startup path spawned every declared source's producer.
        let applied_media = config
            .sources
            .iter()
            .map(|source| (source.id.clone(), source.clone()))
            .collect();
        let switcher_program = switcher_program_cell(&config);
        Self {
            commands,
            config,
            switcher_program,
            switcher_cell_edit_warned: false,
            publisher,
            state: DrainState::default(),
            pending: std::collections::VecDeque::new(),
            resolution: RouteResolution::default(),
            #[cfg(all(feature = "ffmpeg", feature = "overlay"))]
            subtitle_route: None,
            #[cfg(feature = "ffmpeg")]
            audio_route: None,
            live_sources: None,
            applied_media,
            unapplied: std::collections::HashSet::new(),
            live_overlays: None,
            #[cfg(all(feature = "ffmpeg", feature = "overlay"))]
            tile_spec_apply: None,
            #[cfg(all(feature = "ffmpeg", feature = "overlay"))]
            audio_free_sources: None,
            player_mailboxes: std::collections::HashMap::new(),
            display_layout: None,
            cell_ids_set: false,
            #[cfg(feature = "ffmpeg")]
            demand: None,
            #[cfg(feature = "ffmpeg")]
            demand_generation: None,
            #[cfg(all(feature = "ffmpeg", feature = "overlay"))]
            subtitle_routes: std::collections::BTreeMap::new(),
            #[cfg(test)]
            resolve_spy: None,
        }
    }

    /// Thread in the media-player transport seam (ADR-0057 / ADR-0097): the map
    /// of player-id → [`TransportMailbox`](crate::player::TransportMailbox) the
    /// boot-spawned player ingest threads drain, so a `MediaTransport` / exit
    /// command reaches the addressed running player. See
    /// [`command_drain_with_seams`].
    #[must_use]
    fn with_player_mailboxes(
        mut self,
        mailboxes: std::collections::HashMap<
            String,
            std::sync::Arc<crate::player::TransportMailbox>,
        >,
    ) -> Self {
        self.player_mailboxes = mailboxes;
        self
    }

    /// Publish every applied layout change to `slot` for the decode threads.
    #[cfg(any(feature = "ffmpeg", test))]
    #[must_use]
    fn with_display_layout(mut self, slot: crate::display_size::DisplayLayoutSlot) -> Self {
        self.display_layout = Some(slot);
        self
    }

    /// Thread in the live-source producer seam (ADR-W018) so
    /// `UpsertSource`/`RemoveSource` reach the running engine. See
    /// [`command_drain_with_live_sources`].
    #[must_use]
    fn with_live_sources(mut self, live: crate::live_sources::LiveSourceHandle) -> Self {
        self.live_sources = Some(live);
        self
    }

    /// Thread in the live overlay working-set seam (ADR-W022) so
    /// `UpsertOverlay`/`RemoveOverlay` reach the running bake. See
    /// [`command_drain_with_live_overlays`].
    #[must_use]
    fn with_live_overlays(mut self, overlays: crate::live_overlays::OverlayApplySlot) -> Self {
        self.live_overlays = Some(overlays);
        self
    }

    /// Thread in the active tile placement slot for the off-clock baker.
    #[cfg(all(feature = "ffmpeg", feature = "overlay"))]
    #[must_use]
    fn with_tile_spec_apply(
        mut self,
        tiles: Arc<multiview_framestore::SwapCell<Arc<Vec<crate::overlays::TileSpec>>>>,
    ) -> Self {
        self.tile_spec_apply = Some(tiles);
        self
    }

    #[cfg(all(feature = "ffmpeg", feature = "overlay"))]
    #[must_use]
    fn with_audio_free_sources(mut self, sources: crate::pipeline::AudioFreeSources) -> Self {
        self.audio_free_sources = Some(sources);
        self
    }

    /// Thread in the live run-side subtitle re-point seam (RT-10b) so a
    /// `RouteSubtitle` reaches the running pipeline's layer. See
    /// [`command_drain_with_seams`].
    #[cfg(all(feature = "ffmpeg", feature = "overlay"))]
    #[must_use]
    fn with_subtitle_route(
        mut self,
        subtitle_route: Arc<
            multiview_framestore::SwapCell<Option<Arc<crate::captions::SubtitleRouteHandle>>>,
        >,
    ) -> Self {
        self.subtitle_route = Some(subtitle_route);
        self
    }

    /// Thread the run-owned bus's resolved, non-blocking audio command slot in.
    #[cfg(feature = "ffmpeg")]
    #[must_use]
    fn with_audio_route(
        mut self,
        audio_route: Arc<
            multiview_framestore::SwapCell<Option<Arc<crate::audio::AudioControlHandle>>>,
        >,
    ) -> Self {
        self.audio_route = Some(audio_route);
        self
    }

    /// Thread in the demand seam so binding changes take and release source
    /// leases (new before old).
    #[cfg(feature = "ffmpeg")]
    #[must_use]
    fn with_demand(mut self, demand: Option<crate::source_demand::DrainDemand<Nv12Image>>) -> Self {
        self.demand = demand;
        self
    }

    /// Publish the active layout's consumers when its bindings changed. One
    /// integer compare per tick; on change a bounded vector build and a wait-free
    /// publish, never a rail start (the demand worker does that off-clock).
    #[cfg(feature = "ffmpeg")]
    fn publish_demand(&mut self, drive: &CompositorDrive<Nv12Image>) -> bool {
        let Some(demand) = &self.demand else {
            return false;
        };
        let generation = drive.layout_generation();
        if self.demand_generation == Some(generation) {
            return false;
        }
        self.demand_generation = Some(generation);
        demand
            .layout
            .publish(crate::source_demand::layout_items_for(drive.layout()));
        true
    }

    /// Make sure a catalogued source has a store in the drive before a cell
    /// binds it. A catalogued source's store exists even while it is idle.
    #[cfg(feature = "ffmpeg")]
    fn ensure_store(&self, id: &str, drive: &mut CompositorDrive<Nv12Image>) {
        if drive.store(id).is_some() {
            return;
        }
        if let Some(demand) = &self.demand {
            if let Some(entry) = demand.catalogue.load().get(id) {
                drive.insert_store(id.to_owned(), Arc::clone(entry.store()));
            }
        }
    }

    /// Attach a test spy that counts every `solve_layout` re-solve the drain does.
    #[cfg(test)]
    #[must_use]
    fn with_resolve_spy(mut self, spy: &Arc<std::sync::atomic::AtomicUsize>) -> Self {
        self.resolve_spy = Some(Arc::clone(spy));
        self
    }

    /// Apply one frame-boundary pass: drain the (non-blocking) bus, classify each
    /// command, batch + cap the VIDEO→cell re-points, and apply them to the
    /// running `drive`. Returns the number of re-points applied **this tick**
    /// (bounded by [`MAX_REPOINTS_PER_TICK`]).
    ///
    /// Never awaits, and is never back-pressured by a slow consumer — it drains a
    /// non-blocking queue (O(pending)), applies O(1) re-points, publishes
    /// drop-oldest events, and (for a media-transport command) takes a short
    /// uncontended `std::sync::Mutex` to push into a player's bounded mailbox (the
    /// player's drain holds that lock only for a `mem::take`, never across a
    /// decode/publish). So the output clock is never stalled by control
    /// (invariants #1 + #10).
    pub fn apply(&mut self, drive: &mut CompositorDrive<Nv12Image>) -> usize {
        // First tick: hand the drive the cell ids (in config-cell order, which is
        // exactly `solve_layout`'s core-cell order) so `rebind_cell` can address
        // cells by id, and the per-cell `on_loss` failover-slate policy (ADR-0027
        // / ADR-0030) so a down tile composites the slate its config declares.
        // One-shot, off the hot composite.
        if !self.cell_ids_set {
            if let Some((id, slate)) = &self.switcher_program {
                // #307: the drive is the M/E's one-cell Program bus, never the
                // document's cells.
                drive.set_cell_ids(vec![Some(id.clone())]);
                drive.set_cell_slates(vec![*slate]);
                self.cell_ids_set = true;
            }
        }
        if !self.cell_ids_set {
            let ids: Vec<Option<String>> = self
                .config
                .cells
                .iter()
                .map(|c| Some(c.id.clone()))
                .collect();
            drive.set_cell_ids(ids);
            drive.set_cell_slates(self.config.cells.iter().map(|c| c.on_loss).collect());
            self.cell_ids_set = true;
        }

        // Drain the bus, routing commands. Video re-points are enqueued (batched +
        // bounded); every other command is applied immediately as before.
        #[cfg(all(feature = "ffmpeg", feature = "overlay"))]
        let mut tile_binding_changed = false;
        let mut layout_applied = false;
        for command in self.commands.try_drain() {
            layout_applied |= matches!(&command, Command::ApplyLayout { .. });
            #[cfg(all(feature = "ffmpeg", feature = "overlay"))]
            {
                tile_binding_changed |= matches!(
                    &command,
                    Command::ApplyLayout { .. }
                        | Command::UpsertSource { .. }
                        | Command::RemoveSource { .. }
                );
            }
            self.route_command(command, drive);
        }

        // Take at most the per-tick cap of pending VIDEO route intents off the
        // bounded backlog (the rest stay for the next tick — the RT-6 cap-per-tick
        // budget). Each is applied through the canonical, engine-tested
        // `RouteApplier::apply_video` → O(1) `rebind_cell` (no `solve_layout`/
        // `validate` re-solve). Each intent is applied as its own one-element batch
        // so an honest route error on one cell (unknown cell / source with no store)
        // is held + logged WITHOUT aborting the others' valid re-points — the
        // per-cell hold the old `apply_repoint` path gave. Returns the number of
        // intents taken off the backlog this tick.
        let mut applied = 0_usize;
        while applied < MAX_REPOINTS_PER_TICK {
            let Some(intent) = self.pending.pop_front() else {
                break;
            };
            #[cfg(feature = "ffmpeg")]
            if let RouteIntent::Video { source, .. } = &intent {
                self.ensure_store(&source.input_id, drive);
            }
            let mut route_applier = RouteApplier::new(&self.resolution);
            if let Err(e) = route_applier.apply_video(drive, std::slice::from_ref(&intent)) {
                // An honest route error (unknown cell / source with no store): the
                // binding is held unchanged, logged, never a panic, never a re-solve.
                tracing::warn!(error = %e, "video route held (unknown cell/source)");
            }
            applied = applied.saturating_add(1);
        }
        #[cfg(all(feature = "ffmpeg", feature = "overlay"))]
        if tile_binding_changed || applied != 0 {
            self.publish_tile_specs(drive);
        }
        // The demand seam republishes the layout consumer set when the drive's
        // layout generation moves; the NVDEC relayout hook follows the same
        // republish so a newly consumed or re-sized source re-picks its decode
        // size (display_size.rs) on the same frame boundary.
        #[cfg(feature = "ffmpeg")]
        let consumers_republished = self.publish_demand(drive);
        #[cfg(not(feature = "ffmpeg"))]
        let consumers_republished = false;
        if layout_applied || applied != 0 || consumers_republished {
            self.publish_display_layout(drive);
        }
        applied
    }

    /// Hand the active layout to the decode threads. Only runs after a layout
    /// or route command: O(cells) plus one wait-free store, and the decoders do
    /// all the resulting work on their own threads (inv #1/#10).
    fn publish_display_layout(&self, drive: &CompositorDrive<Nv12Image>) {
        if let Some(slot) = &self.display_layout {
            if *slot.load() != *drive.layout() {
                slot.store(Arc::new(drive.layout().clone()));
            }
        }
    }

    /// Publish only when the active placements differ from the baker's snapshot.
    /// This happens on the frame boundary after layout or video-route changes;
    /// steady-state ticks do not allocate or wait on the bake consumer.
    #[cfg(all(feature = "ffmpeg", feature = "overlay"))]
    fn publish_tile_specs(&self, drive: &CompositorDrive<Nv12Image>) {
        if let Some(slot) = &self.tile_spec_apply {
            let labels = self
                .config
                .sources
                .iter()
                .map(|source| {
                    (
                        source.id.clone(),
                        source
                            .display_name
                            .clone()
                            .unwrap_or_else(|| source.id.clone()),
                    )
                })
                .collect();
            let tiles = crate::pipeline::tile_specs_for_layout(drive.layout(), &labels);
            if *slot.load().as_ref() != tiles {
                slot.store(Arc::new(tiles));
            }
        }
    }

    /// Enqueue a VIDEO→cell route intent, bounded drop-oldest (safety rule §5).
    ///
    /// Registers the intent's source store key in the [`RouteResolution`] (the run
    /// store key **is** the source id), mirrors the binding into the working config
    /// (so `ApplyLayout`/export reflect it), and pushes the intent onto the bounded
    /// backlog the [`RouteApplier`] drains at the cap each tick.
    fn enqueue_video_intent(&mut self, cell: &str, source: &multiview_config::routing::StreamRef) {
        // Register the source's store key so the applier can resolve the StreamRef.
        // In the run the `CompositorDrive` store key is the source id, which is the
        // StreamRef's `input_id`.
        self.resolution
            .set_video_store_key(source, source.input_id.clone());
        // Mirror into the working config (so `ApplyLayout`/export reflect it); an
        // unknown cell id is ignored there, exactly as before.
        let _ = apply_swap_source(&mut self.config, cell, &source.input_id);
        if self.pending.len() >= MAX_REPOINT_BACKLOG {
            // Shed the oldest pending re-point: the newest binding wins, so an old
            // superseded one being dropped never mis-routes.
            let _ = self.pending.pop_front();
        }
        self.pending.push_back(RouteIntent::Video {
            cell: cell.to_owned(),
            source: source.clone(),
        });
    }

    /// Apply a STORED layout that was resolved + solved **at the route**
    /// (ADR-W019): swap the active layout at this frame boundary, re-establish
    /// the O(1) re-point address space (cell ids) and per-cell failover slates
    /// from the stored document, and mirror the document into the working
    /// config so export / salvo recalls / the back-compat `ApplyLayout`
    /// fallback address the **active** layout.
    ///
    /// O(cells), no I/O, no `solve_layout`, no `.await` — the render thread
    /// only swaps (invariants #1/#10). Held (warned, surfaced as a
    /// `job.progress` `apply_layout_held` outcome, never adopted, never a
    /// panic) when:
    /// * the stored canvas (geometry/cadence) differs from the running
    ///   session's pinned canvas — a Class-2 change (ADR-R004). The route
    ///   refuses this with `422`; this is the authoritative backstop;
    /// * the compositor rejects the layout (`set_layout` re-validates and
    ///   retains the last-good layout on error).
    ///
    /// A cell bound to a source with **no registered store** (no running
    /// ingest) stays bound and composites its per-cell `on_loss` slate until
    /// the source appears — the output never stalls and never panics
    /// (consistent with how an unbound/down tile already composes).
    ///
    /// On success the apply is observable: a `job.progress` outcome event
    /// (phase `apply_layout`, drop-oldest — inv #10) plus a `tracing::info!`;
    /// the proof is the next composited frame.
    fn apply_stored_layout(
        &mut self,
        id: &str,
        resolved: ResolvedLayout,
        drive: &mut CompositorDrive<Nv12Image>,
    ) {
        let current = &drive.layout().canvas;
        let stored = &resolved.solved.canvas;
        // Same SIGNAL, by value: geometry equal and cadence cross-multiplied
        // (`Canvas::same_signal`), so a non-reduced 50/2 against a running 25/1
        // is never a false Class-2 hold (ADR-W019 MINOR-3).
        if !current.same_signal(stored) {
            tracing::warn!(
                layout = %id,
                running = ?current,
                stored = ?stored,
                "apply_layout held: the stored layout's canvas differs from the running \
                 session's pinned canvas (Class-2, ADR-R004); not applied live"
            );
            self.publish_apply_held(
                id,
                "the stored canvas differs from the running session's pinned canvas (Class-2)",
            );
            return;
        }
        // #307: with a switcher M/E the Program drive is the M/E bus. A stored
        // layout (the route refuses apply-layout first; the config watcher
        // still forwards a boot-file layout edit) changes only the document
        // mirror, the stored layout routable to outputs, never the drive, so it
        // can neither replace the bus nor reset the M/E's Program binding.
        if self.switcher_program.is_some() {
            self.config.layout = resolved.document.layout;
            self.config.cells = resolved.document.cells;
            tracing::info!(
                layout = %id,
                "apply_layout: switcher declared; the document layout was updated, \
                 Program stays the M/E bus"
            );
            self.publish_apply_held(
                id,
                "The switcher is on: the Program canvas is the M/E bus; the layout was stored, Program is unchanged",
            );
            return;
        }
        let cells = resolved.solved.cells.len();
        #[cfg(feature = "ffmpeg")]
        for id in resolved
            .solved
            .cells
            .iter()
            .filter_map(|cell| cell.source.clone())
            .collect::<Vec<_>>()
        {
            self.ensure_store(&id, drive);
        }
        match drive.set_layout(Arc::new(resolved.solved)) {
            Ok(()) => {
                drive.set_cell_ids(resolved.document.cell_ids());
                drive.set_cell_slates(resolved.document.cell_slates());
                // Mirror the document into the working config so the other
                // management surfaces follow the ACTIVE layout, not the boot one.
                self.config.layout = resolved.document.layout;
                self.config.cells = resolved.document.cells;
                tracing::info!(
                    layout = %id,
                    cells,
                    "apply_layout: stored layout applied live at the frame boundary"
                );
                self.publisher
                    .publish_event(Event::JobProgress(JobProgress {
                        phase: "apply_layout".to_owned(),
                        pct: 100,
                        message: Some(format!("layout {id} applied live at the frame boundary")),
                    }));
            }
            Err(e) => {
                // Unreachable for a route-solved document (the route validated the
                // same pure invariants), but held honestly rather than panicking.
                tracing::warn!(
                    layout = %id,
                    error = %e,
                    "apply_layout: compositor rejected the stored layout; last-good retained"
                );
                self.publish_apply_held(id, &format!("the compositor rejected the layout: {e}"));
            }
        }
    }

    /// Make a HELD stored-layout apply observable on the realtime stream
    /// (ADR-W019 MINOR-2): the 202 promised a swap, so a drain-side hold (the
    /// pinned-canvas backstop or a compositor rejection) emits a `job.progress`
    /// outcome with the held phase and the reason — drop-oldest, never awaits a
    /// client (inv #10) — alongside the `tracing::warn!`.
    fn publish_apply_held(&self, id: &str, reason: &str) {
        self.publisher
            .publish_event(Event::JobProgress(JobProgress {
                phase: "apply_layout_held".to_owned(),
                pct: 0,
                message: Some(format!("layout {id} not applied: {reason}")),
            }));
    }

    /// The working config's solved layout name (`schema_v{N}`), the one
    /// back-compat `ApplyLayout` id. A full solve: counted on the test spy.
    fn working_layout_name(&self) -> Option<String> {
        #[cfg(test)]
        if let Some(spy) = &self.resolve_spy {
            spy.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        }
        self.config.solve_layout().ok().map(|l| l.name)
    }

    /// Re-solve the working config and hot-swap it onto `drive` (the geometry-
    /// changing path: `ApplyLayout`). Counts the re-solve on the test spy.
    fn resolve_and_apply(&self, drive: &mut CompositorDrive<Nv12Image>) -> bool {
        #[cfg(test)]
        if let Some(spy) = &self.resolve_spy {
            spy.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        }
        resolve_and_apply(&self.config, drive)
    }
}

/// Per-tick command-drain state retained across ticks.
#[derive(Debug, Default)]
struct DrainState {
    /// Whether program output is "running" (flipped by Start/Stop). Observed via
    /// the emitted `OutputStatus` events; retained so a future periodic-status
    /// republish can read it without re-deriving.
    running: bool,
    /// The id of the currently-armed salvo awaiting a take, if any.
    armed_salvo: Option<String>,
}

/// Apply one drained command to the working config + active layout and emit its
/// outcome event. Panic-free (no `unwrap`/`expect`/indexing); an unknown
/// layout/salvo logs `tracing::warn!` and emits nothing (or a tally echo).
impl CommandDrain {
    // A flat one-arm-per-`Command`-variant dispatch (the canonical single apply
    // path): inherently long and clearer kept whole than split — each arm is a
    // thin delegate to a focused handler. The media-player arms tipped it over.
    #[allow(clippy::too_many_lines)]
    fn route_command(&mut self, command: Command, drive: &mut CompositorDrive<Nv12Image>) {
        match command {
            Command::Start { .. } => {
                self.state.running = true;
                publish_output_status(&self.publisher, OutputRunState::Running);
            }
            Command::Stop { .. } => {
                self.state.running = false;
                publish_output_status(&self.publisher, OutputRunState::Idle);
            }
            Command::SwapSource { .. } | Command::RouteVideo { .. } => {
                self.route_video_command(&command);
            }
            Command::RouteAudio {
                ref target,
                ref source,
                ..
            } => self.route_audio(target, source),
            Command::RouteSubtitle {
                ref layer,
                ref source,
                ..
            } => {
                self.route_subtitle(layer, source);
            }
            Command::ApplyLayout {
                layout, document, ..
            } => {
                if let Some(resolved) = document {
                    // ADR-W019: a STORED layout, resolved + solved at the route
                    // (off this render thread). The frame-boundary work is the
                    // swap: O(cells), no I/O, no re-solve.
                    self.apply_stored_layout(&layout, *resolved, drive);
                } else {
                    // Back-compat (no document): the working config carries a
                    // single solved layout named `schema_v{N}`. Applying that
                    // name re-solves + re-applies the working layout; any other
                    // id is a failure (no panic). A layout change CAN alter
                    // geometry, so this keeps the re-solve path (counted by the
                    // test spy).
                    if self.switcher_program.is_some() {
                        // #307: the M/E owns the Program drive; a re-solve of
                        // the document must never land on it. Checked BEFORE
                        // any solve: no clock-side work for an ignored command
                        // (review I1).
                        tracing::warn!(
                            layout = %layout,
                            "apply_layout: switcher declared; Program stays the M/E bus"
                        );
                    } else if self.working_layout_name().as_deref() == Some(layout.as_str()) {
                        let _ = self.resolve_and_apply(drive);
                    } else {
                        tracing::warn!(
                            layout = %layout,
                            "apply_layout: unknown layout id (no stored document on the command); ignored"
                        );
                    }
                }
            }
            Command::ArmSalvo { salvo, head, .. } => {
                if self.config.salvos.iter().any(|s| s.id == salvo) {
                    // Stage the salvo: its source recalls are read from `config` at
                    // take time, so staging is just remembering the id.
                    self.state.armed_salvo = Some(salvo.clone());
                    self.publisher.publish_event(Event::SalvoArmed(salvo_event(
                        salvo,
                        SalvoPhase::Armed,
                        head,
                    )));
                } else {
                    tracing::warn!(salvo = %salvo, "arm_salvo: no such salvo; ignored");
                }
            }
            Command::TakeSalvo { salvo, head, .. } => {
                self.take_salvo(salvo, head);
            }
            Command::CancelSalvo { salvo, head, .. } => {
                // Cancel the named salvo, else the currently-armed one.
                let target = salvo.or_else(|| self.state.armed_salvo.clone());
                self.state.armed_salvo = None;
                let Some(target) = target else {
                    tracing::warn!("cancel_salvo: no salvo named and none armed; ignored");
                    return;
                };
                self.publisher
                    .publish_event(Event::SalvoCancelled(salvo_event(
                        target,
                        SalvoPhase::Cancelled,
                        head,
                    )));
            }
            Command::UpsertSource { ref source, .. } => {
                self.upsert_source(source, drive);
            }
            Command::RemoveSource { ref id, .. } => {
                self.remove_source(id, drive);
            }
            Command::UpsertOverlay { ref overlay, .. } => {
                self.upsert_overlay(overlay);
            }
            Command::RemoveOverlay { ref id, .. } => {
                self.remove_overlay(id);
            }
            Command::ReorderOverlays { ref order, .. } => {
                self.reorder_overlays(order);
            }
            Command::SetTallyOverride { target, color, .. } => {
                self.set_tally_override(target, color);
            }
            // Media-player transport (ADR-0057 / ADR-0097): submit the verb to the
            // addressed player's bounded mailbox (a short mutex-guarded push; the
            // player thread drains it between frames — the engine never awaits it).
            Command::MediaTransport {
                ref player,
                ref verb,
                ..
            } => {
                self.submit_player_verb(player, map_transport_verb(verb));
            }
            Command::ArmMediaExit { ref player, .. } => {
                self.submit_player_verb(player, crate::player::TransportVerb::ArmExit);
            }
            Command::TakeMediaExit { ref player, .. } => {
                self.submit_player_verb(player, crate::player::TransportVerb::TakeExit);
            }
            Command::CancelMediaExit { ref player, .. } => {
                self.submit_player_verb(player, crate::player::TransportVerb::CancelExit);
            }
            // `Command` is `#[non_exhaustive]`: a future variant this build does not
            // know about is logged and skipped, never panicked on.
            ref other => {
                tracing::warn!(kind = other.kind(), "unhandled control command; skipped");
            }
        }
    }

    /// Submit a transport verb to the addressed player's bounded mailbox (a short
    /// mutex-guarded push into the two-class queue) the player ingest thread
    /// drains between frames — the engine never awaits the player (inv #1/#10).
    /// An unknown player id (none configured / a typo) is surfaced held (warned),
    /// never a silent drop.
    fn submit_player_verb(&self, player: &str, verb: crate::player::TransportVerb) {
        let Some(mailbox) = self.player_mailboxes.get(player) else {
            tracing::warn!(
                player,
                ?verb,
                "media transport held: no such player channel (none configured / unknown id)"
            );
            return;
        };
        mailbox.submit(verb);
    }

    /// Apply a `SetTallyOverride`. No tally arbiter is wired into the software
    /// engine yet, so this emits a `TallyState` echo rather than silently
    /// no-op'ing: a forced colour maps to a program-bus lamp of that colour at
    /// the default brightness; a cleared override (`None`) maps to the unlit
    /// default. FOLLOW-UP: route through the real arbiter once it exists.
    fn set_tally_override(
        &self,
        target: multiview_events::TallyTarget,
        color: Option<multiview_core::tally::TallyColor>,
    ) {
        let tally_state = match color {
            Some(color) => multiview_core::tally::TallyState {
                color,
                ..multiview_core::tally::TallyState::default()
            },
            None => multiview_core::tally::TallyState::default(),
        };
        self.publisher.publish_event(Event::TallyState(TallyEvent {
            target,
            state: tally_state,
        }));
    }
}

impl CommandDrain {
    /// Apply a `SwapSource`/`RouteVideo` command: desugar it to the engine-native
    /// [`RouteIntent::Video`] (`SwapSource` is the `RouteVideo{…,Video,Best}` alias
    /// — back-compat) and enqueue it for the canonical [`RouteApplier::apply_video`]
    /// → O(1) [`CompositorDrive::rebind_cell`] path (batched + capped per tick),
    /// NOT a full layout re-solve. An unknown cell id is ignored (no enqueue) with a
    /// warn, exactly as before; the binding only takes effect if the cell exists.
    fn route_video_command(&mut self, command: &Command) {
        if self.drop_switcher_cell_edit("route_video") {
            return;
        }
        match command.route_intent() {
            Some(RouteIntent::Video { cell, source }) => {
                if self.config.cells.iter().any(|c| c.id == cell) {
                    self.enqueue_video_intent(&cell, &source);
                } else {
                    tracing::warn!(cell = %cell, "route_video: no such cell; ignored");
                }
            }
            // `route_intent()` returns `Video` for these variants; any other shape
            // is impossible, but is held (never panicked on) for forward-compat with
            // `#[non_exhaustive]` `RouteIntent`.
            other => {
                tracing::warn!(?other, "route_video: unexpected desugar; held");
            }
        }
    }

    /// #307 review M1 backstop: with a switcher M/E declared, a cell edit
    /// (`SwapSource`, `RouteVideo`, a salvo recalling a layout or cell
    /// sources) changes neither Program (the M/E bus) nor the stored layout
    /// outputs show, and the control plane refuses it (422
    /// `switcher-owns-program`). One that still reaches the drain is DROPPED:
    /// the working-config mirror is never mutated, so it never diverges from
    /// the accepted state. Warns once per drain, then logs at debug. Returns
    /// whether the command was dropped.
    fn drop_switcher_cell_edit(&mut self, what: &'static str) -> bool {
        if self.switcher_program.is_none() {
            return false;
        }
        if self.switcher_cell_edit_warned {
            tracing::debug!(command = what, "switcher declared; cell edit dropped");
        } else {
            self.switcher_cell_edit_warned = true;
            tracing::warn!(
                command = what,
                "switcher declared: Program is the M/E bus; a cell edit reached the \
                 drain and was dropped (edit the stored layout or use a switcher select); \
                 further drops log at debug"
            );
        }
        true
    }

    /// Take the named salvo (else the currently-armed one): enqueue every source
    /// recall as a VIDEO route intent — all the re-points of a salvo ride the same
    /// bounded, capped pass and are applied via the canonical
    /// [`RouteApplier::apply_video`] → O(1) [`CompositorDrive::rebind_cell`], NOT one
    /// re-solve per recall (a recall is the `SwapSource` desugar
    /// `{input_id, Video, Best}`). Emits [`Event::SalvoTaken`]; an unknown / unarmed
    /// salvo logs `tracing::warn!` and emits nothing, never a panic.
    fn take_salvo(&mut self, salvo: Option<String>, head: Option<String>) {
        let Some(target) = salvo.or_else(|| self.state.armed_salvo.clone()) else {
            tracing::warn!("take_salvo: no salvo named and none armed; ignored");
            return;
        };
        // Clone the matched salvo's recalls out so the immutable borrow of `config`
        // ends before the mutations below.
        let Some(recalled) = self.config.salvos.iter().find(|s| s.id == target).cloned() else {
            tracing::warn!(salvo = %target, "take_salvo: no such salvo; ignored");
            return;
        };
        if (recalled.layout.is_some() || !recalled.sources.is_empty())
            && self.drop_switcher_cell_edit("take_salvo")
        {
            return;
        }
        for recall in &recalled.sources {
            if self.config.cells.iter().any(|c| c.id == recall.cell) {
                let cell = recall.cell.clone();
                let source = multiview_config::routing::StreamRef::best(
                    recall.input_id.clone(),
                    multiview_core::stream::StreamKind::Video,
                );
                self.enqueue_video_intent(&cell, &source);
            }
        }
        self.state.armed_salvo = None;
        self.publisher.publish_event(Event::SalvoTaken(salvo_event(
            target,
            SalvoPhase::Taken,
            head,
        )));
    }

    /// Apply an `UpsertSource` (ADR-W018 live add/edit) at the frame boundary.
    ///
    /// Only the **cheap binding mutations** happen here on the output-clock
    /// loop: create *or reuse* the source's `TileStore` (reuse on an edit — the
    /// bound tile holds last-good through the producer swap, never a slate
    /// flash), register it with the drive (`insert_store`), register the route
    /// key so a follow-up `RouteVideo`/`SwapSource` resolves, and mirror the
    /// source into the working config (so `ApplyLayout` re-solves and export
    /// stay coherent). The **heavy** half — spawning the producer thread and
    /// the preview-registry RCU — is handed to the off-thread
    /// [`LiveSourceHub`](crate::live_sources::LiveSourceHub) over a bounded,
    /// non-blocking channel (invariants #1 + #10; a full queue is shed with a
    /// warning and the tile rides the slate — re-applying retries).
    ///
    /// Kinds: this slice ships **synthetic** sources (`bars`/`solid`/`clock`,
    /// ADR-0027) live; a decoded kind is a surfaced held action (the stored
    /// document applies on restart — exactly what the route's
    /// `X-Multiview-Apply: restart` told the client). The route only enqueues
    /// `UpsertSource` for synthetic kinds, so the held arm is defence in depth.
    fn upsert_source(
        &mut self,
        source: &multiview_config::Source,
        drive: &mut CompositorDrive<Nv12Image>,
    ) {
        let Some(seam) = self.live_sources.clone() else {
            tracing::warn!(
                source = %source.id,
                "upsert_source held: no live-source hub wired on this run path \
                 (the stored document applies on restart)"
            );
            return;
        };
        let synthetic = crate::synth::SyntheticKind::from_source_kind(&source.kind);
        if synthetic.is_none() && !live_decoded_kind(&source.kind) {
            tracing::warn!(
                source = %source.id,
                "upsert_source held: this kind is not live-appliable \
                 (ndi/aes67, or youtube without the feature, apply on restart)"
            );
            return;
        }
        if self.policy_only_upsert(source, drive) {
            return;
        }
        let id = source.id.clone();
        // Reuse the registered store on an edit-by-id so the tile holds
        // last-good while the hub swaps the producer behind it.
        let store = drive.store(&id).map_or_else(
            || {
                Arc::new(multiview_framestore::TileStore::new(
                    id.clone(),
                    multiview_framestore::TileThresholds::default(),
                    multiview_framestore::NoSignalPolicy::HoldForever,
                ))
            },
            Arc::clone,
        );
        // Heavy half off-thread FIRST: the hub tears down any previous
        // producer under this id (and its `{id}/` companions) before spawning
        // the replacement — the SAME generator_loop (synthetic) or the SAME
        // supervised ingest_loop (network/file) the startup path runs — and
        // RCUs the preview map. Requesting this BEFORE the binding mutations
        // below gives the hub a head start on stopping a replaced producer,
        // shrinking the bounded window in which old and new frames can
        // interleave in the reused store on an edit (ADR-W018 §5).
        // Demand-managed runs catalogue every kind and start it only when consumed.
        #[cfg(feature = "ffmpeg")]
        let synthetic = synthetic.filter(|_| self.demand.is_none());
        let submitted = match synthetic {
            Some(kind) => seam.request_spawn_synth(crate::live_sources::SynthSpawn {
                id: id.clone(),
                kind,
                store: Arc::clone(&store),
                width: self.config.canvas.width,
                height: self.config.canvas.height,
                canvas: multiview_compositor::pipeline::CanvasColor::default(),
                cadence: self.config.canvas.fps.rational(),
            }),
            None => seam.request_spawn_source(crate::live_sources::SourceSpawn {
                source: source.clone(),
                store: Arc::clone(&store),
            }),
        };
        // Only an ACCEPTED submission changes which media the producer runs
        // (review R4-01); a shed one leaves the mirror ahead of the producer.
        if submitted == crate::live_sources::HubSubmit::Accepted {
            self.applied_media.insert(id.clone(), source.clone());
            self.unapplied.remove(&id);
        } else {
            self.unapplied.insert(id.clone());
        }
        match submitted {
            crate::live_sources::HubSubmit::Accepted => {}
            crate::live_sources::HubSubmit::Full => {
                tracing::warn!(
                    source = %id,
                    "live-source hub queue full; producer spawn shed — the tile \
                     rides the slate (re-apply the source to retry)"
                );
            }
            crate::live_sources::HubSubmit::Gone => {
                tracing::warn!(
                    source = %id,
                    "live-source hub gone — live producer apply is disabled until \
                     restart; the tile rides the slate"
                );
            }
        }
        drive.insert_store(id.clone(), store);
        // Register the route key so a follow-up RouteVideo/SwapSource resolves:
        // in the run, the CompositorDrive store key IS the source id.
        let stream = multiview_config::routing::StreamRef::best(
            id.clone(),
            multiview_core::stream::StreamKind::Video,
        );
        self.resolution.set_video_store_key(&stream, id.clone());
        // Mirror into the working config (replace-or-append) so ApplyLayout's
        // re-solve treats the live source as declared.
        match self.config.sources.iter_mut().find(|s| s.id == id) {
            Some(slot) => *slot = source.clone(),
            None => self.config.sources.push(source.clone()),
        }
        #[cfg(all(feature = "ffmpeg", feature = "overlay"))]
        if let Some(slot) = &self.audio_free_sources {
            let mut audio_free = (*slot.load()).clone();
            if matches!(
                source.kind,
                multiview_config::SourceKind::Clock { .. }
                    | multiview_config::SourceKind::Timer { .. }
                    | multiview_config::SourceKind::Weather { .. }
                    | multiview_config::SourceKind::Graphic { .. }
                    | multiview_config::SourceKind::GraphicsChannel { .. }
                    | multiview_config::SourceKind::Solid { .. }
            ) {
                audio_free.insert(id);
            } else {
                audio_free.remove(&id);
            }
            slot.store(Arc::new(audio_free));
        }
    }

    /// An upsert that changes only presentation or standing policy
    /// ([`multiview_config::Source::same_media`]) of a running source, on a run
    /// with no demand manager: there are no leases to re-read, so the running
    /// producer is left alone and only the working-config mirror follows
    /// (review R3-03). Returns `true` when it handled the upsert.
    ///
    /// The judgment is made against the APPLIED producer identity, never the
    /// requested mirror (review R4-01), and the producer is respawned when:
    /// * the source's latest submission was shed (`Full`/`Gone`) — its
    ///   recovery upsert must reach the producer;
    /// * the applied identity is unknown or differs in media;
    /// * the upsert is an explicit **retry**: byte-identical to the requested
    ///   config the drain already holds, i.e. nothing new was requested, so
    ///   the only reason to send it is to re-apply (the documented retry).
    ///
    /// A demand-managed run returns `false`: its hub re-reads the pins and
    /// keeps the producer (`update_policy`).
    fn policy_only_upsert(
        &mut self,
        source: &multiview_config::Source,
        drive: &CompositorDrive<Nv12Image>,
    ) -> bool {
        #[cfg(feature = "ffmpeg")]
        if self.demand.is_some() {
            return false;
        }
        if drive.store(&source.id).is_none() || self.unapplied.contains(&source.id) {
            return false;
        }
        if !self
            .applied_media
            .get(&source.id)
            .is_some_and(|applied| applied.same_media(source))
        {
            return false;
        }
        match self.config.sources.iter_mut().find(|s| s.id == source.id) {
            Some(slot) if *slot == *source => return false,
            Some(slot) => slot.clone_from(source),
            None => self.config.sources.push(source.clone()),
        }
        tracing::debug!(
            source = %source.id,
            "upsert_source: policy/presentation-only edit; the producer keeps running"
        );
        true
    }

    /// Apply a `RemoveSource` (ADR-W018 live remove) at the frame boundary:
    /// unregister the frame store (cells bound to the id composite their
    /// `on_loss` failover slate from the next tick — the honest `NoSignal` path),
    /// mirror the removal out of the working config, and hand the producer
    /// teardown (stop-flag raise + bounded join) and the preview-registry RCU
    /// to the off-thread hub. Removing an unknown id is a logged no-op.
    fn remove_source(&mut self, id: &str, drive: &mut CompositorDrive<Nv12Image>) {
        let Some(seam) = self.live_sources.clone() else {
            tracing::warn!(
                source = %id,
                "remove_source held: no live-source hub wired on this run path \
                 (the stored removal applies on restart)"
            );
            return;
        };
        // Teardown-request FIRST (the hub starts raising the producer's stop
        // flags while the drain finishes the binding mutations — the same
        // window-shrinking order as upsert), then unregister the store.
        match seam.request_teardown(id) {
            crate::live_sources::HubSubmit::Accepted => {}
            crate::live_sources::HubSubmit::Full => {
                tracing::warn!(
                    source = %id,
                    "live-source hub queue full; producer teardown shed — the store \
                     is unregistered (slate) but the producer stops only at run teardown"
                );
            }
            crate::live_sources::HubSubmit::Gone => {
                tracing::warn!(
                    source = %id,
                    "live-source hub gone — live apply disabled until restart; the \
                     store is unregistered (slate) but the producer stops only at \
                     run teardown"
                );
            }
        }
        let removed = drive.remove_store(id);
        if !removed {
            tracing::info!(source = %id, "remove_source: no registered store under that id");
        }
        self.config.sources.retain(|s| s.id != id);
        self.applied_media.remove(id);
        self.unapplied.remove(id);
        #[cfg(all(feature = "ffmpeg", feature = "overlay"))]
        if let Some(slot) = &self.audio_free_sources {
            let mut audio_free = (*slot.load()).clone();
            audio_free.remove(id);
            slot.store(Arc::new(audio_free));
        }
    }

    /// Apply an `UpsertOverlay` (ADR-W022) at the frame boundary: upsert the
    /// document by id into the working overlay set (the config mirror — the
    /// same discipline as live sources) and publish the set, generation-
    /// bumped, through the lock-free slot the bake consumer re-derives from
    /// at its next frame. Pure data mutation — no rasterization, no I/O, no
    /// `.await` (invariants #1/#10).
    ///
    /// A document the running build does not visibly render
    /// ([`renders_live`](crate::live_overlays::renders_live) is `false`) is
    /// still mirrored + published (the working set stays coherent) but warned
    /// loudly: the route already declared `restart` for it (ADR-W022 §4) and
    /// the operator should know why nothing changed on screen. Without a
    /// wired seam the command is a surfaced held action — warned and made
    /// observable as a `job.progress` `apply_overlay_held` outcome.
    fn upsert_overlay(&mut self, overlay: &multiview_config::Overlay) {
        let Some(slot) = self.live_overlays.as_ref() else {
            tracing::warn!(
                overlay = %overlay.id,
                "upsert_overlay held: no live overlay seam on this run path \
                 (the stored document applies on restart)"
            );
            self.publish_overlay_held(&overlay.id, "no live overlay seam on this run path");
            return;
        };
        // Mirror: replace-or-append by id (a live edit replaces, never
        // duplicates), then publish the WHOLE set — the consumer derives from
        // the set, so one publish per applied change keeps one truth.
        match self.config.overlays.iter_mut().find(|o| o.id == overlay.id) {
            Some(existing) => *existing = overlay.clone(),
            None => self.config.overlays.push(overlay.clone()),
        }
        let generation = crate::live_overlays::publish_set(slot, self.config.overlays.clone());
        let renders = crate::live_overlays::renders_live(overlay);
        if renders {
            tracing::info!(
                overlay = %overlay.id,
                generation,
                "apply_overlay: overlay applied live at the frame boundary"
            );
        } else {
            tracing::warn!(
                overlay = %overlay.id,
                kind = %overlay.kind,
                generation,
                "apply_overlay: stored + mirrored into the working set, but this \
                 kind has no live renderer in this build — no visual change (ADR-W022)"
            );
        }
        let message = if renders {
            format!("overlay {} applied live at the frame boundary", overlay.id)
        } else {
            format!(
                "overlay {} mirrored (kind {} has no live renderer in this build)",
                overlay.id, overlay.kind
            )
        };
        self.publisher
            .publish_event(Event::JobProgress(JobProgress {
                phase: "apply_overlay".to_owned(),
                pct: 100,
                message: Some(message),
            }));
    }

    /// Apply a `RemoveOverlay` (ADR-W022) at the frame boundary: drop the
    /// document from the working overlay set and republish — a rendered face
    /// disappears on the next baked frame. Removing an unknown id publishes
    /// no new set (the consumer never re-derives spuriously) but is still
    /// surfaced — warned and observable as a `job.progress`
    /// `apply_overlay_held` outcome, symmetric with a held upsert, never a
    /// silent drop. Without a wired seam it is likewise a surfaced held
    /// action.
    fn remove_overlay(&mut self, id: &str) {
        let Some(slot) = self.live_overlays.as_ref() else {
            tracing::warn!(
                overlay = %id,
                "remove_overlay held: no live overlay seam on this run path \
                 (the stored removal applies on restart)"
            );
            self.publish_overlay_held(id, "no live overlay seam on this run path");
            return;
        };
        let before = self.config.overlays.len();
        self.config.overlays.retain(|o| o.id != id);
        if self.config.overlays.len() == before {
            tracing::warn!(
                overlay = %id,
                "remove_overlay held: unknown overlay id (nothing to remove); \
                 no set published"
            );
            self.publish_overlay_held(id, "unknown overlay id (nothing to remove)");
            return;
        }
        let generation = crate::live_overlays::publish_set(slot, self.config.overlays.clone());
        tracing::info!(
            overlay = %id,
            generation,
            "remove_overlay: overlay removed live at the frame boundary"
        );
        self.publisher
            .publish_event(Event::JobProgress(JobProgress {
                phase: "apply_overlay".to_owned(),
                pct: 100,
                message: Some(format!("overlay {id} removed at the frame boundary")),
            }));
    }

    /// Apply a `ReorderOverlays` (task #130) at the frame boundary: re-sequence
    /// the working-overlay mirror to the requested draw order and republish the
    /// set, so the bake consumer re-derives the new equal-`z` draw order on its
    /// next frame. A pure permutation — no document is added, removed, or edited
    /// — and a generation bump only when the order actually changes (idempotent:
    /// a shed retry re-submitting the same order is a no-op that cannot thrash
    /// the consumer). Pure data mutation, no rasterization, no I/O (inv #1/#10).
    ///
    /// `order` is the full desired id sequence. The mirror is sorted by each
    /// entry's index in `order`; an entry whose id is absent from `order` (which
    /// a well-formed reorder never produces — the watcher derives `order` from
    /// the same document the set mirrors) is kept, after the listed ones, in its
    /// prior relative order — never dropped. A reorder with no live seam is a
    /// held action, symmetric with a held upsert/remove.
    fn reorder_overlays(&mut self, order: &[String]) {
        let Some(slot) = self.live_overlays.as_ref() else {
            tracing::warn!(
                "reorder_overlays held: no live overlay seam on this run path \
                 (the stored order applies on restart)"
            );
            self.publish_overlay_held("(set)", "no live overlay seam on this run path");
            return;
        };
        // Stable key: an id listed in `order` sorts by its position there; an
        // unlisted id sorts after all listed ones (usize::MAX) and — because the
        // sort is STABLE — keeps its prior relative order. A pure permutation of
        // the existing entries: nothing is added or removed.
        let rank = |overlay: &multiview_config::Overlay| -> usize {
            order
                .iter()
                .position(|id| *id == overlay.id)
                .unwrap_or(usize::MAX)
        };
        let before: Vec<String> = self.config.overlays.iter().map(|o| o.id.clone()).collect();
        self.config.overlays.sort_by_key(rank);
        let after: Vec<String> = self.config.overlays.iter().map(|o| o.id.clone()).collect();
        if before == after {
            // Already in the requested order — publish nothing (no spurious
            // generation bump / re-derivation), but surface the no-op so a
            // submitted command is never silently swallowed.
            tracing::debug!("reorder_overlays: working set already in the requested order; no-op");
            return;
        }
        let generation = crate::live_overlays::publish_set(slot, self.config.overlays.clone());
        tracing::info!(
            generation,
            order = ?after,
            "reorder_overlays: overlay draw order re-sequenced live at the frame boundary"
        );
        self.publisher
            .publish_event(Event::JobProgress(JobProgress {
                phase: "apply_overlay".to_owned(),
                pct: 100,
                message: Some(format!(
                    "overlay draw order re-sequenced at the frame boundary: {}",
                    after.join(", ")
                )),
            }));
    }

    /// Make a HELD overlay apply observable on the realtime stream (the
    /// ADR-W019 pattern): a `job.progress` outcome with the held phase and
    /// the reason — drop-oldest, never awaits a client (inv #10) — alongside
    /// the `tracing::warn!`.
    fn publish_overlay_held(&self, id: &str, reason: &str) {
        self.publisher
            .publish_event(Event::JobProgress(JobProgress {
                phase: "apply_overlay_held".to_owned(),
                pct: 0,
                message: Some(format!("overlay {id} not applied: {reason}")),
            }));
    }

    /// Publish a resolved audio re-point without borrowing the consumer-owned bus.
    /// The one-store-per-source run supports Best (not arbitrary alternate tracks).
    #[cfg(feature = "ffmpeg")]
    fn route_audio(&self, target: &str, source: &multiview_config::routing::StreamRef) {
        let Some(handle) = self.audio_route.as_ref().and_then(|slot| slot.load()) else {
            tracing::warn!(target, "route_audio held: program audio is not running");
            return;
        };
        if !matches!(
            source.selector,
            multiview_config::routing::StreamSelector::Best
        ) || source.kind != multiview_core::stream::StreamKind::Audio
            || !handle.request_route(target, &source.input_id, 480)
        {
            tracing::warn!(target, source = ?source, "route_audio held: unknown channel, source or track");
        }
    }

    #[cfg(not(feature = "ffmpeg"))]
    #[allow(clippy::unused_self)]
    fn route_audio(&self, target: &str, _source: &multiview_config::routing::StreamRef) {
        tracing::warn!(target, "route_audio held: this build has no program audio");
    }

    /// Apply a `RouteSubtitle` through the live re-point seam. The router
    /// keys layers + sources by source id, so the subtitle `StreamRef`'s `input_id`
    /// names the target source (selector resolution to a specific track within a
    /// source is the run-side caption-track work; identity-by-source is today's
    /// per-source caption model).
    #[cfg(all(feature = "ffmpeg", feature = "overlay"))]
    fn route_subtitle(&mut self, layer: &str, source: &multiview_config::routing::StreamRef) {
        let Some(slot) = self.subtitle_route.as_ref() else {
            tracing::warn!(
                layer = %layer,
                "route_subtitle held: no subtitle route seam wired on this run path"
            );
            return;
        };
        let Some(handle) = slot.load() else {
            // The run has not yet published its live handle (it does so at drive
            // start); a route arriving in that tiny window is held, not panicked on.
            tracing::warn!(
                layer = %layer,
                "route_subtitle held: the run has not yet published its subtitle route handle"
            );
            return;
        };
        handle.request_repoint(layer, &source.input_id);
        if let Some(demand) = &self.demand {
            // A subtitle layer routed to another source consumes that source's
            // captions, which ride its video rail.
            if layer == source.input_id {
                self.subtitle_routes.remove(layer);
            } else {
                self.subtitle_routes
                    .insert(layer.to_owned(), source.input_id.clone());
            }
            demand.subtitles.publish(
                self.subtitle_routes
                    .iter()
                    .map(|(layer, source)| multiview_engine::SetItem {
                        id: source.as_str().into(),
                        label: layer.as_str().into(),
                        size: None,
                    })
                    .collect(),
            );
        }
    }

    /// Without `ffmpeg`+`overlay` the run renders no subtitles, so a `RouteSubtitle`
    /// has no live layer to re-point. Surface it as a held action (never a silent
    /// drop), naming why.
    #[cfg(not(all(feature = "ffmpeg", feature = "overlay")))]
    #[allow(clippy::unused_self)]
    // reason: this method must mirror the `ffmpeg`+`overlay` variant's signature so
    // the single `self.route_subtitle(..)` call site in `route_command` compiles
    // under both feature sets; in this build there is no subtitle seam to consult.
    fn route_subtitle(&self, layer: &str, _source: &multiview_config::routing::StreamRef) {
        tracing::warn!(
            layer = %layer,
            "route_subtitle held: this build renders no subtitles (needs ffmpeg+overlay)"
        );
    }
}

/// Emit an `OutputStatus` event with no measured bitrate/client count (the
/// software engine has no output server wired in yet — this is the running-state
/// echo, not a measured sink status).
fn publish_output_status(
    publisher: &EnginePublisher<EngineStateSnapshot, Event>,
    run_state: OutputRunState,
) {
    publisher.publish_event(Event::OutputStatus(OutputStatus {
        state: run_state,
        bitrate_bps: None,
        clients: None,
    }));
}

/// Build a `SalvoEvent` for `salvo` entering `phase`, scoped to `head` if given.
fn salvo_event(salvo: String, phase: SalvoPhase, head: Option<String>) -> SalvoEvent {
    let event = SalvoEvent::new(salvo, phase);
    match head {
        Some(head) => event.with_head(head),
        None => event,
    }
}

/// Restore the complete saved library from its authoritative envelope — the
/// model's accepted snapshot, NOT a fresh read of `desired.json`: after a
/// start-up fallback (#324) the file still holds the operator's rejected state
/// while the snapshot holds the last-good one the engine actually runs.
fn restore_desired_layouts(
    model: &multiview_control::boot_model::BootModel,
) -> Option<Arc<dyn multiview_control::repository::Repository>> {
    let desired = model.accepted().ok()??;
    let restored = multiview_control::InMemoryRepository::new();
    for layout in &desired.layouts {
        let id = layout.id.clone();
        let input = multiview_control::repository::LayoutInput {
            name: layout.name.clone(),
            body: layout.body.clone(),
        };
        if let Err(error) =
            multiview_control::repository::Repository::create_layout(&restored, &id, input)
        {
            tracing::error!(%error, layout = %id, "cannot restore accepted layout");
            return None;
        }
    }
    Some(Arc::new(restored))
}

/// The stored-layout repository for this run.
///
/// With the `sqlite` feature and a config-file run (a boot model), layouts are
/// persisted to `<config-dir>/.multiview/layouts.db`. Before the first desired
/// commit that database is the legacy library; afterwards it is a projection of
/// the envelope. Otherwise — or if the database cannot be opened — layouts live
/// in memory for the life of the process (logged, never fatal: the control
/// plane must still come up).
#[cfg_attr(
    not(feature = "sqlite"),
    allow(clippy::unused_async, reason = "awaits only the sqlite open")
)]
async fn persist_layouts(
    seeded: &Arc<dyn multiview_control::repository::Repository>,
    boot_model: Option<&multiview_control::boot_model::BootModel>,
) -> Arc<dyn multiview_control::repository::Repository> {
    #[cfg(feature = "sqlite")]
    if let Some(model) = boot_model {
        let dir = model.state_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!(error = %e, dir = %dir.display(), "cannot create the layout store dir; layouts are in-memory only");
        } else {
            let path = dir.join("layouts.db");
            let url = format!("sqlite://{}?mode=rwc", path.display());
            match multiview_control::sqlite::SqliteRepository::connect(&url).await {
                Ok(repo) => {
                    tracing::info!(path = %path.display(), "stored layouts persisted to SQLite");
                    // Once an envelope exists the database is only its
                    // projection: it stays the live repository, and the repair
                    // task `bind_and_serve` starts reconciles it. A corrupt
                    // envelope fails startup in the caller. The model's snapshot
                    // (not `desired.json`) decides: after a fallback (#324) it is
                    // the last-good envelope the engine runs.
                    if !matches!(model.accepted(), Ok(None)) {
                        return Arc::new(repo);
                    }
                    // A legacy library: seed ONLY absent ids. Never overwrite an
                    // operator-authored saved definition at boot.
                    for entry in seeded.list_layouts().unwrap_or_default() {
                        let id = entry.layout.id;
                        let input = multiview_control::repository::LayoutInput {
                            name: entry.layout.name,
                            body: entry.layout.body,
                        };
                        let result =
                            match multiview_control::repository::Repository::get_layout(&repo, &id)
                            {
                                Ok(_) => Ok(()),
                                Err(multiview_control::ControlError::NotFound { .. }) => {
                                    multiview_control::repository::Repository::create_layout(
                                        &repo, &id, input,
                                    )
                                    .map(|_| ())
                                }
                                Err(error) => Err(error),
                            };
                        if let Err(error) = result {
                            tracing::error!(%error, layout = %id, "cannot seed the working layout");
                        }
                    }
                    return Arc::new(repo);
                }
                Err(e) => {
                    tracing::warn!(error = %e, path = %path.display(), "cannot open the layout store; layouts are in-memory only");
                }
            }
        }
    }
    if let Some(model) = boot_model {
        if let Some(restored) = restore_desired_layouts(model) {
            return restored;
        }
    }
    Arc::clone(seeded)
}

/// Whether a decoded (non-synthetic) source kind is live-appliable in this
/// build: the network/file kinds, plus `youtube` (an HLS wrapper, issue #420)
/// when the build carries the `youtube` feature.
fn live_decoded_kind(kind: &multiview_config::SourceKind) -> bool {
    kind.is_network_media()
        || (cfg!(feature = "youtube")
            && matches!(kind, multiview_config::SourceKind::Youtube { .. }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use multiview_compositor::blend::LinearRgba;
    use multiview_compositor::pipeline::CanvasColor;
    use multiview_control::{command_bus, Command, OperationId};
    use multiview_engine::EnginePublisher;
    use multiview_events::{Event, OutputRunState};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A two-source, two-cell config carrying one salvo whose source recall
    /// rebinds `cell_a` from its config-default `in_a` to `in_b`.
    const TWO_CELL_DOC: &str = r##"schema_version = 1
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
columns = ["1fr", "1fr"]
rows = ["1fr"]
areas = ["a b"]
[[sources]]
id = "in_a"
kind = "rtsp"
url = "rtsp://x/a"
[[sources]]
id = "in_b"
kind = "rtsp"
url = "rtsp://x/b"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[cells]]
id = "cell_b"
area = "b"
[cells.source]
input_id = "in_b"
[[salvos]]
id = "salvo_one"
[[salvos.sources]]
cell = "cell_a"
input_id = "in_b"
"##;

    fn test_config() -> MultiviewConfig {
        MultiviewConfig::load_from_toml(TWO_CELL_DOC).expect("parse two-cell config")
    }

    /// Build a real `CompositorDrive` over the test config's solved layout, with
    /// a registered (empty) `TileStore` per declared source so a live re-point to
    /// a declared source resolves (the engine refuses to bind a cell to a source
    /// with no store — RT-6). The stores hold no frame, so every tile shows the
    /// slate; these tests only assert the layout/binding + event effects of the
    /// drain, not the pixels.
    fn test_drive(config: &MultiviewConfig) -> CompositorDrive<Nv12Image> {
        use multiview_framestore::TileStore;
        let layout = config.solve_layout().expect("solve layout");
        let canvas_color = CanvasColor::default();
        let nosignal = Nv12Image::solid(
            config.canvas.width,
            config.canvas.height,
            16,
            128,
            128,
            canvas_color.output_tag(),
        )
        .expect("nosignal card");
        let mut stores = std::collections::HashMap::new();
        for source in &config.sources {
            stores.insert(
                source.id.clone(),
                Arc::new(TileStore::<Nv12Image>::with_defaults(source.id.clone())),
            );
        }
        CompositorDrive::new(
            Arc::new(layout),
            stores,
            nosignal,
            canvas_color,
            LinearRgba::opaque(0.0, 0.0, 0.0),
        )
        .expect("build drive")
    }

    /// The core-cell index whose source binding is `want`, if any.
    fn cell_index_bound_to(drive: &CompositorDrive<Nv12Image>, want: &str) -> Option<usize> {
        drive
            .layout()
            .cells
            .iter()
            .position(|c| c.source.as_deref() == Some(want))
    }

    #[test]
    fn start_then_stop_emits_output_status() {
        let config = test_config();
        let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
        let (sender, command_rx) = command_bus(8);
        let mut sub = publisher.subscribe();
        let mut drain = command_drain(command_rx, config, Arc::clone(&publisher));
        let mut drive = test_drive(&test_config());

        sender
            .try_submit(Command::Start {
                op: OperationId::new(),
            })
            .expect("submit start");
        sender
            .try_submit(Command::Stop {
                op: OperationId::new(),
            })
            .expect("submit stop");
        drain(&mut drive);

        let first = sub.try_recv().expect("first event present");
        match first.event.as_ref() {
            Event::OutputStatus(s) => assert_eq!(s.state, OutputRunState::Running),
            other => panic!("expected Running OutputStatus, got {other:?}"),
        }
        let second = sub.try_recv().expect("second event present");
        match second.event.as_ref() {
            Event::OutputStatus(s) => assert_eq!(s.state, OutputRunState::Idle),
            other => panic!("expected Idle OutputStatus, got {other:?}"),
        }
    }

    #[test]
    fn apply_layout_swaps_active_layout() {
        let config = test_config();
        let working_name = config.solve_layout().expect("solve").name;
        let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
        let (sender, command_rx) = command_bus(8);
        let mut drain = command_drain(command_rx, config, Arc::clone(&publisher));
        let mut drive = test_drive(&test_config());

        // Applying the working layout name re-solves and re-applies successfully:
        // the active layout keeps its (stable) name and is unchanged.
        sender
            .try_submit(Command::ApplyLayout {
                op: OperationId::new(),
                layout: working_name.clone(),
                document: None,
            })
            .expect("submit apply-layout");
        drain(&mut drive);

        assert_eq!(drive.layout().name, working_name);
        assert_eq!(drive.layout().cells.len(), 2);
    }

    #[test]
    fn unknown_layout_emits_failure_not_panic() {
        let config = test_config();
        let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
        let (sender, command_rx) = command_bus(8);
        let mut sub = publisher.subscribe();
        let mut drain = command_drain(command_rx, config, Arc::clone(&publisher));
        let mut drive = test_drive(&test_config());
        let before = drive.layout().name.clone();

        sender
            .try_submit(Command::ApplyLayout {
                op: OperationId::new(),
                layout: "no_such_layout".to_owned(),
                document: None,
            })
            .expect("submit apply-layout");
        // Must not panic.
        drain(&mut drive);

        // The active layout is untouched by an unknown layout id.
        assert_eq!(drive.layout().name, before);
        // No spurious success: no `OutputStatus` (a successful apply does not emit
        // one anyway) and specifically no salvo/tally event is emitted here. The
        // only thing on the stream, if anything, must not claim success — assert
        // there is no event at all.
        assert!(
            sub.try_recv().is_err(),
            "an unknown layout must not emit a success event"
        );
    }

    /// Build a stored absolute-layout [`multiview_control::ResolvedLayout`]
    /// named `wall-x` — one full-canvas cell `stored_cell` bound to `source`
    /// with an `on_loss = black` slate — solved exactly as the apply-layout
    /// route solves it (ADR-W019). `canvas` matches `TWO_CELL_DOC` (64x64@25)
    /// unless overridden.
    fn stored_full_canvas(
        source: &str,
        canvas: &serde_json::Value,
    ) -> multiview_control::ResolvedLayout {
        let body = serde_json::json!({
            "canvas": canvas,
            "layout": { "kind": "absolute" },
            "cells": [{
                "id": "stored_cell",
                "rect": { "x": 0.0, "y": 0.0, "w": 1.0, "h": 1.0 },
                "z": 0,
                "on_loss": { "slate": "black" },
                "source": { "input_id": source }
            }]
        });
        let document =
            multiview_config::LayoutDocument::from_body(&body).expect("stored body parses");
        let solved = document.solve_named("wall-x").expect("stored body solves");
        multiview_control::ResolvedLayout::new(solved, document)
    }

    /// The matching canvas for `TWO_CELL_DOC` (64x64 @ 25/1).
    fn matching_canvas() -> serde_json::Value {
        serde_json::json!({ "width": 64, "height": 64, "fps": "25/1" })
    }

    /// The production fault: a boot layout that binds only some sources, then a
    /// stored layout applied live. The drain must hand the active layout to the
    /// decoders, and a decoder bound to a smaller cell must plan an NVDEC reopen
    /// at the new tile size, while a source the layout drops goes to the small
    /// unbound size, never the full canvas.
    #[test]
    fn applying_a_smaller_layout_makes_the_decoders_reopen_at_the_new_size() {
        use crate::display_size::{
            display_size, plan_display_change, DecoderState, DisplayChange, DisplayFollow,
            DisplayFollower, DISPLAY_SETTLE, UNBOUND_DISPLAY_SIZE,
        };
        use multiview_ffmpeg::TileSize;

        let config = test_config();
        let boot = Arc::new(config.solve_layout().expect("boot layout"));
        let slot = Arc::new(multiview_framestore::SwapCell::new(Arc::clone(&boot)));
        let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
        let (sender, receiver) = command_bus(8);
        let mut drain =
            CommandDrain::new(receiver, config, publisher).with_display_layout(Arc::clone(&slot));
        let mut drive = test_drive(&test_config());

        // Both sources decode at their 32x64 boot cells.
        let boot_size = display_size(&boot, "in_a");
        assert_eq!(boot_size, (32, 64));
        let source = TileSize::new(1920, 1080);
        let opened = DecoderState {
            source,
            hardware: true,
            resize: multiview_ffmpeg::nvdec_resize_target(
                source,
                TileSize::new(boot_size.0, boot_size.1),
                None,
            ),
        };
        let mut follower_a = DisplayFollower::new(
            DisplayFollow::new(Arc::clone(&slot), None),
            "in_a",
            boot_size,
        );
        let mut follower_b = DisplayFollower::new(
            DisplayFollow::new(Arc::clone(&slot), None),
            "in_b",
            boot_size,
        );
        let t0 = std::time::Instant::now();
        assert_eq!(follower_a.due(t0), None, "the boot layout needs no change");

        // A stored layout with in_a in a quarter cell and in_b dropped.
        let body = serde_json::json!({
            "canvas": matching_canvas(),
            "layout": { "kind": "absolute" },
            "cells": [
                { "id": "small", "rect": { "x": 0.0, "y": 0.0, "w": 0.25, "h": 0.25 },
                  "source": { "input_id": "in_a" } }
            ]
        });
        let document = multiview_config::LayoutDocument::from_body(&body).expect("stored doc");
        let solved = document.solve_named("small").expect("stored layout");
        sender
            .try_submit(Command::ApplyLayout {
                op: OperationId::new(),
                layout: "small".to_owned(),
                document: Some(Box::new(multiview_control::ResolvedLayout::new(
                    solved, document,
                ))),
            })
            .expect("apply layout");
        let before_apply = slot.load();
        drain.apply(&mut drive);
        assert!(
            !Arc::ptr_eq(&before_apply, &slot.load()),
            "the drain must publish the applied layout"
        );
        assert_eq!(*slot.load(), *drive.layout());

        // Debounced, then due at the new sizes.
        assert_eq!(follower_a.due(t0), None);
        let settled = t0 + DISPLAY_SETTLE;
        let to_a = follower_a.due(settled).expect("in_a must resize");
        assert_eq!(to_a, (16, 16));
        assert_eq!(
            plan_display_change(opened, boot_size, to_a, None),
            DisplayChange::Reopen {
                resize: Some(TileSize::new(16, 16))
            },
            "a running NVDEC decode must reopen at the new resize target"
        );
        let unbound = (
            UNBOUND_DISPLAY_SIZE.0.min(64) & !1,
            UNBOUND_DISPLAY_SIZE.1.min(64) & !1,
        );
        let _first = follower_b.due(t0);
        assert_eq!(follower_b.due(settled), Some(unbound));

        // A frame boundary with no layout change publishes nothing.
        let current = slot.load();
        drain.apply(&mut drive);
        assert!(Arc::ptr_eq(&current, &slot.load()));
    }

    /// A video route moves a source between cells without a layout command;
    /// the decoders must still see the new binding.
    #[test]
    fn a_video_route_republishes_the_active_layout() {
        let config = test_config();
        let boot = Arc::new(config.solve_layout().expect("boot layout"));
        let slot = Arc::new(multiview_framestore::SwapCell::new(boot));
        let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
        let (sender, receiver) = command_bus(8);
        let mut drain =
            CommandDrain::new(receiver, config, publisher).with_display_layout(Arc::clone(&slot));
        let mut drive = test_drive(&test_config());
        sender
            .try_submit(Command::SwapSource {
                op: OperationId::new(),
                tile: "cell_b".to_owned(),
                source: "in_a".to_owned(),
            })
            .expect("route");
        drain.apply(&mut drive);
        assert_eq!(
            crate::display_size::cell_pixel_size(&slot.load(), "in_b"),
            None,
            "in_b is no longer bound anywhere"
        );
    }

    /// A stored NewsOne-like layout replaces the boot tile chrome, not just the
    /// video geometry; a later video route updates the same cell's label.
    #[cfg(all(feature = "ffmpeg", feature = "overlay"))]
    #[test]
    fn live_layout_publishes_unequal_tile_overlay_placements_and_routes() {
        let config = test_config();
        let boot = config.solve_layout().expect("boot layout");
        let labels = std::collections::HashMap::from([
            ("in_a".to_owned(), "CNN".to_owned()),
            ("in_b".to_owned(), "BBC".to_owned()),
        ]);
        let tiles = Arc::new(multiview_framestore::SwapCell::new(Arc::new(
            crate::pipeline::tile_specs_for_layout(&boot, &labels),
        )));
        let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
        let (sender, receiver) = command_bus(8);
        let mut drain =
            CommandDrain::new(receiver, config, publisher).with_tile_spec_apply(Arc::clone(&tiles));
        let mut drive = test_drive(&test_config());
        let body = serde_json::json!({
            "canvas": matching_canvas(),
            "layout": { "kind": "absolute" },
            "cells": [
                { "id": "hero", "rect": { "x": 0.0, "y": 0.0, "w": 0.75, "h": 1.0 },
                  "source": { "input_id": "in_a" } },
                { "id": "side", "rect": { "x": 0.75, "y": 0.25, "w": 0.25, "h": 0.5 },
                  "source": { "input_id": "in_b" } }
            ]
        });
        let document = multiview_config::LayoutDocument::from_body(&body).expect("stored doc");
        let solved = document.solve_named("NewsOne").expect("stored layout");
        sender
            .try_submit(Command::ApplyLayout {
                op: OperationId::new(),
                layout: "NewsOne".to_owned(),
                document: Some(Box::new(multiview_control::ResolvedLayout::new(
                    solved, document,
                ))),
            })
            .expect("apply layout");
        drain.apply(&mut drive);
        let active = tiles.load();
        assert_eq!(active.len(), 2, "each source-bound tile receives chrome");
        assert_eq!(
            (
                active[0].source_id.as_str(),
                active[0].rect.x,
                active[0].rect.width
            ),
            ("in_a", 0.0, 48.0)
        );
        assert_eq!(
            (
                active[1].source_id.as_str(),
                active[1].rect.x,
                active[1].rect.y,
                active[1].rect.width,
                active[1].rect.height
            ),
            ("in_b", 48.0, 16.0, 16.0, 32.0)
        );
        sender
            .try_submit(Command::SwapSource {
                op: OperationId::new(),
                tile: "side".to_owned(),
                source: "in_a".to_owned(),
            })
            .expect("route side");
        drain.apply(&mut drive);
        let rerouted = tiles.load();
        assert_eq!(rerouted[1].source_id, "in_a");
        assert_eq!(rerouted[1].rect, active[1].rect);
    }

    /// A real command drained on the clock publishes a resolved re-point to the
    /// off-thread consumer, which applies the crossfade before its next audio tick.
    #[cfg(feature = "ffmpeg")]
    #[test]
    fn route_audio_command_reaches_program_bus_crossfade_without_clock_wait() {
        use multiview_config::routing::StreamRef;
        use multiview_core::stream::StreamKind;
        let old = crate::audio::new_store();
        let incoming = crate::audio::new_store();
        // Different steady signals show the crossfade replaces, not appends, the
        // target channel; tick_to(3) also catches up a dropped video tick.
        old.publish_samples(&vec![0.25; 48_000 * 2])
            .expect("old audio");
        incoming
            .publish_samples(&vec![0.75; 48_000 * 2])
            .expect("incoming audio");
        let mut bus = multiview_audio::ProgramBus::new(
            crate::audio::canonical_format(),
            multiview_core::time::Rational::new(25, 1),
        );
        let point = bus.add_source("in_a", Arc::clone(&old), 1.0);
        let mut channels = std::collections::HashMap::new();
        channels.insert("in_a".to_owned(), point);
        let mut stores = std::collections::HashMap::new();
        stores.insert("in_a".to_owned(), old);
        stores.insert("in_b".to_owned(), Arc::clone(&incoming));
        let handle = Arc::new(crate::audio::AudioControlHandle::from_sources(
            stores, channels,
        ));
        let slot = Arc::new(multiview_framestore::SwapCell::new(Some(Arc::clone(
            &handle,
        ))));
        let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
        let (sender, receiver) = command_bus(8);
        let mut drain =
            CommandDrain::new(receiver, test_config(), publisher).with_audio_route(slot);
        let mut drive = test_drive(&test_config());
        sender
            .try_submit(Command::RouteAudio {
                op: OperationId::new(),
                target: "in_a".to_owned(),
                source: StreamRef::best("in_b", StreamKind::Audio),
                gain_db: 0.0,
                mute: false,
            })
            .expect("submit audio breakaway");
        drain.apply(&mut drive);
        assert_eq!(handle.apply_pending(&mut bus), 1);
        // The live edge is intentionally seeked on re-point; fresh decoder audio
        // arriving after that edge is sampled without replaying stale history.
        incoming
            .publish_samples(&vec![0.75; 5_760 * 2])
            .expect("fresh audio");
        let block = bus.tick_to(3);
        assert_eq!(block.frame_count(), 5_760);
        assert!(block
            .interleaved()
            .iter()
            .skip(480 * 2)
            .all(|sample| (*sample - 0.75).abs() < 0.001));
    }

    /// ADR-W019: an `ApplyLayout` carrying a stored, route-solved document swaps
    /// the ACTIVE layout at the frame boundary — geometry, bindings, per-cell
    /// slates, and the re-point address space (cell ids) all follow the stored
    /// document, regardless of any config-layout name.
    #[test]
    fn apply_layout_with_stored_document_swaps_geometry_bindings_and_ids() {
        let config = test_config();
        let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
        let (sender, command_rx) = command_bus(8);
        let mut drain = CommandDrain::new(command_rx, config, Arc::clone(&publisher));
        let mut drive = test_drive(&test_config());
        let mut sub = publisher.subscribe();

        sender
            .try_submit(Command::ApplyLayout {
                op: OperationId::new(),
                layout: "wall-x".to_owned(),
                document: Some(Box::new(stored_full_canvas("in_b", &matching_canvas()))),
            })
            .expect("submit apply-layout");
        let _ = drain.apply(&mut drive);

        // The stored layout is ACTIVE: its name, geometry, and binding.
        assert_eq!(
            drive.layout().name,
            "wall-x",
            "the stored layout must become the active layout"
        );
        assert_eq!(drive.layout().cells.len(), 1);
        let cell = drive.layout().cells.first().expect("one cell");
        assert_eq!(cell.source.as_deref(), Some("in_b"));
        assert!(
            (cell.w - 1.0).abs() < f32::EPSILON && (cell.h - 1.0).abs() < f32::EPSILON,
            "the stored cell spans the full canvas"
        );

        // The re-point address space follows the stored document: the NEW cell
        // id is addressable (an O(1) SwapSource onto it lands).
        sender
            .try_submit(Command::SwapSource {
                op: OperationId::new(),
                tile: "stored_cell".to_owned(),
                source: "in_a".to_owned(),
            })
            .expect("submit swap");
        let _ = drain.apply(&mut drive);
        assert_eq!(
            drive.effective_cell_source("stored_cell"),
            Some("in_a".to_owned()),
            "the stored layout's cell ids must be live re-point addresses"
        );

        // The apply is observable on the realtime stream (drop-oldest, inv #10):
        // a job.progress outcome naming the stored layout id.
        let mut saw_apply = false;
        while let Ok(seq) = sub.try_recv() {
            if let Event::JobProgress(progress) = seq.event.as_ref() {
                if progress.phase == "apply_layout" {
                    assert_eq!(progress.pct, 100);
                    assert!(
                        progress
                            .message
                            .as_deref()
                            .unwrap_or_default()
                            .contains("wall-x"),
                        "the outcome names the stored layout id"
                    );
                    saw_apply = true;
                }
            }
        }
        assert!(
            saw_apply,
            "a successful stored-layout apply emits a job.progress outcome"
        );
    }

    /// ADR-W019: the next composited frame PROVES the apply — pixels that were
    /// the left cell's no-signal slate become the stored layout's full-canvas
    /// source on the very next tick.
    #[test]
    fn apply_layout_changes_the_next_composited_frame() {
        use multiview_core::time::MediaTime;
        let config = test_config();
        let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
        let (sender, command_rx) = command_bus(8);
        let mut drain = CommandDrain::new(command_rx, config, Arc::clone(&publisher));

        // A real drive whose `in_b` store holds a BRIGHT frame (luma 200); `in_a`
        // stays empty so cell_a (the left half) composes the dark slate.
        let cfg = test_config();
        let drive_cfg = test_config();
        let mut drive = test_drive(&drive_cfg);
        let bright = Nv12Image::solid(
            cfg.canvas.width,
            cfg.canvas.height,
            200,
            128,
            128,
            multiview_compositor::pipeline::CanvasColor::default().output_tag(),
        )
        .expect("bright frame");
        // Reach the in_b store through a fresh drive build is not possible here;
        // publish via a store registered on the drive instead.
        let store = Arc::new(multiview_framestore::TileStore::<Nv12Image>::with_defaults(
            "in_b",
        ));
        store.publish(bright, MediaTime::from_nanos(0));
        drive.insert_store("in_b", Arc::clone(&store));

        let tick = |index: u64| multiview_engine::Tick {
            index,
            pts: MediaTime::from_nanos(0),
        };
        // Left-half center pixel: cell_a samples the empty `in_a` → slate (dark).
        let before = drive.compose(tick(0)).expect("compose before");
        let (y_before, _, _) = before.canvas.sample(16, 32).expect("sample before");
        assert!(
            y_before < 64,
            "before the apply the left half is the dark slate (got luma {y_before})"
        );

        sender
            .try_submit(Command::ApplyLayout {
                op: OperationId::new(),
                layout: "wall-x".to_owned(),
                document: Some(Box::new(stored_full_canvas("in_b", &matching_canvas()))),
            })
            .expect("submit apply-layout");
        let _ = drain.apply(&mut drive);

        // The very next composited frame draws the stored layout: the same
        // pixel is now the bright full-canvas `in_b` source.
        let after = drive.compose(tick(1)).expect("compose after");
        let (y_after, _, _) = after.canvas.sample(16, 32).expect("sample after");
        assert!(
            y_after > 150,
            "after the apply the next frame draws the stored full-canvas source \
             (got luma {y_after}, was {y_before})"
        );
    }

    /// ADR-R004 / ADR-W019 guard: the output canvas (geometry + cadence) is
    /// PINNED for the session — a stored document authored for a different
    /// canvas is held (warned), never adopted, and the output keeps composing
    /// on the pinned canvas. (The route refuses this with 422; the drain is the
    /// authoritative backstop.)
    #[test]
    fn apply_layout_with_mismatched_canvas_is_held() {
        let config = test_config();
        let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
        let (sender, command_rx) = command_bus(8);
        let mut drain = CommandDrain::new(command_rx, config, Arc::clone(&publisher));
        let mut drive = test_drive(&test_config());
        let before = drive.layout().name.clone();
        let mut sub = publisher.subscribe();

        // Same document shape, WRONG canvas (128x128@30 vs the running 64x64@25).
        let mismatched = stored_full_canvas(
            "in_b",
            &serde_json::json!({ "width": 128, "height": 128, "fps": "30/1" }),
        );
        sender
            .try_submit(Command::ApplyLayout {
                op: OperationId::new(),
                layout: "wall-x".to_owned(),
                document: Some(Box::new(mismatched)),
            })
            .expect("submit apply-layout");
        let _ = drain.apply(&mut drive);

        assert_eq!(
            drive.layout().name,
            before,
            "a pinned-canvas mismatch must be held, never adopted (Class-2)"
        );
        assert_eq!(
            drive.layout().canvas.width,
            64,
            "the pinned canvas survives"
        );

        // MINOR-2 (ADR-W019 review): the broken promise must be OBSERVABLE on
        // the realtime stream, not only a tracing line — a `job.progress`
        // outcome with the held phase, pct < 100, naming the reason.
        let mut saw_held = false;
        while let Ok(seq) = sub.try_recv() {
            if let Event::JobProgress(progress) = seq.event.as_ref() {
                if progress.phase == "apply_layout_held" {
                    assert!(progress.pct < 100, "a held apply is not 100% complete");
                    let message = progress.message.as_deref().unwrap_or_default();
                    assert!(
                        message.contains("wall-x") && message.contains("canvas"),
                        "the held outcome names the layout and the reason, got {message:?}"
                    );
                    saw_held = true;
                }
            }
        }
        assert!(
            saw_held,
            "a held stored-layout apply emits an apply_layout_held outcome (inv #10 drop-oldest)"
        );
    }

    /// #317 backstop: with a switcher M/E declared, a stored multi-cell layout
    /// reaching the drain (the route refuses it first) is HELD with the
    /// operator reason, never swapped under the running M/E.
    #[test]
    fn apply_layout_multi_cell_under_a_switcher_is_held() {
        let mut config = test_config();
        config.switcher = Some(multiview_config::SwitcherConfig {
            mix_effects: vec![multiview_config::MixEffectsConfig {
                id: "me1".into(),
                program: "main".into(),
                program_source: "in_a".into(),
                preview_source: "in_b".into(),
                flip_flop: true,
                inputs: vec![],
                program_input: None,
                preview_input: None,
            }],
            extra: serde_json::Map::new(),
        });
        let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
        let (sender, command_rx) = command_bus(8);
        let mut drain = CommandDrain::new(command_rx, config.clone(), Arc::clone(&publisher));
        let mut drive = test_drive(&config);
        let before = drive.layout().clone();
        let mut sub = publisher.subscribe();
        let body = serde_json::json!({
            "canvas": matching_canvas(),
            "layout": { "kind": "absolute" },
            "cells": [
                { "id": "l", "rect": { "x": 0.0, "y": 0.0, "w": 0.5, "h": 1.0 }, "source": { "input_id": "in_a" } },
                { "id": "r", "rect": { "x": 0.5, "y": 0.0, "w": 0.5, "h": 1.0 }, "source": { "input_id": "in_b" } }
            ]
        });
        let document = multiview_config::LayoutDocument::from_body(&body).expect("body parses");
        let solved = document.solve_named("NewsOne").expect("body solves");
        sender
            .try_submit(Command::ApplyLayout {
                op: OperationId::new(),
                layout: "NewsOne".to_owned(),
                document: Some(Box::new(multiview_control::ResolvedLayout::new(
                    solved, document,
                ))),
            })
            .expect("submit apply-layout");
        let _ = drain.apply(&mut drive);
        assert_eq!(drive.layout().cells, before.cells, "held, never swapped");
        let mut saw_held = false;
        while let Ok(seq) = sub.try_recv() {
            if let Event::JobProgress(progress) = seq.event.as_ref() {
                if progress.phase == "apply_layout_held" {
                    let message = progress.message.as_deref().unwrap_or_default();
                    assert!(message.contains("The switcher is on"), "{message:?}");
                    saw_held = true;
                }
            }
        }
        assert!(saw_held, "the hold is observable on the realtime stream");
    }

    /// The two-cell test config with an M/E declared (program `in_a`).
    fn switcher_config() -> MultiviewConfig {
        let mut config = test_config();
        config.switcher = Some(multiview_config::SwitcherConfig {
            mix_effects: vec![multiview_config::MixEffectsConfig {
                id: "me1".into(),
                program: "main".into(),
                program_source: "in_a".into(),
                preview_source: "in_b".into(),
                flip_flop: true,
                inputs: vec![],
                program_input: None,
                preview_input: None,
            }],
            extra: serde_json::Map::new(),
        });
        config
    }

    /// The Program drive `Pipeline::build` makes for `config` (#307): the
    /// M/E's one-cell bus, keyed by its reserved cell id.
    fn switcher_drive(config: &MultiviewConfig) -> (CompositorDrive<Nv12Image>, String) {
        let program = config
            .program_canvas(config.solve_layout().expect("solve"))
            .expect("program canvas");
        let mut doc = config.clone();
        doc.switcher = None;
        let base = test_drive(&doc);
        let id = program.cell_id.expect("program cell id");
        let mut drive = base;
        drive
            .set_layout(Arc::new(program.layout))
            .expect("program layout");
        let drive = drive.with_cell_ids(vec![Some(id.clone())]);
        (drive, id)
    }

    /// #307: with a switcher M/E declared, a stored multi-cell layout reaching
    /// the drain (the route refuses apply-layout first; the config watcher
    /// forwards a boot-file edit) changes only the document mirror and is
    /// never put on the Program drive, which stays the one-cell M/E bus.
    #[test]
    fn apply_layout_multi_cell_under_a_switcher_never_reaches_the_program_drive() {
        let config = switcher_config();
        let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
        let (sender, command_rx) = command_bus(8);
        let spy = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut drain = CommandDrain::new(command_rx, config.clone(), Arc::clone(&publisher))
            .with_resolve_spy(&spy);
        let (mut drive, cell_id) = switcher_drive(&config);
        let before = drive.layout().clone();
        assert_eq!(before.cells.len(), 1, "Program is the one-cell M/E bus");
        let mut sub = publisher.subscribe();
        let body = serde_json::json!({
            "canvas": matching_canvas(),
            "layout": { "kind": "absolute" },
            "cells": [
                { "id": "l", "rect": { "x": 0.0, "y": 0.0, "w": 0.5, "h": 1.0 }, "source": { "input_id": "in_a" } },
                { "id": "r", "rect": { "x": 0.5, "y": 0.0, "w": 0.5, "h": 1.0 }, "source": { "input_id": "in_b" } }
            ]
        });
        let document = multiview_config::LayoutDocument::from_body(&body).expect("body parses");
        let solved = document.solve_named("NewsOne").expect("body solves");
        sender
            .try_submit(Command::ApplyLayout {
                op: OperationId::new(),
                layout: "NewsOne".to_owned(),
                document: Some(Box::new(multiview_control::ResolvedLayout::new(
                    solved, document,
                ))),
            })
            .expect("submit apply-layout");
        // The back-compat (no document) re-solve path must not land either.
        let working = config.solve_layout().expect("solve").name;
        sender
            .try_submit(Command::ApplyLayout {
                op: OperationId::new(),
                layout: working,
                document: None,
            })
            .expect("submit working apply-layout");
        let _ = drain.apply(&mut drive);
        assert_eq!(
            spy.load(std::sync::atomic::Ordering::Acquire),
            0,
            "switcher ApplyLayout commands (stored and no-document) solve nothing \
             on the output clock (review I1)"
        );
        assert_eq!(
            drive.layout().cells,
            before.cells,
            "Program drive untouched"
        );
        assert_eq!(
            drive.effective_cell_source(&cell_id),
            Some("in_a".to_owned()),
            "the M/E's Program cell binding is not reset"
        );
        assert_eq!(
            drain
                .config
                .cells
                .iter()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>(),
            ["l", "r"],
            "the document (stored layout) follows the apply"
        );
        let mut saw_held = false;
        while let Ok(seq) = sub.try_recv() {
            if let Event::JobProgress(progress) = seq.event.as_ref() {
                if progress.phase == "apply_layout_held" {
                    let message = progress.message.as_deref().unwrap_or_default();
                    assert!(message.contains("The switcher is on"), "{message:?}");
                    saw_held = true;
                }
            }
        }
        assert!(saw_held, "the outcome is observable on the realtime stream");
    }

    /// #307 review M1: `SwapSource` / `RouteVideo` / a cell-recalling salvo
    /// reaching the drain under a switcher (the control plane refuses them)
    /// are dropped: never the M/E's Program cell, and never the working-config
    /// mirror, which stays equal to the accepted state.
    #[test]
    fn cell_repoints_under_a_switcher_are_dropped_and_never_touch_the_mirror() {
        let config = switcher_config();
        let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
        let (sender, command_rx) = command_bus(8);
        let mut drain = CommandDrain::new(command_rx, config.clone(), Arc::clone(&publisher));
        let (mut drive, cell_id) = switcher_drive(&config);
        sender
            .try_submit(Command::SwapSource {
                op: OperationId::new(),
                tile: "cell_a".to_owned(),
                source: "in_b".to_owned(),
            })
            .expect("submit swap");
        sender
            .try_submit(Command::RouteVideo {
                op: OperationId::new(),
                cell: "cell_b".to_owned(),
                source: multiview_config::routing::StreamRef::best(
                    "in_a".to_owned(),
                    multiview_core::stream::StreamKind::Video,
                ),
            })
            .expect("submit route");
        sender
            .try_submit(Command::TakeSalvo {
                op: OperationId::new(),
                salvo: Some("salvo_one".to_owned()),
                head: None,
            })
            .expect("submit salvo");
        let applied = drain.apply(&mut drive);
        assert_eq!(applied, 0, "no re-point reaches the Program drive");
        assert_eq!(
            drive.effective_cell_source(&cell_id),
            Some("in_a".to_owned())
        );
        assert_eq!(drive.layout().cells.len(), 1);
        assert_eq!(
            drain.config.cells, config.cells,
            "the mirror never diverges from the accepted state"
        );
    }

    /// #307: tally and audio-follow-video read the Program source from the
    /// composed frame's `source_states` (`StreamItem.switcher_audio` takes its
    /// first key). With a multi-cell document under a switcher, the Program
    /// canvas binds exactly one source, `program_source`, and after the M/E
    /// rebinds its cell, exactly the new one; the wall's other source never
    /// appears.
    #[test]
    fn tally_and_afv_see_only_the_program_source_with_a_multi_cell_document() {
        use multiview_core::time::MediaTime;
        let config = switcher_config();
        let (mut drive, cell_id) = switcher_drive(&config);
        let tick = |index: u64| multiview_engine::Tick {
            index,
            pts: MediaTime::from_nanos(0),
        };
        let frame = drive.compose(tick(0)).expect("compose");
        assert_eq!(
            frame.source_states.keys().collect::<Vec<_>>(),
            ["in_a"],
            "one bound source: the Program source"
        );
        drive.rebind_cell(&cell_id, "in_b").expect("M/E cut");
        let frame = drive.compose(tick(1)).expect("compose after cut");
        assert_eq!(frame.source_states.keys().collect::<Vec<_>>(), ["in_b"]);
    }

    /// #307 fail-closed retirement: removing the source on Program with the
    /// synthesized canvas unregisters its store, and the M/E's Program cell
    /// composites its failover slate from the next tick (never a stale frame,
    /// never a panic).
    #[test]
    fn removing_the_program_source_under_a_switcher_fails_closed() {
        use multiview_core::time::MediaTime;
        let config = switcher_config();
        let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
        let (sender, command_rx) = command_bus(8);
        let hub = crate::live_sources::LiveSourceHub::start(
            crate::live_sources::stop_registry(),
            crate::live_sources::shared_stores(std::collections::HashMap::new()),
        );
        let mut drain = CommandDrain::new(command_rx, config.clone(), Arc::clone(&publisher))
            .with_live_sources(hub.handle());
        let (mut drive, cell_id) = switcher_drive(&config);
        let bright = Nv12Image::solid(64, 64, 200, 128, 128, CanvasColor::default().output_tag())
            .expect("bright frame");
        let store = Arc::new(multiview_framestore::TileStore::<Nv12Image>::with_defaults(
            "in_a",
        ));
        store.publish(bright, MediaTime::from_nanos(0));
        drive.insert_store("in_a", store);
        let tick = |index: u64| multiview_engine::Tick {
            index,
            pts: MediaTime::from_nanos(0),
        };
        let _ = drain.apply(&mut drive);
        let before = drive.compose(tick(0)).expect("compose before");
        assert_eq!(
            before.source_states.get("in_a"),
            Some(&multiview_core::traits::SourceState::Live)
        );
        sender
            .try_submit(Command::RemoveSource {
                op: OperationId::new(),
                id: "in_a".to_owned(),
            })
            .expect("submit remove");
        let _ = drain.apply(&mut drive);
        let after = drive.compose(tick(1)).expect("compose after");
        assert_eq!(
            after.source_states.get("in_a"),
            Some(&multiview_core::traits::SourceState::NoSignal),
            "the Program cell rides its slate"
        );
        let (y, _, _) = after.canvas.sample(32, 32).expect("sample");
        assert!(y < 190, "the removed source's frame is gone (luma {y})");
        assert!(drive.rebind_cell(&cell_id, "in_a").is_err());
        hub.shutdown();
    }

    /// MINOR-3 (ADR-W019 review): the drain's pinned-canvas backstop compares
    /// cadence by VALUE — a stored `50/2` against the running `25/1` is the
    /// same signal and must apply, never a false Class-2 hold.
    #[test]
    fn apply_layout_with_equivalent_cadence_applies() {
        let config = test_config();
        let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
        let (sender, command_rx) = command_bus(8);
        let mut drain = CommandDrain::new(command_rx, config, Arc::clone(&publisher));
        let mut drive = test_drive(&test_config());

        // Identical geometry, equivalent non-reduced cadence (50/2 == 25/1).
        let equivalent = stored_full_canvas(
            "in_b",
            &serde_json::json!({ "width": 64, "height": 64, "fps": "50/2" }),
        );
        sender
            .try_submit(Command::ApplyLayout {
                op: OperationId::new(),
                layout: "wall-x".to_owned(),
                document: Some(Box::new(equivalent)),
            })
            .expect("submit apply-layout");
        let _ = drain.apply(&mut drive);

        assert_eq!(
            drive.layout().name,
            "wall-x",
            "an equivalent (non-reduced) cadence is the SAME pinned signal and must apply"
        );
    }

    #[test]
    fn salvo_take_applies_armed_layout() {
        let config = test_config();
        let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
        let (sender, command_rx) = command_bus(8);
        let mut sub = publisher.subscribe();
        let mut drain = command_drain(command_rx, config, Arc::clone(&publisher));
        let mut drive = test_drive(&test_config());

        // Before: cell_a (index 0) is bound to in_a; cell_b (index 1) to in_b.
        assert_eq!(
            drive.layout().cells.first().and_then(|c| c.source.clone()),
            Some("in_a".to_owned())
        );

        sender
            .try_submit(Command::ArmSalvo {
                op: OperationId::new(),
                salvo: "salvo_one".to_owned(),
                head: None,
            })
            .expect("submit arm");
        sender
            .try_submit(Command::TakeSalvo {
                op: OperationId::new(),
                salvo: None,
                head: None,
            })
            .expect("submit take");
        drain(&mut drive);

        // The salvo rebinds cell_a's source to in_b; both cells now show in_b.
        assert_eq!(
            drive.layout().cells.first().and_then(|c| c.source.clone()),
            Some("in_b".to_owned()),
            "salvo take must rebind cell_a to in_b"
        );
        // Both cell indices are now bound to in_b (cell_b already was).
        assert!(cell_index_bound_to(&drive, "in_a").is_none());

        // Arm and Take each emit their salvo lifecycle event.
        let armed = sub.try_recv().expect("armed event");
        assert!(
            matches!(armed.event.as_ref(), Event::SalvoArmed(e) if e.salvo == "salvo_one"),
            "expected SalvoArmed, got {:?}",
            armed.event
        );
        let taken = sub.try_recv().expect("taken event");
        assert!(
            matches!(taken.event.as_ref(), Event::SalvoTaken(e) if e.salvo == "salvo_one"),
            "expected SalvoTaken, got {:?}",
            taken.event
        );
    }

    #[test]
    fn drain_is_bounded_and_never_awaits() {
        let config = test_config();
        let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
        let (sender, command_rx) = command_bus(64);
        let mut drain = command_drain(command_rx, config, Arc::clone(&publisher));
        let mut drive = test_drive(&test_config());

        // Flood the bus with a mix of accepted commands.
        for _ in 0..16 {
            sender
                .try_submit(Command::Start {
                    op: OperationId::new(),
                })
                .expect("submit start");
            sender
                .try_submit(Command::SwapSource {
                    op: OperationId::new(),
                    tile: "cell_a".to_owned(),
                    source: "in_b".to_owned(),
                })
                .expect("submit swap");
        }

        // The drain is a synchronous `FnMut`: calling it processes every pending
        // command in O(pending) and returns without awaiting anything. A second
        // call over the now-empty bus is a no-op and also returns.
        drain(&mut drive);
        drain(&mut drive);

        // The swaps took effect (cell_a now bound to in_b) — proof the loop ran
        // to completion rather than blocking.
        assert_eq!(
            drive.layout().cells.first().and_then(|c| c.source.clone()),
            Some("in_b".to_owned())
        );
    }

    /// A K-command salvo of pure source re-points must trigger **at most one**
    /// `solve_layout` re-solve per tick (the coalesce gate) — and in fact zero,
    /// because a pure source re-point goes through the O(1) `rebind_cell` path,
    /// never the full layout re-solve. The spy counts every `solve_layout` call
    /// the drain makes (RT-6 hard gate #1: no O(1) claim without removing
    /// `solve_layout` from the re-point path).
    #[test]
    fn salvo_of_repoints_does_at_most_one_resolve() {
        let config = test_config();
        let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
        let (sender, command_rx) = command_bus(64);
        let resolves = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut drain = CommandDrain::new(command_rx, config, Arc::clone(&publisher))
            .with_resolve_spy(&resolves);
        let mut drive = test_drive(&test_config());

        // A salvo storm: a batch of direct SwapSource re-points — all pure source
        // re-points (no geometry change).
        for _ in 0..32 {
            sender
                .try_submit(Command::SwapSource {
                    op: OperationId::new(),
                    tile: "cell_a".to_owned(),
                    source: "in_b".to_owned(),
                })
                .expect("submit swap");
        }
        let _applied = drain.apply(&mut drive);

        let count = resolves.load(std::sync::atomic::Ordering::Acquire);
        assert!(
            count <= 1,
            "a K-command salvo of pure source re-points must do <=1 layout \
             re-solve (got {count}); pure re-points use the O(1) rebind path"
        );

        // The re-point still took effect (the binding is live).
        assert_eq!(
            drive.effective_cell_source("cell_a"),
            Some("in_b".to_owned()),
            "the re-point must be applied via rebind_cell"
        );
    }

    /// Under a command storm exceeding the per-tick cap, the drain applies at
    /// most `MAX_REPOINTS_PER_TICK` re-points in a single tick and never blows
    /// the tick budget (the bounded-drain gate, RT-6 hard gate test (c)). The
    /// remaining re-points are deferred to later ticks (or shed), not applied in
    /// one unbounded burst.
    #[test]
    fn repoint_storm_is_capped_per_tick() {
        let config = test_config();
        let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(256));
        let (sender, command_rx) = command_bus(256);
        let mut drain = CommandDrain::new(command_rx, config, Arc::clone(&publisher));
        let mut drive = test_drive(&test_config());

        // Far more re-points than the per-tick cap.
        let storm = MAX_REPOINTS_PER_TICK.saturating_mul(8).max(64);
        for i in 0..storm {
            let source = if i % 2 == 0 { "in_b" } else { "in_a" };
            sender
                .try_submit(Command::SwapSource {
                    op: OperationId::new(),
                    tile: "cell_a".to_owned(),
                    source: source.to_owned(),
                })
                .expect("submit swap");
        }

        // One drain must apply AT MOST the cap (bounded tick budget), reporting
        // how many re-points it applied this tick.
        let applied = drain.apply(&mut drive);
        assert!(
            applied <= MAX_REPOINTS_PER_TICK,
            "a single tick must apply at most {MAX_REPOINTS_PER_TICK} re-points \
             (applied {applied}); the storm must be capped, not applied in one burst"
        );
        assert!(
            applied > 0,
            "the drain must make progress (applied {applied})"
        );

        // Draining repeatedly drains the deferred backlog without ever exceeding
        // the cap on any single tick — the budget holds across ticks.
        for _ in 0..storm {
            let n = drain.apply(&mut drive);
            assert!(
                n <= MAX_REPOINTS_PER_TICK,
                "every tick stays within the cap (got {n})"
            );
        }
    }

    #[test]
    fn state_snapshot_is_compact_and_tagged() {
        let snap = state_snapshot(7, 233_333_333, 1920, 1080);
        assert_eq!(snap["v"], 1);
        assert_eq!(snap["tick"], 7);
        assert_eq!(snap["pts_ns"], 233_333_333_i64);
        assert_eq!(snap["canvas"]["width"], 1920);
        assert_eq!(snap["canvas"]["height"], 1080);
        // No inputs were folded in, so the snapshot stays minimal (no `inputs`
        // key) — the base blob is unchanged for the synthetic/empty-probe path.
        assert!(snap.get("inputs").is_none());
    }

    #[test]
    fn fold_tile_states_adds_sorted_lifecycle_tiles() {
        let mut states = std::collections::HashMap::new();
        states.insert(
            "zeta".to_owned(),
            multiview_core::traits::SourceState::NoSignal,
        );
        states.insert(
            "alpha".to_owned(),
            multiview_core::traits::SourceState::Live,
        );
        states.insert(
            "mid".to_owned(),
            multiview_core::traits::SourceState::Reconnecting,
        );

        let mut snap = state_snapshot(7, 233_333_333, 1920, 1080);
        fold_tile_states(&mut snap, &states);

        // Sorted by id (HashMap order is non-deterministic; the wire must not
        // be), with the SAME LifecycleState wire strings the events use.
        let tiles = snap["tiles"].as_array().expect("tiles array");
        assert_eq!(
            tiles,
            &vec![
                serde_json::json!({"id": "alpha", "state": "LIVE"}),
                serde_json::json!({"id": "mid", "state": "RECONNECTING"}),
                serde_json::json!({"id": "zeta", "state": "NO_SIGNAL"}),
            ]
        );
        // The base fields are untouched by the fold.
        assert_eq!(snap["v"], 1);
        assert_eq!(snap["tick"], 7);
    }

    /// A `youtube` source's recorded resolve-failure reason (ADR-0015) rides
    /// the same `reason` field a client already defensively parses
    /// (realtime-api.md §3), and clears once the source resolves again.
    #[cfg(feature = "youtube")]
    #[test]
    fn fold_tile_states_carries_the_youtube_resolve_failure_reason() {
        let id = "control-test-youtube-reason-tile";
        crate::youtube_status::record(id, Some("not a live stream (status: upcoming)"));

        let mut states = std::collections::HashMap::new();
        states.insert(id.to_owned(), multiview_core::traits::SourceState::NoSignal);
        let mut snap = state_snapshot(1, 0, 64, 64);
        fold_tile_states(&mut snap, &states);

        let tiles = snap["tiles"].as_array().expect("tiles array");
        assert_eq!(tiles.len(), 1);
        assert_eq!(
            tiles[0]["reason"],
            serde_json::json!("not a live stream (status: upcoming)")
        );

        // A later successful resolve clears the stored reason, so a fresh fold
        // never carries a stale one forward.
        crate::youtube_status::record(id, None);
        let mut snap2 = state_snapshot(2, 0, 64, 64);
        fold_tile_states(&mut snap2, &states);
        assert!(snap2["tiles"][0].get("reason").is_none());
    }

    #[test]
    fn fold_tile_states_empty_map_yields_an_empty_tiles_array() {
        // A run with no sources still publishes `tiles: []` so a connected
        // client rebuilds to an EMPTY tile cache (not a stale one).
        let states: std::collections::HashMap<String, multiview_core::traits::SourceState> =
            std::collections::HashMap::new();
        let mut snap = state_snapshot(3, 9, 64, 64);
        fold_tile_states(&mut snap, &states);
        assert_eq!(snap["tiles"], serde_json::json!([]));
    }

    /// A tiny representative inventory (one video + one audio) for the fold-in /
    /// event-projection tests.
    fn fixture_inventory(input_id: &str) -> multiview_core::stream::StreamInventory {
        use multiview_core::stream::{
            StableStreamId, StreamDescriptor, StreamDetail, StreamInventory, StreamKind,
        };
        let video = StreamDescriptor::new(
            StableStreamId::from_ts_pid(StreamKind::Video, 0x100),
            StreamKind::Video,
            "h264",
            StreamDetail::Video {
                width: 1920,
                height: 1080,
                frame_rate: None,
            },
        );
        let audio = StreamDescriptor::new(
            StableStreamId::from_general(StreamKind::Audio, 0, "aac", None, None),
            StreamKind::Audio,
            "aac",
            StreamDetail::Audio {
                channels: 2,
                sample_rate: 48_000,
            },
        )
        .with_default(true);
        StreamInventory::from_streams(vec![video, audio]).with_input_id(input_id)
    }

    #[test]
    fn folding_inventories_threads_them_into_the_snapshot_under_inputs() {
        let mut inventories = std::collections::BTreeMap::new();
        inventories.insert("cam1".to_owned(), fixture_inventory("cam1"));

        let mut snap = state_snapshot(0, 0, 1920, 1080);
        fold_input_inventories(&mut snap, &inventories);

        // The inventory is folded into the conflated blob under
        // `inputs.<id>.streams` — exactly the shape the control endpoint reads.
        let streams = &snap["inputs"]["cam1"]["streams"];
        assert_eq!(streams["input_id"], "cam1");
        let arr = streams["streams"].as_array().expect("streams array");
        assert_eq!(
            arr.len(),
            2,
            "both elementary streams survive into the blob"
        );
        // The folded fragment round-trips back into a real StreamInventory (the
        // control plane will deserialise it on read).
        let back: multiview_core::stream::StreamInventory = serde_json::from_value(streams.clone())
            .expect("the folded fragment is a valid inventory");
        assert_eq!(back, fixture_inventory("cam1"));
        // The base fields are untouched by the fold.
        assert_eq!(snap["v"], 1);
        assert_eq!(snap["canvas"]["width"], 1920);
    }

    #[test]
    fn prebuilt_fragment_inserts_identically_to_a_direct_fold() {
        // The hot-loop path (pre-build once + insert) must produce a snapshot
        // byte-identical to the direct fold, so the cheaper per-tick path can't
        // drift from the tested fold.
        let mut inventories = std::collections::BTreeMap::new();
        inventories.insert("cam1".to_owned(), fixture_inventory("cam1"));
        inventories.insert("cam2".to_owned(), fixture_inventory("cam2"));

        let fragment = input_inventories_fragment(&inventories);
        assert!(fragment.is_some(), "a non-empty map yields a fragment");

        let mut via_fold = state_snapshot(5, 1, 16, 16);
        fold_input_inventories(&mut via_fold, &inventories);

        let mut via_insert = state_snapshot(5, 1, 16, 16);
        insert_input_fragment(&mut via_insert, fragment.as_ref());

        assert_eq!(via_fold, via_insert);
        // And an absent fragment is a no-op.
        let mut untouched = state_snapshot(5, 1, 16, 16);
        let before = untouched.clone();
        insert_input_fragment(&mut untouched, None);
        assert_eq!(untouched, before);
        assert!(input_inventories_fragment(&std::collections::BTreeMap::new()).is_none());
    }

    #[test]
    fn folding_empty_map_leaves_the_snapshot_unchanged() {
        let inventories: std::collections::BTreeMap<
            String,
            multiview_core::stream::StreamInventory,
        > = std::collections::BTreeMap::new();
        let mut snap = state_snapshot(3, 9, 64, 64);
        let before = snap.clone();
        fold_input_inventories(&mut snap, &inventories);
        assert_eq!(snap, before, "no inputs ⇒ no `inputs` key, blob unchanged");
    }

    #[test]
    fn input_streams_events_are_one_per_input_tagged_and_routed() {
        let mut inventories = std::collections::BTreeMap::new();
        inventories.insert("cam1".to_owned(), fixture_inventory("cam1"));
        inventories.insert("cam2".to_owned(), fixture_inventory("cam2"));

        let events = input_streams_events(&inventories);
        // Exactly one `input.streams` event per input (no duplicates), and BTreeMap
        // order makes the projection deterministic.
        assert_eq!(events.len(), 2);
        for (event, expect_id) in events.iter().zip(["cam1", "cam2"]) {
            match event {
                Event::InputStreams(is) => {
                    assert_eq!(is.input_id, expect_id);
                    assert_eq!(is.inventory, fixture_inventory(expect_id));
                }
                other => panic!("expected Event::InputStreams, got {other:?}"),
            }
            // It must ride the existing `inputs` lane (RT-3), never the control
            // catch-all.
            assert_eq!(
                multiview_control::realtime::topic_for_event(event),
                multiview_events::Topic::Inputs
            );
            assert_eq!(event.type_tag(), "input.streams");
        }
    }

    /// `bind_and_serve` binds a real loopback socket, serves the unauthenticated
    /// `OpenAPI` document, and returns cleanly once its shutdown future resolves.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bind_and_serve_exposes_openapi_then_shuts_down() {
        let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
        let (commands, _rx) = multiview_control::command_bus(8);
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        // IPv6-first: the CLI serve path must bind the IPv6 loopback `[::1]`.
        let (addr, handle, _state) = bind_and_serve(
            "[::1]:0",
            &test_config(),
            publisher,
            commands,
            multiview_control::no_preview(),
            None, // switcher: absent unless a real paired bus is attached
            None, // switcher lease owner absent
            // whep: the default (no native transport) — a pure build path.
            None,
            None,
            None,
            None,
            None,
            multiview_control::LiveApplyCaps::default(),
            multiview_control::LiveSourceCapability::synthetic_only(),
            // boot_model: none for this store-only bind test (ADR-W024).
            None,
            // No source event runtime on this store-only run path.
            None,
            async move {
                let _ = shutdown_rx.await;
            },
        )
        .await
        .expect("bind + serve should start");
        assert!(addr.is_ipv6(), "CLI control plane must bind IPv6 loopback");

        // A genuine client hits the unauthenticated OpenAPI document (the control
        // plane's default `openapi` feature). HTTP/1.0 + close → read to EOF.
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let req = format!(
            "GET /api/v1/openapi.json HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        let response = String::from_utf8_lossy(&buf);
        // Assert the status CODE (the second token), not the protocol version —
        // hyper may answer an HTTP/1.0 request as 1.0 or 1.1.
        let status_line = response.lines().next().unwrap_or_default();
        assert_eq!(
            status_line.split_whitespace().nth(1),
            Some("200"),
            "expected a 200 status code, got status line: {status_line:?}"
        );
        assert!(
            response.contains("openapi"),
            "expected an OpenAPI document in the body"
        );

        // Graceful shutdown returns cleanly within a generous bound.
        shutdown_tx.send(()).unwrap();
        let joined = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("serve should return within 5s of shutdown");
        joined
            .expect("serve task panicked")
            .expect("serve returned an I/O error");
    }

    /// ADR-W024 MAJOR-C1 (startup TOCTOU): for a config-file run (a boot model
    /// is wired) `bind_and_serve` must install the config-file watch handle into
    /// the served `AppState` BEFORE it starts serving — so a
    /// `POST /config/promote` arriving with the very first request already finds
    /// the suppression seam, and the promoted file is never later re-applied as
    /// an external edit. The returned state (the one the router serves) must
    /// therefore already carry a watch handle.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bind_and_serve_installs_the_watch_handle_before_serving() {
        let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
        let (commands, _rx) = multiview_control::command_bus(8);
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let config = test_config();
        let boot_model = Arc::new(multiview_control::boot_model::BootModel::new(
            std::path::PathBuf::from("/nonexistent/multiview.toml"),
            config.clone(),
            config.clone(),
            multiview_config::StartMode::Boot,
            false,
            None,
        ));

        let (_addr, handle, state) = bind_and_serve(
            "[::1]:0",
            &config,
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
            multiview_control::LiveApplyCaps::default(),
            multiview_control::LiveSourceCapability::synthetic_only(),
            // A config-file run: the watch handle MUST be installed before serving.
            Some(boot_model),
            None,
            async move {
                let _ = shutdown_rx.await;
            },
        )
        .await
        .expect("bind + serve should start");

        assert!(
            state.watch_handle().is_some(),
            "a config-file run must have the watch handle installed before the \
             router serves a single request (no startup TOCTOU for promote)"
        );

        shutdown_tx.send(()).unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
    }

    /// Review H1/H2: the real boot path restores accepted names and versions.
    /// Reseeding here would restart every `ETag` at 1, so a stale tag could match.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bind_and_serve_restores_accepted_names_and_versions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let boot_path = dir.path().join("multiview.toml");
        let mut config = config_with_hls_outputs(&[("out-a", "/tmp/restore-accepted.m3u8")]);
        std::fs::write(&boot_path, config.to_toml().expect("render boot")).expect("boot file");
        config.overlays = vec![serde_json::from_value(serde_json::json!({
            "id": "ov_clock", "kind": "clock", "target": "canvas"
        }))
        .expect("overlay")];
        let body = serde_json::json!({
            "canvas": config.canvas, "layout": config.layout, "cells": config.cells,
        });
        let desired = multiview_control::boot_model::DesiredState {
            version: 1,
            selected_layout_id: "working".to_owned(),
            layouts: vec![multiview_control::Layout {
                id: "working".to_owned(),
                name: "working".to_owned(),
                body: body.clone(),
            }],
            selected_body: body,
            output_ids: (0..config.outputs.len())
                .map(|index| format!("output-{index}"))
                .collect(),
            layout_versions: std::collections::BTreeMap::new(),
            resource_versions: [(
                "overlays".to_owned(),
                [("ov_clock".to_owned(), 7_u64)].into_iter().collect(),
            )]
            .into_iter()
            .collect(),
            resource_names: [(
                "overlays".to_owned(),
                [("ov_clock".to_owned(), "Studio clock".to_owned())]
                    .into_iter()
                    .collect(),
            )]
            .into_iter()
            .collect(),
            config: config.clone(),
        };
        desired.validate().expect("valid envelope");
        let state_dir = dir.path().join(".multiview");
        std::fs::create_dir_all(&state_dir).expect("state dir");
        std::fs::write(
            state_dir.join("desired.json"),
            serde_json::to_vec(&desired).expect("serialize"),
        )
        .expect("write desired");
        let boot_model = Arc::new(multiview_control::boot_model::BootModel::new(
            boot_path,
            config.clone(),
            config.clone(),
            multiview_config::StartMode::Resume,
            true,
            None,
        ));
        let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
        let (commands, _rx) = multiview_control::command_bus(8);
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let (_addr, handle, state) = bind_and_serve(
            "[::1]:0",
            &config,
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
            multiview_control::LiveApplyCaps::default(),
            multiview_control::LiveSourceCapability::synthetic_only(),
            Some(boot_model),
            None,
            async move {
                let _ = shutdown_rx.await;
            },
        )
        .await
        .expect("bind + serve should start");
        let overlay = state.overlays.get("ov_clock").expect("restored overlay");
        assert_eq!(overlay.version.get(), 7);
        assert_eq!(overlay.resource.name, "Studio clock");
        shutdown_tx.send(()).unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
    }

    /// Build a config carrying one HLS output per `(id, path)` pair (the rest of
    /// the canvas/layout/source/cell scaffolding is fixed and valid).
    fn config_with_hls_outputs(outputs: &[(&str, &str)]) -> MultiviewConfig {
        use std::fmt::Write as _;
        let mut doc = String::from(
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
id = "in_a"
kind = "rtsp"
url = "rtsp://x/a"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
"##,
        );
        for (id, path) in outputs {
            // Escape backslashes/quotes are unnecessary for these test ids/paths.
            let _ = write!(
                doc,
                "[[outputs]]\nkind = \"hls\"\nid = \"{id}\"\npath = \"{path}\"\ncodec = \"h264\"\n"
            );
        }
        MultiviewConfig::load_from_toml(&doc).expect("parse HLS-outputs config")
    }

    #[test]
    fn cast_uses_master_for_selected_tracks_and_keeps_legacy_program_url() {
        let base = config_with_hls_outputs(&[("program", "/srv/hls/index.m3u8")]);
        let mut doc = base.to_toml().expect("serialize config");
        doc.push_str("\n[control]\nlisten = \"[::]:8080\"\ncast_media_base = \"http://[2001:db8::1]:8080\"\n[audio]\nsample_rate_hz = 48000\n[[audio.routes]]\ninput_id = \"in_a\"\nchannels = { kind = \"stereo\" }\ntarget_track = \"clean\"\n");
        let legacy = MultiviewConfig::load_from_toml(&doc).expect("legacy config");
        let url = cast_delivery(&legacy)
            .expect("cast delivery")
            .for_output("program")
            .expect("output")
            .url
            .clone();
        assert!(url.ends_with("/index.m3u8"), "{url}");
        doc.push_str("\n[outputs.audio]\nmode = \"auto\"\n");
        let selected = MultiviewConfig::load_from_toml(&doc).expect("selected config");
        let url = cast_delivery(&selected)
            .expect("cast delivery")
            .for_output("program")
            .expect("output")
            .url
            .clone();
        assert!(url.ends_with("/master.m3u8"), "{url}");
    }

    /// Two outputs whose **distinct** ids sanitise to the SAME URL segment get
    /// **distinct** mounts: the first keeps the base segment, the second is
    /// deduped with a deterministic `-2` suffix, so every output stays reachable.
    #[test]
    fn colliding_sanitised_mounts_are_deduplicated() {
        // `aux/out` → `aux-out` and `aux out` → `aux-out` collide post-sanitise.
        let config = config_with_hls_outputs(&[
            ("aux/out", "/tmp/a/multiview.m3u8"),
            ("aux out", "/tmp/b/multiview.m3u8"),
        ]);
        let mounts = hls_mounts(&config);
        assert_eq!(mounts.len(), 2, "both outputs must mount");
        assert_eq!(mounts[0].route, "/hls/aux-out");
        assert_eq!(
            mounts[1].route, "/hls/aux-out-2",
            "the colliding second id must dedupe with a -2 suffix, got {:?}",
            mounts[1].route
        );
        assert_ne!(
            mounts[0].route, mounts[1].route,
            "deduped mounts must be distinct"
        );
    }

    /// A THIRD collision continues the deterministic suffix sequence (`-2`, `-3`).
    #[test]
    fn three_way_collision_deduplicates_2_then_3() {
        // `a/b`, `a b`, and `a!b` all sanitise to the same `a-b` segment.
        let config = config_with_hls_outputs(&[
            ("a/b", "/tmp/a/multiview.m3u8"),
            ("a b", "/tmp/b/multiview.m3u8"),
            ("a!b", "/tmp/c/multiview.m3u8"),
        ]);
        let routes: Vec<String> = hls_mounts(&config).into_iter().map(|m| m.route).collect();
        assert_eq!(
            routes,
            vec![
                "/hls/a-b".to_owned(),
                "/hls/a-b-2".to_owned(),
                "/hls/a-b-3".to_owned(),
            ]
        );
    }

    /// An id of `..`, the empty string, or all-dots (`...`) is not a usable URL
    /// path segment and maps to the `out` fallback.
    #[test]
    fn unusable_ids_fall_back_to_out() {
        for id in ["..", "", "..."] {
            assert_eq!(
                multiview_config::hls_mount_segment(id),
                "out",
                "id {id:?} must map to the `out` fallback"
            );
        }
    }

    /// The `out` fallback ALSO participates in dedupe: two outputs whose ids
    /// both collapse to `out` get `/hls/out` and `/hls/out-2`.
    #[test]
    fn colliding_out_fallbacks_are_deduplicated() {
        let config = config_with_hls_outputs(&[
            ("..", "/tmp/a/multiview.m3u8"),
            ("...", "/tmp/b/multiview.m3u8"),
        ]);
        let routes: Vec<String> = hls_mounts(&config).into_iter().map(|m| m.route).collect();
        assert_eq!(routes, vec!["/hls/out".to_owned(), "/hls/out-2".to_owned()]);
    }

    /// A normal alphanumeric id (with the kept `-`/`_`/`.`/`~` set) passes
    /// through unchanged — sanitisation never mangles already-safe segments.
    #[test]
    fn already_safe_ids_pass_through_unchanged() {
        for id in ["program", "low-latency_1.0~alt", "ABC123"] {
            assert_eq!(multiview_config::hls_mount_segment(id), id);
        }
    }
}

/// Regression tests for the push-output link fragment (issue #202): every
/// non-delivery is attributed to its own reason, and a peer session ending is
/// history — never current link health.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod push_links_tests {
    use std::sync::Arc;

    use super::push_links_fragment;
    use multiview_output::push_link::PushLinkStatus;

    #[test]
    fn every_non_delivery_is_attributed_and_a_session_end_is_history() {
        let link = Arc::new(PushLinkStatus::new("srt", true, "srt://[::]:9000"));
        link.discard_no_client();
        link.discard_no_client();
        link.discard_awaiting_keyframe();
        link.drop_slow_peer();
        link.set_session_error("Input/output error");

        let links = vec![("srt-main".to_owned(), Arc::clone(&link))];
        let fragment = push_links_fragment(&links).expect("a non-empty fragment");
        let out = &fragment["srt-main"]["link"];
        assert_eq!(out["discarded_no_client"], 2, "waiting for a caller: {out}");
        assert_eq!(out["discarded_awaiting_keyframe"], 1, "{out}");
        assert_eq!(out["dropped_slow_peer"], 1, "{out}");
        assert_eq!(out["dropped_upstream"], 0, "{out}");
        assert_eq!(
            out["dropped"], 4,
            "the total is the sum of the reasons: {out}"
        );
        assert_eq!(
            out["last_session_error"], "Input/output error",
            "a session ending is carried as history: {out}"
        );
        assert!(
            out.get("last_error").is_none(),
            "a session ending is NOT current link health: {out}"
        );
        assert_eq!(out["state"], "idle", "the fragment is a pure projection");
    }

    #[test]
    fn a_link_error_is_current_health_while_a_session_end_is_not() {
        let link = Arc::new(PushLinkStatus::new("srt", true, "srt://[::]:9000"));
        link.set_error("bind failed");
        let links = vec![("srt-main".to_owned(), Arc::clone(&link))];
        let fragment = push_links_fragment(&links).expect("fragment");
        let out = &fragment["srt-main"]["link"];
        assert_eq!(out["last_error"], "bind failed", "{out}");
        assert!(
            out.get("last_session_error").is_none(),
            "no session has ended: {out}"
        );
    }

    #[test]
    fn no_push_outputs_means_no_fragment() {
        assert!(push_links_fragment(&[]).is_none());
    }
}

#[cfg(test)]
mod live_decoded_kind_tests {
    use super::live_decoded_kind;

    #[test]
    fn youtube_is_live_exactly_when_the_build_resolves_it() {
        let yt: multiview_config::SourceKind = serde_json::from_value(
            serde_json::json!({"kind": "youtube", "url": "https://youtu.be/x"}),
        )
        .expect("youtube kind");
        assert_eq!(live_decoded_kind(&yt), cfg!(feature = "youtube"));
        let ndi: multiview_config::SourceKind =
            serde_json::from_value(serde_json::json!({"kind": "ndi", "name": "CAM"}))
                .expect("ndi kind");
        assert!(!live_decoded_kind(&ndi));
    }
}
