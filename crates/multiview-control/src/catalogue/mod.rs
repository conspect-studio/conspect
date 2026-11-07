//! IPTV-ORG source browser backend (ADR-M015, `docs/io/iptv-org.md`).
//!
//! * [`model`] / [`index`] / [`gazetteer`] — the upstream model, the joined index
//!   (one row per stream) and the place gazetteer (pure, always compiled).
//! * [`store`] / [`sync`] / [`service`] — atomic snapshot persistence, conditional-GET
//!   sync, offline import and the shared [`service::Catalogue`] handle.
//! * [`links`] — the per-source link tracker (slice D).
//! * `guard` / `net` / `probe` — network egress (feature `catalogue`): the
//!   screened dial guard, the fixed-host sync client and the deep probe.
//!
//! Everything runs on the control plane with bounded concurrency/memory/time. None
//! of it touches the output clock (invariant #1) or makes the engine await a
//! client (invariant #10).

pub mod gazetteer;
pub mod index;
pub mod links;
pub mod model;
pub mod netdiag;

/// A shared network-diagnostics collector.
pub type NetworkCollectorHandle = std::sync::Arc<dyn netdiag::NetworkCollector>;
pub mod service;
pub mod store;
pub mod sync;

#[cfg(feature = "catalogue")]
pub mod guard;
#[cfg(feature = "catalogue")]
pub mod net;
pub mod probe;

pub use service::Catalogue;

pub mod link_runtime;
#[cfg(feature = "catalogue")]
pub mod logos;

/// Without the `catalogue` feature there is no network client: logos answer honestly.
#[cfg(not(feature = "catalogue"))]
pub mod logos {
    use crate::state::AppState;

    /// A cached logo image (never produced in this build).
    #[derive(Debug, PartialEq, Eq)]
    pub struct Image {
        /// The image bytes.
        pub bytes: Vec<u8>,
        /// Its content type.
        pub content_type: String,
        /// Strong `ETag`.
        pub etag: String,
    }

    /// The answer for one logo request.
    #[derive(Debug, Clone)]
    pub enum Logo {
        /// The image.
        Image(std::sync::Arc<Image>),
        /// Known missing.
        Missing,
        /// No fetch slot in time.
        Busy,
    }

    /// Always missing: this build has no catalogue network I/O.
    #[allow(clippy::unused_async)]
    pub async fn serve(_: &AppState, _: &std::path::Path, _: &str) -> Logo {
        Logo::Missing
    }
}

use crate::state::AppState;

/// All accepted source documents, parsed (system view, not caller-scoped; only for
/// server-side bookkeeping — never returned to a caller unfiltered).
#[must_use]
pub fn accepted_source_docs(state: &AppState) -> Vec<multiview_config::Source> {
    crate::routes::sources::accepted_sources_system(state)
}

/// Apply a link [`links::Change`] to `source_id` through the normal If-Match write
/// path (the current version is read under the same config-mutation lock), audited
/// as `actor`.
///
/// # Errors
///
/// A message when the source vanished, the change does not validate, or the write
/// is rejected.
pub async fn apply_source_change(
    state: &AppState,
    source_id: &str,
    change: &links::Change,
    actor: &str,
) -> Result<(), String> {
    let _guard = state.lock_config_mutation().await;
    let current = crate::routes::sources::accepted_sources_system_versioned(state, source_id)
        .ok_or_else(|| format!("source {source_id:?} not found"))?;
    let source: multiview_config::Source =
        serde_json::from_value(current.resource.body.clone()).map_err(|e| e.to_string())?;
    let next = links::apply_change(&source, change, &store::now_rfc3339())?;
    let body = serde_json::to_value(&next).map_err(|e| e.to_string())?;
    let input = crate::resource_store::ResourceInput {
        name: current.resource.name.clone(),
        body,
    };
    let version = current.version;
    let if_match = crate::concurrency::IfMatch {
        expected: Some(version),
        raw: Some(version.to_etag()),
    };
    crate::routes::sources::apply_source_update(state, actor, source_id, Some(&if_match), input)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Wire the catalogue + probe services into `state` and spawn the sync loop
/// (feature `catalogue`). `boot_path` anchors `<config-dir>/.multiview/catalogues/`;
/// without one (no config file) the snapshot lives under `./.multiview/`.
///
/// A disabled catalogue still installs the (inert) services so every route answers
/// an honest "disabled" instead of a missing-route 404.
#[cfg(feature = "catalogue")]
#[must_use]
pub fn install(
    state: AppState,
    cfg: &multiview_config::IptvOrgConfig,
    boot_path: Option<&std::path::Path>,
    host: String,
    egress_lookup: bool,
) -> AppState {
    use std::sync::Arc;
    let dir =
        service::catalogue_dir(boot_path.unwrap_or_else(|| std::path::Path::new("multiview.toml")));
    let fetcher: Arc<dyn sync::Fetcher> =
        Arc::new(net::UpstreamFetcher::new(Arc::clone(&state.dial_policy)));
    let catalogue = Catalogue::new(dir, cfg, fetcher);
    let guard = guard::DialGuard::new(Arc::clone(&state.dial_policy));
    let collector: Arc<dyn netdiag::NetworkCollector> = Arc::new(netdiag::collect::Collector::new(
        guard.clone(),
        egress_lookup,
    ));
    let runner: Arc<dyn probe::ProbeRunner> = match probe::engine::HttpProbeRunner::new(guard) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            tracing::warn!(error = %e, "iptv-org probe client unavailable");
            Arc::new(probe::UnavailableRunner)
        }
    };
    let probes = probe::ProbeService::with_network(runner, host, collector);
    let state = state.with_catalogue(Arc::clone(&catalogue), probes);
    if catalogue.enabled() {
        spawn_sync_loop(state.clone(), Arc::clone(&catalogue));
        if let Some(every) = cfg.probe_linked_every {
            spawn_probe_linked_loop(state.clone(), every.as_duration());
        }
    }
    state
}

