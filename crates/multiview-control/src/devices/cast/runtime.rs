//! The cast driver's spawn factory (DEV-D2, ADR-M011): plugs the
//! [`CastSessionActor`] into the **same**
//! [`DevicePollerRegistry`](crate::devices::DevicePollerRegistry) / factory /
//! tombstone machinery the zowietek driver established (DEV-A4) — one
//! runtime, every driver.
//!
//! The factory manages `driver = cast` devices only. What a device casts is
//! its `display.assign`: `{ output = "out-…" }` names a served HLS rendition,
//! `{ program = true }` casts the first declared rendition (every HLS output
//! is a rendition of the program canvas). A cast device with no assignment —
//! or a wall-head assignment, which is not an HLS rendition (ADR-M011) — gets
//! **no** session actor: the device record rides `ADOPTING` honestly and a
//! warning names the missing rendition, mirroring the zowietek
//! missing-address posture.

use std::sync::Arc;

use multiview_config::{Device, DeviceDriver};

use super::media::{split_authority, CastDelivery};
use super::session::{CastConnector, CastSessionActor, CastSessionConfig};
use crate::devices::zowietek::poller::PollerHandle;
use crate::devices::zowietek::runtime::{DevicePollerFactory, PollerWiring};

/// Spawns a supervised [`CastSessionActor`] per `driver = cast` device,
/// dialling through the shared connector and resolving the rendition from
/// the [`CastDelivery`] map the binary built (DEV-D1 mounts × the validated
/// `control.cast_media_base`).
pub struct CastSessionFactory<C: CastConnector + 'static> {
    connector: Arc<C>,
    delivery: Arc<CastDelivery>,
    config: CastSessionConfig,
}

impl<C: CastConnector + 'static> CastSessionFactory<C> {
    /// Build a factory over `connector` and the `delivery` map, spawning
    /// actors with `config` timings.
    #[must_use]
    pub fn new(connector: Arc<C>, delivery: Arc<CastDelivery>, config: CastSessionConfig) -> Self {
        Self {
            connector,
            delivery,
            config,
        }
    }
}

impl<C: CastConnector + 'static> DevicePollerFactory for CastSessionFactory<C> {
    fn spawn(&self, device: &Device, wiring: &PollerWiring) -> Option<PollerHandle> {
        // Only `cast` devices are ours.
        if device.driver != DeviceDriver::Cast {
            return None;
        }
        // A cast device requires an authority (config validation enforces the
        // address; the split rejects malformed ports rather than guessing).
        let address = device.address.as_deref()?;
        let Some((_host, _port)) = split_authority(address) else {
            tracing::warn!(
                device = %device.id,
                address = %address,
                "cast device address is not a valid host[:port]; no session spawned"
            );
            return None;
        };
        // Resolve what to cast from the display assignment (ADR-M011: the
        // media path is an existing HLS rendition).
        let Some(assign) = device.display.as_ref().map(|d| &d.assign) else {
            tracing::warn!(
                device = %device.id,
                "cast device has no display assignment (nothing to cast); declare \
                 display.assign = {{ output = \"…\" }} or {{ program = true }} — no session \
                 spawned, the device rides ADOPTING"
            );
            return None;
        };
        let Some(target) = self.delivery.resolve_assign(assign) else {
            tracing::warn!(
                device = %device.id,
                assign = ?assign,
                "cast device assignment does not resolve to a served HLS rendition (a wall \
                 head is not a rendition; an output must be a configured HLS/LL-HLS output \
                 and control.cast_media_base must be set) — no session spawned"
            );
            return None;
        };
        // The started-at stamp (DEV-D3.1): at the actor's LOAD-accept moment
        // (the receiver created our media session — the cast verifiably began)
        // stamp the ephemeral session record from the control plane's
        // injectable clock. First-write-wins in the store; a no-op for a
        // SAVED cast device's id (no ephemeral record to stamp).
        let stamp_store = Arc::clone(&wiring.cast_sessions);
        let stamp_clock = Arc::clone(&wiring.clock);
        let stamp_id = device.id.clone();
        let actor = CastSessionActor::new(
            &device.id,
            Arc::clone(&self.connector),
            address,
            target.clone(),
            wiring.broadcaster.clone(),
            self.config,
        )
        .with_load_accepted_hook(Arc::new(move || {
            stamp_store.mark_started(&stamp_id, (stamp_clock)().as_nanos());
        }));
        // Spawn inside the ambient Tokio runtime when there is one — the
        // running control plane (axum handlers, boot seeding) always has one.
        // A synchronous caller (no ambient runtime: the factory unit tests, a
        // sync embedding) enters the shared fallback runtime instead, so the
        // returned handle always carries a RUNNING actor.
        if tokio::runtime::Handle::try_current().is_ok() {
            Some(actor.spawn())
        } else {
            let runtime = fallback_runtime()?;
            let _guard = runtime.enter();
            Some(actor.spawn())
        }
    }
}

/// The shared fallback runtime for [`CastSessionFactory::spawn`] calls issued
/// **outside** any ambient Tokio runtime: one lazily-built single-worker
/// runtime per process whose worker thread drives the session actors spawned
/// through it (a [`tokio::runtime::Runtime`] runs its spawned tasks on its
/// worker threads without needing a `block_on`). The running control plane
/// never builds this — every spawn there happens inside its own runtime.
/// Returns [`None`] (with one warning) only when the OS refuses the
/// runtime's thread/IO resources; the factory then spawns nothing and the
/// device honestly rides `ADOPTING`.
fn fallback_runtime() -> Option<&'static tokio::runtime::Runtime> {
    static FALLBACK: std::sync::OnceLock<Option<tokio::runtime::Runtime>> =
        std::sync::OnceLock::new();
    FALLBACK
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("cast-session-fallback")
                .enable_all()
                .build()
                .map_err(|e| {
                    tracing::warn!(
                        error = %e,
                        "the cast fallback runtime did not build; no session actor can be \
                         spawned outside a Tokio runtime"
                    );
                })
                .ok()
        })
        .as_ref()
}
