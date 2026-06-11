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
/// # Errors
/// Returns an I/O error from binding the `listen` address, or — wrapped as
/// [`std::io::ErrorKind::InvalidData`] — a failure to seed the resource stores
/// from `config` (not expected for a validated config).
pub async fn bind_and_serve<F>(
    listen: &str,
    config: &MultiviewConfig,
    publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    commands: CommandSender,
    preview: SharedPreview,
    licence: Option<multiview_control::LicenceState>,
    shutdown: F,
) -> std::io::Result<(SocketAddr, JoinHandle<std::io::Result<()>>)>
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
    let (api_keys, bootstrap_token) = provision_admin_keys(admin_secret);
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

    // Optional, explicit, opt-in auth-disable for trusted/local deployments.
    // `MULTIVIEW_CONTROL_AUTH=disabled|off|none|0` opens the whole API + WS as a
    // local admin (no token). Secure default: anything else (incl. unset) keeps
    // auth ON. A loud warning is logged whenever it is off.
    let auth_disabled = std::env::var("MULTIVIEW_CONTROL_AUTH")
        .ok()
        .is_some_and(|v| {
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

    let state = AppState::new(
        publisher,
        commands,
        Arc::new(InMemoryRepository::new()),
        Arc::new(api_keys),
    )
    .with_seeded_resources(seeded)
    .with_base_document(
        serde_json::to_value(config)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?,
    )
    .with_preview(preview)
    .with_warning_store(warnings)
    .with_device_pollers(device_poller_registry())
    .with_auth_disabled(auth_disabled)
    // The `[discovery]` browse configuration: the operator-configured
    // zowietek-control service type (the vendor's type is unverified — only a
    // configured string is ever recognised) plus any extra DNS-SD types.
    .with_discovery_config(config.discovery.clone().unwrap_or_default());

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

    // Mount each configured HLS/LL-HLS output's delivery surface under
    // `/hls/{output-id}/` (DEV-D1): the ADR-0032 §6 router serving that
    // output's playlist/segment/init files with the Cache-Control tiers,
    // Range/206, and Origin-reflecting CORS — so a Cast receiver (a browser
    // app on a Google origin) or any browser player fetches cross-origin
    // straight off this listener. Deliberately OUTSIDE `/api/v1`, so it is
    // unauthenticated like `/docs` (media devices cannot send Bearer tokens).
    // Isolation-safe (inv #10): the handlers only read files the segmenter
    // already published to disk — never an engine channel or lock.
    let mut app = multiview_control::router(state);
    for mount in hls_mounts(config) {
        app = app.nest(
            &mount.route,
            multiview_output::hls::http::hls_router(mount.dir),
        );
    }
    let handle = tokio::spawn(multiview_control::serve_router(listener, app, shutdown));
    Ok((addr, handle))
}

/// Build the runtime device-poller registry for the control plane (DEV-A4).
///
/// With the `devices-net` feature on (which forwards `multiview-control/zowietek`),
/// the registry carries the reqwest-backed
/// [`ReqwestPollerFactory`](multiview_control::devices::ReqwestPollerFactory) so
/// boot-seed/adopt spawn a **live** supervised poller per `zowietek` device,
/// resolving each device's credentials from its `auth.secret_ref` via
/// [`resolve_device_credentials`]. Without the feature it is the default no-op
/// registry (no live transport → no poller spawned; the projection routes stay
/// honestly empty), so the default build pulls no socket.
#[cfg(feature = "devices-net")]
fn device_poller_registry() -> Arc<multiview_control::devices::DevicePollerRegistry> {
    use multiview_control::devices::{DevicePollerRegistry, ReqwestPollerFactory};
    // A 5s per-request timeout: generous for a LAN appliance, bounded so a hung
    // device times out into the supervised-reconnect path rather than wedging
    // the poller task.
    let factory = ReqwestPollerFactory::new(
        std::time::Duration::from_secs(5),
        resolve_device_credentials,
    );
    Arc::new(DevicePollerRegistry::with_factory(Arc::new(factory)))
}

/// The default no-op poller registry (no `zowietek` feature): no live device
/// transport, so no poller is spawned and the projection routes stay honestly
/// empty — exactly the pre-DEV-A4 behaviour.
#[cfg(not(feature = "devices-net"))]
fn device_poller_registry() -> Arc<multiview_control::devices::DevicePollerRegistry> {
    Arc::new(multiview_control::devices::DevicePollerRegistry::new())
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
/// (the configured playlist's parent — where the segmenter writes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsMount {
    /// Route prefix, e.g. `/hls/program`.
    pub route: String,
    /// The served directory (the output playlist's parent directory).
    pub dir: std::path::PathBuf,
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
#[must_use]
pub fn hls_mounts(config: &MultiviewConfig) -> Vec<HlsMount> {
    use multiview_config::Output;
    let mut taken = std::collections::HashSet::new();
    let mut mounts = Vec::new();
    for output in &config.outputs {
        let (Output::Hls { path, .. } | Output::LlHls { path, .. }) = output else {
            continue;
        };
        let dir = match std::path::Path::new(path).parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            // A bare filename (or a root path): serve the process working dir
            // (where such a playlist would be written).
            _ => std::path::PathBuf::from("."),
        };
        let mut segment = sanitize_mount_segment(&output.id());
        if !taken.insert(segment.clone()) {
            // Deterministic suffix dedupe; bounded by the output count.
            let mut n: u32 = 2;
            segment = loop {
                let candidate = format!("{segment}-{n}");
                if taken.insert(candidate.clone()) {
                    break candidate;
                }
                n = n.saturating_add(1);
            };
        }
        mounts.push(HlsMount {
            route: format!("/hls/{segment}"),
            dir,
        });
    }
    mounts
}

/// Map an output id to a URL-segment-safe mount name (see [`hls_mounts`]).
fn sanitize_mount_segment(id: &str) -> String {
    let segment: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
                c
            } else {
                '-'
            }
        })
        .collect();
    // `.`/`..`/empty are not usable URL path segments.
    if segment.is_empty() || segment.chars().all(|c| c == '.') {
        "out".to_owned()
    } else {
        segment
    }
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
    let tiles: Vec<serde_json::Value> = entries
        .into_iter()
        .map(|(id, state)| {
            // `LifecycleState` is a plain unit-variant enum: serialising it is
            // infallible in practice; the guardrails forbid `unwrap`, so a
            // (never-occurring) fault degrades to a `null` state the control
            // plane skips rather than panicking on the publish path.
            let state = serde_json::to_value(multiview_events::LifecycleState::from(state))
                .unwrap_or(serde_json::Value::Null);
            serde_json::json!({ "id": id, "state": state })
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
/// * [`Command::RouteAudio`] desugars to [`RouteIntent::Audio`] but the run path
///   wires **no per-source audio crosspoint** yet (program audio is silence —
///   there is no per-source `AudioStore` to re-point onto, the run-side audio
///   ingest is RT-5/RT-8b, unbuilt). It is therefore a **surfaced** held action
///   (`tracing::warn!` naming the missing crosspoint), never a silent drop.
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
    let mut drain = CommandDrain::new(commands, config, publisher).with_live_sources(live);
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
/// is a lock-free `ArcSwapOption` load and publishing a re-point is wait-free +
/// bounded drop-oldest, so neither can pace or stall the output clock
/// (invariants #1/#10).
///
/// The binary wires this on the full libav\* path (`run_pipeline_until_ctrl_c`),
/// where the pipeline has a subtitle router; the software-engine path (no subtitle
/// rendering) wires the plain [`command_drain`].
#[cfg(all(feature = "ffmpeg", feature = "overlay"))]
pub fn command_drain_with_seams(
    commands: CommandReceiver,
    config: MultiviewConfig,
    publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    subtitle_route: Arc<arc_swap::ArcSwapOption<crate::captions::SubtitleRouteHandle>>,
    live: crate::live_sources::LiveSourceHandle,
) -> impl FnMut(&mut CompositorDrive<Nv12Image>) {
    let mut drain = CommandDrain::new(commands, config, publisher)
        .with_subtitle_route(subtitle_route)
        .with_live_sources(live);
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
/// * **audio** → a surfaced held action (the run wires no per-source audio
///   crosspoint yet — RT-5/RT-8b), never a silent drop.
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
    subtitle_route: Option<Arc<arc_swap::ArcSwapOption<crate::captions::SubtitleRouteHandle>>>,
    /// The live-source producer seam (ADR-W018), when wired
    /// ([`command_drain_with_live_sources`] / [`command_drain_with_seams`]): the
    /// bounded, non-blocking handle to the off-thread
    /// [`LiveSourceHub`](crate::live_sources::LiveSourceHub) that owns producer
    /// spawn/teardown + the preview registry. `None` ⇒ `UpsertSource`/
    /// `RemoveSource` are surfaced held actions (never a silent drop).
    live_sources: Option<crate::live_sources::LiveSourceHandle>,
    /// One-shot: the drive's cell-id → index map is established the first tick.
    cell_ids_set: bool,
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
        Self {
            commands,
            config,
            publisher,
            state: DrainState::default(),
            pending: std::collections::VecDeque::new(),
            resolution: RouteResolution::default(),
            #[cfg(all(feature = "ffmpeg", feature = "overlay"))]
            subtitle_route: None,
            live_sources: None,
            cell_ids_set: false,
            #[cfg(test)]
            resolve_spy: None,
        }
    }

    /// Thread in the live-source producer seam (ADR-W018) so
    /// `UpsertSource`/`RemoveSource` reach the running engine. See
    /// [`command_drain_with_live_sources`].
    #[must_use]
    fn with_live_sources(mut self, live: crate::live_sources::LiveSourceHandle) -> Self {
        self.live_sources = Some(live);
        self
    }

    /// Thread in the live run-side subtitle re-point seam (RT-10b) so a
    /// `RouteSubtitle` reaches the running pipeline's layer. See
    /// [`command_drain_with_seams`].
    #[cfg(all(feature = "ffmpeg", feature = "overlay"))]
    #[must_use]
    fn with_subtitle_route(
        mut self,
        subtitle_route: Arc<arc_swap::ArcSwapOption<crate::captions::SubtitleRouteHandle>>,
    ) -> Self {
        self.subtitle_route = Some(subtitle_route);
        self
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
    /// Never blocks, never awaits — it drains a non-blocking queue (O(pending)),
    /// applies O(1) re-points, and publishes drop-oldest events, so the output
    /// clock is never stalled by control (invariants #1 + #10).
    pub fn apply(&mut self, drive: &mut CompositorDrive<Nv12Image>) -> usize {
        // First tick: hand the drive the cell ids (in config-cell order, which is
        // exactly `solve_layout`'s core-cell order) so `rebind_cell` can address
        // cells by id, and the per-cell `on_loss` failover-slate policy (ADR-0027
        // / ADR-0030) so a down tile composites the slate its config declares.
        // One-shot, off the hot composite.
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
        for command in self.commands.try_drain() {
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
            let mut route_applier = RouteApplier::new(&self.resolution);
            if let Err(e) = route_applier.apply_video(drive, std::slice::from_ref(&intent)) {
                // An honest route error (unknown cell / source with no store): the
                // binding is held unchanged, logged, never a panic, never a re-solve.
                tracing::warn!(error = %e, "video route held (unknown cell/source)");
            }
            applied = applied.saturating_add(1);
        }
        applied
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
        let cells = resolved.solved.cells.len();
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
            } => {
                // RT-11: `RouteAudio` desugars to `RouteIntent::Audio` and the
                // canonical apply is `RouteApplier::apply_audio` →
                // `ProgramBus::repoint_crossfade`. BUT the run wires **no per-source
                // audio crosspoint** yet: program audio is silence (there is no
                // per-source `AudioStore` to re-point onto), and the program bus is
                // owned off-thread by the bake consumer with no re-point seam. The
                // run-side audio ingest (per-source decode → `AudioStore` → bus
                // registration) is RT-5/RT-8b, not built. Surface the held route
                // loudly — NEVER a silent drop — naming the missing crosspoint.
                tracing::warn!(
                    target = %target,
                    source = ?source,
                    "route_audio held: the run has no per-source audio crosspoint yet \
                     (program audio is silence; per-source audio ingest is RT-5/RT-8b)"
                );
            }
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
                    let working = self.config.solve_layout().ok().map(|l| l.name);
                    if working.as_deref() == Some(layout.as_str()) {
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
            Command::SetTallyOverride { target, color, .. } => {
                // No tally arbiter is wired into the software engine yet, so this
                // emits a TallyState echo rather than silently no-op'ing: a forced
                // colour maps to a program-bus lamp of that colour at the default
                // brightness; a cleared override (`None`) maps to the unlit default.
                // FOLLOW-UP: route through the real arbiter once it exists.
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
            // `Command` is `#[non_exhaustive]`: a future variant this build does not
            // know about is logged and skipped, never panicked on.
            ref other => {
                tracing::warn!(kind = other.kind(), "unhandled control command; skipped");
            }
        }
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
        let Some(kind) = crate::synth::SyntheticKind::from_source_kind(&source.kind) else {
            tracing::warn!(
                source = %source.id,
                "upsert_source held: this kind is not live-appliable yet \
                 (network live-add is the next ADR-W018 slice); it applies on restart"
            );
            return;
        };
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
        // the SAME generator_loop the startup path runs, and RCUs the preview
        // map. Requesting this BEFORE the binding mutations below gives the
        // hub a head start on stopping a replaced producer, shrinking the
        // bounded window in which old and new frames can interleave in the
        // reused store on an edit (ADR-W018 §5).
        match seam.request_spawn_synth(crate::live_sources::SynthSpawn {
            id: id.clone(),
            kind,
            store: Arc::clone(&store),
            width: self.config.canvas.width,
            height: self.config.canvas.height,
            canvas: multiview_compositor::pipeline::CanvasColor::default(),
            cadence: self.config.canvas.fps.rational(),
        }) {
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
    }

    /// Apply a `RouteSubtitle` by driving the run's live subtitle re-point seam
    /// (RT-10b): re-point the layer rendered into `layer` to the cues of the source
    /// `source` resolves to.
    ///
    /// The seam ([`SubtitleRouteHandle`](crate::captions::SubtitleRouteHandle)) is
    /// the thread-safe bridge to the bake consumer's `SubtitleRouter`, which applies
    /// the re-point at its next sample boundary via the engine
    /// [`SubtitleLayer::repoint`](multiview_overlay::SubtitleLayer) (CLEAR-on-switch
    /// at the seam). Publishing is wait-free + bounded drop-oldest, so it can never
    /// pace or stall the output clock (invariants #1/#10). The run's `SubtitleRouter`
    /// keys layers + sources by source id, so the subtitle `StreamRef`'s `input_id`
    /// names the target source (selector resolution to a specific track within a
    /// source is the run-side caption-track work; identity-by-source is today's
    /// per-source caption model).
    #[cfg(all(feature = "ffmpeg", feature = "overlay"))]
    fn route_subtitle(&self, layer: &str, source: &multiview_config::routing::StreamRef) {
        let Some(slot) = self.subtitle_route.as_ref() else {
            tracing::warn!(
                layer = %layer,
                "route_subtitle held: no subtitle route seam wired on this run path"
            );
            return;
        };
        let Some(handle) = slot.load_full() else {
            // The run has not yet published its live handle (it does so at drive
            // start); a route arriving in that tiny window is held, not panicked on.
            tracing::warn!(
                layer = %layer,
                "route_subtitle held: the run has not yet published its subtitle route handle"
            );
            return;
        };
        handle.request_repoint(layer, &source.input_id);
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
        let (addr, handle) = bind_and_serve(
            "[::1]:0",
            &test_config(),
            publisher,
            commands,
            multiview_control::no_preview(),
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
                sanitize_mount_segment(id),
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
            assert_eq!(sanitize_mount_segment(id), id);
        }
    }
}