/// Most linked sources probed per round (the probe queue bound is 64).
#[cfg(feature = "catalogue")]
const PROBE_LINKED_PER_ROUND: usize = 32;

/// One round of `probe_linked_every`: submit a probe for each iptv-org-linked
/// source through the normal [`probe::ProbeService::submit`] (same semaphore of 4,
/// queue of 64, 15-minute cache and per-principal rate as the browser's probes).
/// Stops at the first refusal (rate or queue full) rather than queueing more.
/// Returns how many probes were admitted.
#[cfg(feature = "catalogue")]
#[must_use]
pub fn probe_linked_round(state: &AppState) -> usize {
    let Some(svc) = state.catalogue_probe.as_ref() else {
        return 0;
    };
    let mut admitted = 0;
    for source in crate::routes::sources::accepted_sources_system(state)
        .into_iter()
        .filter(|s| {
            s.origin
                .as_ref()
                .is_some_and(|o| o.provider == links::PROVIDER)
        })
        .take(PROBE_LINKED_PER_ROUND)
    {
        let Some(url) = links::source_url(&source) else {
            continue;
        };
        let (user_agent, referrer) = links::source_headers(&source);
        let req = probe::ProbeRequest {
            url,
            user_agent,
            referrer,
            quality: None,
            channel_country: None,
        };
        if svc.submit("system:catalogue-probe", req).is_err() {
            break;
        }
        admitted += 1;
    }
    admitted
}

/// The `probe_linked_every` runner: off unless configured (min 5 min, validated
/// in the config), first round one period after boot, no catch-up bursts.
#[cfg(feature = "catalogue")]
fn spawn_probe_linked_loop(state: AppState, every: std::time::Duration) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await; // the immediate first tick
        loop {
            tick.tick().await;
            let n = probe_linked_round(&state);
            tracing::debug!(admitted = n, "iptv-org linked-source probe round");
        }
    });
}

/// Jittered periodic sync: first run shortly after boot when nothing is persisted
/// (or the snapshot is older than one interval), then every `interval ± 10 %`.
#[cfg(feature = "catalogue")]
fn spawn_sync_loop(state: AppState, catalogue: std::sync::Arc<Catalogue>) {
    use std::time::Duration;
    tokio::spawn(async move {
        // Startup: recompute link status from whatever was persisted.
        link_runtime::recompute_and_publish(&state, &catalogue).await;
        let interval = catalogue.interval();
        let mut first = true;
        loop {
            let jitter_ms = {
                let base = u64::try_from(interval.as_millis() / 10).unwrap_or(0).max(1);
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| u64::from(d.subsec_nanos()));
                nanos % (base * 2)
            };
            let wait = if first && catalogue.snapshot().is_none() {
                Duration::from_secs(20)
            } else {
                interval.saturating_sub(Duration::from_millis(jitter_ms / 2))
                    + Duration::from_millis(jitter_ms / 2)
            };
            first = false;
            tokio::time::sleep(wait).await;
            match catalogue.refresh().await {
                Ok(outcome) => {
                    tracing::info!(?outcome, "iptv-org catalogue sync finished");
                    link_runtime::recompute_and_publish(&state, &catalogue).await;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "iptv-org catalogue sync failed; keeping the previous snapshot");
                }
            }
        }
    });
}
