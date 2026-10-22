//! The Conspect **entitlement-plane wiring** for the cli (CONSPECT-2b/10,
//! ADR-0050 §3/§5/§7, the brief §13.6).
//!
//! The cli owns the single shared entitlement [`LeaseStore`] and threads its
//! computed state into the engine seams as **data**:
//!
//! * **S1 (startup gate):** the sampled [`EnforcementLevel`] the
//!   [`SoftwareEngine::build_gated`](crate::run::SoftwareEngine::build_gated)
//!   consults *before* building a NEW engine — see [`current_level`].
//! * **S3 (tile watermark):** the wait-free [`WatermarkSignal`] the overlay bake
//!   samples each frame to decide whether to stamp the corner watermark.
//! * The same store backs the control plane's `LicenceState`, so the API, the
//!   chrome, and the engine all read the **same** ladder state — there is no
//!   second opinion (ADR-0050 §2).
//!
//! # Never off air (invariant #1 / #10)
//!
//! Everything here is **data the engine samples**, never control flow on the tick
//! path. The [`WatermarkSignal`] is an `arc_swap`'d [`EnforcementLevel`] read with
//! a single wait-free load — no lock, no allocation, no `.await` — so the overlay
//! bake's per-frame watermark decision cannot pace or stall the output clock. The
//! signal is updated **off-thread** (a background poll of the lease store), never
//! from the hot loop. The startup gate runs only at build time; a running engine
//! is never re-gated (ADR-0050 §6.3).

use std::sync::Arc;

use arc_swap::ArcSwap;
#[cfg(feature = "heartbeat")]
use multiview_licence::heartbeat::{DeviceIdentity, HeartbeatClient, HeartbeatConfig};
use multiview_licence::verify::PinnedKey;
use multiview_licence::watcher::{LeaseDirectoryWatcher, DEFAULT_LEASE_DIR};
use multiview_licence::{EnforcementLevel, LeaseStore};
use multiview_mesh::MeshState;

/// The env var the pinned issuer **verifying key** is read from (hex-encoded
/// Ed25519 public key, 64 hex chars / 32 bytes). When unset (or malformed), no
/// key is pinned: the machine runs **unlicensed-honest** — no lease verifies, so
/// the entitlement plane reports no installed lease and every seam fails toward
/// leniency (no watermark, no config-lock, no start block). Never embedded in the
/// binary (secret-hygiene + the honest source-build caveat, ADR-0050 §7).
pub const PUBKEY_ENV: &str = "MULTIVIEW_LICENCE_PUBKEY";

/// The env var the lease **directory** is read from (overriding
/// [`DEFAULT_LEASE_DIR`]). A dropped, signed lease file there is verified against
/// the pinned key and installed at startup; absent/unreadable is fine (no lease).
pub const LEASE_DIR_ENV: &str = "MULTIVIEW_LICENCE_DIR";

/// The wait-free **tile-watermark signal** the overlay bake samples each frame
/// (Conspect S3, ADR-0050 §5).
///
/// It holds an `arc_swap`'d [`EnforcementLevel`] — the single published ladder
/// level. The bake reads [`WatermarkSignal::watermark`] with one lock-free load
/// (no lock, no allocation), so the per-frame watermark decision is O(1) and
/// cannot stall the output clock (invariant #1). The level is updated off-thread
/// via [`WatermarkSignal::set`] (a background poll of the lease store), never from
/// the hot loop.
#[derive(Clone)]
pub struct WatermarkSignal {
    /// The published ladder level. `arc_swap` gives a wait-free latest-value read
    /// + an off-thread atomic store (ADR-I001 isolation primitive).
    level: Arc<ArcSwap<EnforcementLevel>>,
}

impl WatermarkSignal {
    /// A clean signal (no watermark): the compliant default. Equivalent to a
    /// machine with a valid lease — the canvas is unmarked.
    #[must_use]
    pub fn clean() -> Self {
        Self::at(EnforcementLevel::Active)
    }

    /// A signal pinned at `level` (tests inject a watermark rung directly to drive
    /// a deterministic golden bake).
    #[must_use]
    pub fn at(level: EnforcementLevel) -> Self {
        Self {
            level: Arc::new(ArcSwap::from_pointee(level)),
        }
    }

    /// Publish a new ladder `level` (off-thread). The next bake reads it.
    pub fn set(&self, level: EnforcementLevel) {
        self.level.store(Arc::new(level));
    }

    /// The currently-published ladder level (a wait-free load).
    #[must_use]
    pub fn level(&self) -> EnforcementLevel {
        **self.level.load()
    }

    /// Whether the bake should stamp the corner watermark this frame — a single
    /// wait-free load + the pure `EnforcementLevel::watermark()` test. Called once
    /// per baked frame, off the hot loop (the bake runs on collected output
    /// frames, ADR-0050 §5).
    #[must_use]
    pub fn watermark(&self) -> bool {
        self.level().watermark()
    }
}

impl Default for WatermarkSignal {
    fn default() -> Self {
        Self::clean()
    }
}

/// The assembled cli entitlement plane: the shared lease store, the published
/// watermark signal (S3), and the optional pinned key (so the control plane can
/// also accept a presented lease over `POST /api/v1/licence/lease`).
///
/// Everything here is **data** the engine seams sample; nothing holds an engine
/// handle (ADR-0050 §5/§6.3, invariant #1/#10). The same `store` backs the
/// control plane's `LicenceState`, so the API, the chrome, and the engine read
/// the **same** ladder state.
pub struct EntitlementPlane {
    /// The shared verified-lease store (also the control plane's `LicenceState`).
    pub store: Arc<LeaseStore>,
    /// The wait-free watermark signal the overlay bake samples (S3).
    pub signal: WatermarkSignal,
    /// The pinned issuer key, if one was configured (the install path needs it).
    pub pinned: Option<PinnedKey>,
    /// The shared local-mesh discovery/relay state (Conspect, ADR-0051): the
    /// untrusted discovered-peer inventory + the relay opt-in the control plane's
    /// `/api/v1/mesh/*` routes render + toggle, and (under `mesh-mdns`) the
    /// always-on browse loop maintains (announce awaits the O1/O2/O6 signing
    /// material — see `spawn_mesh_discovery`). Always wired so the API serves a
    /// real shared store; control-plane only, no engine handle (invariant #10).
    pub mesh: Arc<MeshState>,
    /// The retained device-licensing client, set once when `spawn_heartbeat` builds
    /// it (ADR-I009). The operator-invoked one-shot lifecycle ops
    /// ([`rebind`](Self::rebind)/[`deactivate`](Self::deactivate)) reuse THIS SAME
    /// in-process client so the in-memory retry pin (`PendingAttempt`) and the
    /// single-owner `FileNonceStore` flock stay coherent — an ambiguous transport /
    /// `409` then replays the SAME idempotency-key verbatim and never double-charges
    /// the scarce rebind budget. `None` until the heartbeat is configured + started
    /// (a lifecycle op then builds a one-shot client itself, see `lifecycle_client`).
    #[cfg(feature = "heartbeat")]
    heartbeat_client: std::sync::OnceLock<std::sync::Arc<HeartbeatClient<ConspectHttpServer>>>,
}

impl EntitlementPlane {
    /// Assemble the entitlement plane from the process environment: read the
    /// pinned key from [`PUBKEY_ENV`] (hex), build the shared store, and — when a
    /// key is pinned — load any dropped lease file from [`LEASE_DIR_ENV`] (or
    /// [`DEFAULT_LEASE_DIR`]) once at startup. The published [`WatermarkSignal`] is
    /// initialised from the resulting state.
    ///
    /// All-data, fail-toward-leniency: a missing/malformed key, a missing lease
    /// dir, or a garbage lease file leaves the machine unlicensed-honest (no
    /// watermark, no lock, no start block) — never a crash, never off air.
    #[must_use]
    pub fn from_env() -> Self {
        let store = Arc::new(LeaseStore::new());
        let pinned = pinned_key_from_env();
        if let Some(pinned) = pinned.as_ref() {
            let dir = std::env::var(LEASE_DIR_ENV)
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_LEASE_DIR.to_owned());
            // One-shot load of any dropped, signed lease file (the directory
            // watcher's poll step). A missing dir / garbage file is logged + skipped
            // inside `poll_once` (never off air).
            let watcher = LeaseDirectoryWatcher::new(dir, pinned.clone(), Arc::clone(&store));
            let _ = watcher.poll_once(multiview_licence::store::system_now());
        } else {
            tracing::info!(
                "no {PUBKEY_ENV} pinned — running unlicensed-honest (no lease verifies; \
                 enforcement seams fail toward leniency)"
            );
        }
        let signal = WatermarkSignal::clean();
        refresh_signal(&store, &signal);
        Self {
            store,
            signal,
            pinned,
            // A fresh, empty mesh state: always-on discovery, no peers yet, relay
            // declined (opt-out default). The control plane renders + toggles it;
            // under `mesh-mdns` the spawned announce/browse loop folds discovered
            // neighbours into it. Control-plane only (invariant #10).
            mesh: Arc::new(MeshState::new()),
            #[cfg(feature = "heartbeat")]
            heartbeat_client: std::sync::OnceLock::new(),
        }
    }

    /// Spawn the always-on local-mesh **browse + age** loop (Conspect, ADR-0051
    /// §2/§5) — the `mesh-mdns` feature only.
    ///
    /// Starts the live mDNS service (IPv6-first `ff02::fb`, IPv4 legacy interop)
    /// and folds discovered neighbours (untrusted) into the shared
    /// [`MeshState`](multiview_mesh::MeshState) the control plane serves, aging out
    /// peers it stops hearing from. **Best-effort, never blocks (invariant #10):**
    /// a daemon that fails to start is logged and skipped (discovery simply yields
    /// no peers — never off air, never a crash); the browse drain is non-blocking
    /// and the loop sleeps between rounds, holding no engine handle.
    ///
    /// The loop **browses only** — it does not yet *announce* a signed summary,
    /// because signing requires this machine's own Ed25519 key and salted
    /// fingerprint digests, which are provisioned by the licence-server handshake +
    /// claim-time salt (the operator-confirm items O1/O2/O6, the conspect brief
    /// §14) that are sequenced after the local plane. Once that material is wired,
    /// the announce side folds in here over the same transport (the
    /// [`announce_browse_step`](multiview_mesh::driver::announce_browse_step) round
    /// the offline tests already cover) without touching the discovery path.
    #[cfg(feature = "mesh-mdns")]
    pub fn spawn_mesh_discovery(&self) {
        use std::time::Instant;

        use multiview_mesh::peer::PeerObservation;
        use multiview_mesh::service::{decode_received, MdnsService, DEFAULT_PORT};
        use multiview_mesh::transport::MeshTransport;

        // The instance name is this machine's stable hex peer id once a fingerprint
        // is provisioned; until then a per-process random hex anchor keeps our own
        // announcement (if any) distinct. Browse-only here, so the host/instance are
        // informational. A `.local.` host derived from the process is fine for mDNS.
        let host = "multiview.local.".to_owned();
        let instance = format!("multiview-{:016x}", std::process::id());
        let service = match MdnsService::start(&instance, &host, DEFAULT_PORT) {
            Ok(service) => service,
            Err(err) => {
                tracing::info!(
                    %err,
                    "local-mesh mDNS discovery unavailable this run (no multicast \
                     interface?) — best-effort, never off air; /api/v1/mesh serves an \
                     empty inventory"
                );
                return;
            }
        };
        let mesh = Arc::clone(&self.mesh);
        let start = Instant::now();
        tokio::spawn(async move {
            loop {
                // Browse (non-blocking) + fold untrusted observations + age out.
                // Browse-only until the machine signing key + salt land (O1/O2/O6).
                let now = start.elapsed();
                match service.poll_received() {
                    Ok(received) => {
                        for announcement in &received {
                            match decode_received(announcement) {
                                Ok(payload) => {
                                    if let Some(key) = payload.peer_key() {
                                        mesh.observe(PeerObservation {
                                            key,
                                            claim_state: payload.claim_state,
                                            observed_at: now,
                                        });
                                    }
                                }
                                Err(err) => {
                                    tracing::debug!(%err, "ignoring a malformed mesh announcement");
                                }
                            }
                        }
                    }
                    Err(err) => {
                        tracing::debug!(%err, "mesh browse poll failed this round (best-effort)");
                    }
                }
                mesh.age_out(now);
                tokio::time::sleep(multiview_mesh::driver::ANNOUNCE_INTERVAL).await;
            }
        });
        tracing::info!(
            service = multiview_mesh::service::SERVICE_TYPE,
            "local-mesh discovery running (always-on mDNS browse, IPv6-first; \
             /api/v1/mesh/peers serves the untrusted inventory)"
        );
    }

    /// No-op when the `mesh-mdns` feature is off: the shared [`MeshState`] is still
    /// wired (so `/api/v1/mesh/*` serves it), but no live socket loop runs and the
    /// default build stays socket-free + `cargo deny`-clean (ADR-0051 §6).
    #[cfg(not(feature = "mesh-mdns"))]
    pub fn spawn_mesh_discovery(&self) {
        tracing::debug!(
            "local-mesh live discovery is OFF (build without `mesh-mdns`); \
             /api/v1/mesh serves the wired (empty) inventory"
        );
    }

    /// Spawn the Conspect **device-licensing heartbeat** loop (CONSPECT-3,
    /// ADR-0096) — the `heartbeat` feature only.
    ///
    /// Reads the heartbeat settings from the environment ([`HeartbeatSettings`]):
    /// the organisation id (config-driven, O4), the pinned ECDSA-P256 **root**
    /// key, the Conspect base URL, the account JWT bearer, and the salted device
    /// identity. When the essential settings are unset, the loop is **not** started
    /// (logged) — the machine runs unlicensed-honest, the same fail-toward-leniency
    /// posture as the rest of the plane.
    ///
    /// When configured, it `tokio::spawn`s a [`HeartbeatClient`] loop that renews
    /// the lease against conspect.studio and drives the shared
    /// [`LeaseStore::install_binding`](multiview_licence::LeaseStore::install_binding)
    /// convergence — the same path the offline file-drop and mesh relay already
    /// use, so S1/S2/S3 re-sample with no extra wiring (via the existing
    /// [`refresh_signal`] poll). **Best-effort, never off air (invariants #1/#10):**
    /// the task holds only [`Arc<LeaseStore>`](multiview_licence::LeaseStore) + the
    /// pinned root + the client — **no** engine handle/channel/lock — and only ever
    /// tightens on a positively-verified signed lease; every failure (or a withheld
    /// lease) keeps the last-good lease and lets it age.
    #[cfg(feature = "heartbeat")]
    pub fn spawn_heartbeat(&self) {
        let Some(settings) = HeartbeatSettings::from_env() else {
            tracing::info!(
                "Conspect heartbeat is OFF (unconfigured): set {} + {} + {} + {} to enable the \
                 device-licensing phone-home; running unlicensed-honest until then",
                HeartbeatSettings::ORG_ENV,
                HeartbeatSettings::ROOT_ENV,
                HeartbeatSettings::API_ENV,
                HeartbeatSettings::TOKEN_ENV
            );
            return;
        };
        let api = settings.api_base.clone();
        let org = settings.org_id.clone();
        // Build (and RETAIN) the one client this process drives — the renew loop AND
        // the operator-invoked lifecycle ops (rebind/deactivate, ADR-I009) share it,
        // so the in-memory retry pin + the single-owner nonce-store lock stay coherent.
        let Some(client) = self.build_client(&settings) else {
            return; // build_client logged the fail-closed reason
        };
        // Retain it for the lifecycle ops (idempotent: set-or-keep the first client).
        // `get_or_init` returns the already-retained client if a prior call set one,
        // else stores and returns this one — so the renew loop and the operator ops
        // always drive the SAME in-process client (coherent pin + nonce-store lock).
        let client = std::sync::Arc::clone(self.heartbeat_client.get_or_init(|| client));
        tokio::spawn(async move {
            client.run_forever().await;
        });
        tracing::info!(
            org = %org,
            api = %api,
            "Conspect device heartbeat running (control-plane phone-home; renews the lease via \
             the shared install convergence; never off air)"
        );
    }

    /// Build the device-licensing [`HeartbeatClient`] from `settings` (ADR-I006/I007/
    /// I008/I009), or `None` (logging the fail-closed reason) when any piece cannot be
    /// built — a malformed pinned root, an un-buildable HTTPS-only client (never a
    /// plaintext credential-carrying fallback), an un-takeable durable idempotency-nonce
    /// lock (a second owner on this lease dir), or an un-loadable/generatable device key.
    /// Shared by `spawn_heartbeat` (the renew loop) and the one-shot lifecycle ops so
    /// they wire the SAME server + nonce store + device key (the `heartbeat`-only seam).
    #[cfg(feature = "heartbeat")]
    fn build_client(
        &self,
        settings: &HeartbeatSettings,
    ) -> Option<std::sync::Arc<HeartbeatClient<ConspectHttpServer>>> {
        let pinned_root = match multiview_licence::heartbeat::PinnedRoot::from_base64url(
            &settings.pinned_root_b64url,
        ) {
            Ok(root) => root,
            Err(err) => {
                tracing::warn!(
                    %err,
                    "Conspect licensing is OFF: {} is not a valid base64url ECDSA-P256 root \
                     point — running unlicensed-honest",
                    HeartbeatSettings::ROOT_ENV
                );
                return None;
            }
        };
        // Fail closed: if the HTTPS-only client cannot be built, do NOT fall back
        // to a plaintext-capable client that would leak the bearer JWT.
        let server =
            match ConspectHttpServer::new(settings.api_base.clone(), settings.bearer_token.clone())
            {
                Ok(server) => std::sync::Arc::new(server),
                Err(err) => {
                    tracing::warn!(
                        %err,
                        "Conspect licensing is OFF: could not build the HTTPS-only client — \
                         running unlicensed-honest (never a plaintext credential-carrying client)"
                    );
                    return None;
                }
            };
        // Durable idempotency nonce beside the lease state (the same dir the
        // offline file-drop watcher reads), so a restart never reuses a prior
        // lifetime's Idempotency-Key (cross-restart duplicate-mutation defence).
        let lease_dir = std::env::var(LEASE_DIR_ENV)
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_LEASE_DIR.to_owned());
        // Fail closed: if the nonce lock cannot be taken (another `multiview` owns
        // this lease dir) do NOT start a second minter that could issue colliding
        // Idempotency-Keys.
        let nonce_store: multiview_licence::heartbeat::SharedNonceStore =
            match FileNonceStore::in_dir(&lease_dir) {
                Ok(store) => std::sync::Arc::new(store),
                Err(err) => {
                    tracing::warn!(
                        %err,
                        "Conspect licensing is OFF: could not take the durable idempotency-nonce \
                         lock (a second heartbeat owner on this lease dir?) — keeping last-good"
                    );
                    return None;
                }
            };
        // Device-PoP (v0.9.0 enforced, ADR-I007): load (or first-boot generate +
        // persist) the per-instance Ed25519 device keypair beside the lease state.
        // Fail closed: a corrupt/unwritable key keeps licensing OFF (a present-but-
        // corrupt key is NOT silently regenerated — that breaks server-side continuity).
        let device_signer: std::sync::Arc<dyn multiview_licence::heartbeat::DeviceSigner> =
            match DeviceKeyStore::load_or_generate(&lease_dir) {
                Ok(store) => std::sync::Arc::new(store),
                Err(err) => {
                    tracing::warn!(
                        %err,
                        "Conspect licensing is OFF: could not load/generate the durable device \
                         proof-of-possession keypair — keeping last-good (never an op without a \
                         valid device-PoP proof)"
                    );
                    return None;
                }
            };
        Some(std::sync::Arc::new(HeartbeatClient::with_nonce_and_signer(
            server,
            std::sync::Arc::clone(&self.store),
            pinned_root,
            settings.heartbeat_config(),
            settings.identity.clone(),
            nonce_store,
            device_signer,
        )))
    }

    /// Get the retained device-licensing client (set by `spawn_heartbeat`), or build a
    /// fresh one from the environment for a one-shot lifecycle op invoked WITHOUT a
    /// running daemon. Reusing the retained client (the common case) keeps the in-memory
    /// retry pin + the nonce-store flock coherent (ADR-I009); building a fresh one is the
    /// no-daemon fallback (it takes the flock for the duration of the op). `None` when
    /// licensing is unconfigured / un-buildable (the caller reports it).
    #[cfg(feature = "heartbeat")]
    fn lifecycle_client(
        &self,
        settings: &HeartbeatSettings,
    ) -> Option<std::sync::Arc<HeartbeatClient<ConspectHttpServer>>> {
        if let Some(existing) = self.heartbeat_client.get() {
            return Some(std::sync::Arc::clone(existing));
        }
        let client = self.build_client(settings)?;
        // Retain it so a subsequent op (or a later daemon start) reuses the SAME client
        // (the pin stays coherent across operator re-invocations within this process).
        let _ = self.heartbeat_client.set(std::sync::Arc::clone(&client));
        Some(client)
    }

    /// Operator-invoked **device REBIND** (ADR-I009): reactivate this device's bound
    /// instance after a lawful hardware change (the same binding — no new seat), charging
    /// the 3-free-per-AEST-year budget. A device-initiated, PoP-signed, idempotent HTTPS
    /// call on the in-process client. Returns the [`HeartbeatOutcome::Rebound`] on
    /// success, or a [`HeartbeatError`] (fail-closed — keep last-good, never off air,
    /// never an engine touch). Requires the licence id ([`HeartbeatSettings::LICENCE_ID_ENV`]).
    ///
    /// # Errors
    /// [`HeartbeatError`] when licensing is unconfigured, the licence id is unset, or the
    /// rebind contact / proof fails — every one keeps the entitlement plane last-good.
    #[cfg(feature = "heartbeat")]
    pub async fn rebind(
        &self,
    ) -> Result<
        multiview_licence::heartbeat::HeartbeatOutcome,
        multiview_licence::heartbeat::HeartbeatError,
    > {
        use multiview_licence::heartbeat::{HeartbeatError, PopError};
        let settings = HeartbeatSettings::from_env().ok_or_else(|| {
            HeartbeatError::Pop(PopError::Cose(
                "Conspect licensing is unconfigured (set MULTIVIEW_LICENCE_* env)".to_owned(),
            ))
        })?;
        if settings.licence_id.is_empty() {
            return Err(HeartbeatError::Pop(PopError::Cose(format!(
                "no licence id configured ({}) — a rebind needs RebindRequest.licenceId",
                HeartbeatSettings::LICENCE_ID_ENV
            ))));
        }
        let client = self.lifecycle_client(&settings).ok_or_else(|| {
            HeartbeatError::Pop(PopError::Cose(
                "Conspect licensing client could not be built (keeping last-good)".to_owned(),
            ))
        })?;
        client.rebind_once(&settings.licence_id).await
    }

    /// Operator-invoked **device DEACTIVATE** (ADR-I009): decommission this device's
    /// binding and return its seat (idempotent, graceful surrender). A device-initiated,
    /// PoP-signed HTTPS call on the in-process client. Returns
    /// [`HeartbeatOutcome::Deactivated`] on success. It installs **nothing** and does
    /// **not** stop local output — the last-good lease ages out via the offline ladder
    /// (never off air). The operator stops the running process for an immediate local
    /// stop (there is no kill verb — invariant #1).
    ///
    /// # Errors
    /// [`HeartbeatError`] when licensing is unconfigured or the deactivate contact /
    /// proof fails — every one keeps the entitlement plane last-good.
    #[cfg(feature = "heartbeat")]
    pub async fn deactivate(
        &self,
    ) -> Result<
        multiview_licence::heartbeat::HeartbeatOutcome,
        multiview_licence::heartbeat::HeartbeatError,
    > {
        use multiview_licence::heartbeat::{HeartbeatError, PopError};
        let settings = HeartbeatSettings::from_env().ok_or_else(|| {
            HeartbeatError::Pop(PopError::Cose(
                "Conspect licensing is unconfigured (set MULTIVIEW_LICENCE_* env)".to_owned(),
            ))
        })?;
        let client = self.lifecycle_client(&settings).ok_or_else(|| {
            HeartbeatError::Pop(PopError::Cose(
                "Conspect licensing client could not be built (keeping last-good)".to_owned(),
            ))
        })?;
        client.deactivate_once().await
    }

    /// No-op when the `heartbeat` feature is off: the entitlement plane is fully
    /// wired (the offline file-drop install + the read-only heartbeat-status
    /// surface still work), but no live licence-server loop runs and the default
    /// build stays network-free + `cargo deny`-clean (ADR-0096).
    #[cfg(not(feature = "heartbeat"))]
    pub fn spawn_heartbeat(&self) {
        tracing::debug!(
            "Conspect device heartbeat is OFF (build without `heartbeat`); the entitlement \
             plane serves local lease state only"
        );
    }

    /// The current sampled enforcement level for the S1 startup gate.
    #[must_use]
    pub fn level(&self) -> Option<EnforcementLevel> {
        current_level(&self.store)
    }
}

/// Read + parse the pinned issuer verifying key from [`PUBKEY_ENV`] (hex). Returns
/// `None` when unset, empty, or not a valid 32-byte Ed25519 key (logged) — the
/// unlicensed-honest default.
#[must_use]
fn pinned_key_from_env() -> Option<PinnedKey> {
    let hex = std::env::var(PUBKEY_ENV).ok().filter(|s| !s.is_empty())?;
    let bytes = decode_hex(hex.trim())?;
    if let Ok(key) = PinnedKey::from_slice(&bytes) {
        Some(key)
    } else {
        tracing::warn!(
            "{PUBKEY_ENV} is not a valid 32-byte Ed25519 public key — ignoring \
             (running unlicensed-honest)"
        );
        None
    }
}

/// Decode a hex string to bytes, or `None` on any non-hex / odd-length input
/// (no `as` casts; total + panic-free).
#[must_use]
fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let nibble = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    let mut i = 0;
    while i + 1 < s.len() {
        let hi = nibble(*bytes.get(i)?)?;
        let lo = nibble(*bytes.get(i + 1)?)?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Some(out)
}

/// Sample the current entitlement [`EnforcementLevel`] from the lease `store`, or
/// `None` when no lease is installed.
///
/// This is what the startup gate (S1) consults *before* building a new engine
/// (`build_gated`). Reading off the store is wait-free and off the hot loop; the
/// gate fails toward leniency on `None` (ADR-0050 §6.3).
#[must_use]
pub fn current_level(store: &LeaseStore) -> Option<EnforcementLevel> {
    store.status().map(|s| s.enforcement)
}

/// Publish the lease `store`'s current level into `signal` (off-thread refresh).
///
/// Called by the cli's background entitlement poll so the wait-free watermark
/// signal tracks the lease as it ages across day boundaries / is renewed —
/// without ever reading the store on the hot loop. A store with no lease publishes
/// the compliant `Active` level (fail-toward-leniency: no positive lapse evidence
/// → no watermark).
pub fn refresh_signal(store: &LeaseStore, signal: &WatermarkSignal) {
    let level = current_level(store).unwrap_or(EnforcementLevel::Active);
    signal.set(level);
}

// ===========================================================================
// Conspect heartbeat (CONSPECT-3, ADR-0096) — the cli-boundary HTTP transport
// + environment-driven settings. Feature `heartbeat`, OFF by default.
// ===========================================================================

/// The environment-driven heartbeat settings the cli assembles for the
/// [`HeartbeatClient`]. Read from the process environment so the pinned root, the
/// account token, and the salted device identity never live in the binary
/// (secret hygiene; mirrors [`PUBKEY_ENV`]). The **organisation id is
/// config-driven** (ADR-0096 O4): the operator sets it explicitly (the free
/// auto-issue default org is an external-doc residual), so there is no hard-coded
/// guess.
#[cfg(feature = "heartbeat")]
#[derive(Debug, Clone)]
pub struct HeartbeatSettings {
    /// The organisation id the device heartbeats against (`{orgId}`).
    pub org_id: String,
    /// The pinned ECDSA-P256 **root** key, base64url uncompressed point.
    pub pinned_root_b64url: String,
    /// The Conspect API base URL (e.g. `https://api.conspect.studio/v0`).
    pub api_base: String,
    /// The account JWT bearer token (operator role for heartbeat). Never logged.
    pub bearer_token: String,
    /// The salted device identity the requests carry (no raw identifiers).
    pub identity: DeviceIdentity,
    /// The licence id the binding draws its seat from (ADR-I009) — required by the
    /// device REBIND lifecycle op (`RebindRequest.licenceId`); unused by heartbeat /
    /// activate / deactivate. Empty when unset (rebind then fails closed with a clear
    /// "no licence id configured" message — never a silent wrong id).
    pub licence_id: String,
    /// Whether first-contact device ACTIVATE / enrolment is enabled (ADR-I008) —
    /// `true` only when the full device-identity triple required for a valid
    /// `ActivateRequest` is configured (so a fresh device enrols rather than waiting
    /// for an install surface). A device that already holds a binding renews
    /// regardless; this only governs the no-binding path.
    pub enable_activate: bool,
    /// The optional paid claim code sent on ACTIVATE (ADR-I008). `None` (the default)
    /// auto-issues a free non-commercial licence; `Some(code)` redeems a paid order.
    pub claim_code: Option<String>,
}

#[cfg(feature = "heartbeat")]
impl HeartbeatSettings {
    /// The env var the organisation id is read from (config-driven, O4).
    pub const ORG_ENV: &'static str = "MULTIVIEW_LICENCE_ORG";
    /// The env var the pinned ECDSA-P256 root key (base64url) is read from.
    pub const ROOT_ENV: &'static str = "MULTIVIEW_LICENCE_ROOT";
    /// The env var the Conspect API base URL is read from.
    pub const API_ENV: &'static str = "MULTIVIEW_LICENCE_API";
    /// The env var the account JWT bearer token is read from (never logged).
    pub const TOKEN_ENV: &'static str = "MULTIVIEW_LICENCE_TOKEN";
    /// The env var the salted hardware-fingerprint digest is read from (hex).
    pub const FP_DIGEST_ENV: &'static str = "MULTIVIEW_LICENCE_FP_DIGEST";
    /// The env var the fingerprint score is read from (0–100).
    pub const FP_SCORE_ENV: &'static str = "MULTIVIEW_LICENCE_FP_SCORE";
    /// The env var the registered machine id is read from.
    pub const MACHINE_ENV: &'static str = "MULTIVIEW_LICENCE_MACHINE_ID";
    /// The env var the instance id is read from.
    pub const INSTANCE_ENV: &'static str = "MULTIVIEW_LICENCE_INSTANCE_ID";
    /// The env var the instance binding id is read from (once known).
    pub const BINDING_ENV: &'static str = "MULTIVIEW_LICENCE_BINDING_ID";
    /// The env var the salted hardware digest is read from.
    pub const HW_DIGEST_ENV: &'static str = "MULTIVIEW_LICENCE_HW_DIGEST";
    /// The env var the instance discriminator hash is read from.
    pub const DISC_HASH_ENV: &'static str = "MULTIVIEW_LICENCE_DISC_HASH";
    /// The env var the instance discriminator digest is read from.
    pub const DISC_DIGEST_ENV: &'static str = "MULTIVIEW_LICENCE_DISC_DIGEST";
    /// The env var the device Ed25519 proof-of-possession public key (base64url)
    /// is read from.
    pub const DEVICE_KEY_ENV: &'static str = "MULTIVIEW_LICENCE_DEVICE_KEY";
    /// The env var the optional paid claim code is read from (ADR-I008). Unset →
    /// the free non-commercial auto-issue path on activate.
    pub const CLAIM_ENV: &'static str = "MULTIVIEW_LICENCE_CLAIM_CODE";
    /// The env var the licence id is read from (ADR-I009) — required by the device
    /// REBIND lifecycle op (`RebindRequest.licenceId`).
    pub const LICENCE_ID_ENV: &'static str = "MULTIVIEW_LICENCE_LICENCE_ID";

    /// Assemble settings from the process environment, or `None` when the
    /// essential ones (org id, pinned root, API base, bearer token) are unset —
    /// the heartbeat then stays off and the machine runs unlicensed-honest.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let org_id = non_empty_env(Self::ORG_ENV)?;
        let pinned_root_b64url = non_empty_env(Self::ROOT_ENV)?;
        let api_base = non_empty_env(Self::API_ENV)?;
        let bearer_token = non_empty_env(Self::TOKEN_ENV)?;
        let identity = DeviceIdentity {
            machine_id: non_empty_env(Self::MACHINE_ENV).unwrap_or_default(),
            instance_id: non_empty_env(Self::INSTANCE_ENV).unwrap_or_default(),
            binding_id: non_empty_env(Self::BINDING_ENV),
            fingerprint_digest: non_empty_env(Self::FP_DIGEST_ENV).unwrap_or_default(),
            fingerprint_score: non_empty_env(Self::FP_SCORE_ENV)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            hardware_digest: non_empty_env(Self::HW_DIGEST_ENV).unwrap_or_default(),
            instance_discriminator_hash: non_empty_env(Self::DISC_HASH_ENV).unwrap_or_default(),
            instance_discriminator_digest: non_empty_env(Self::DISC_DIGEST_ENV).unwrap_or_default(),
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            device_public_key_b64url: non_empty_env(Self::DEVICE_KEY_ENV).unwrap_or_default(),
        };
        let claim_code = non_empty_env(Self::CLAIM_ENV);
        // Enable first-contact ACTIVATE only when the device-identity FIELDS a valid
        // ActivateRequest carries are all configured (ADR-I008): the activate server
        // assigns the instanceId, but the machine/fingerprint/hardware/discriminator
        // triple + a ≥70 score must be present, else a fresh device would just earn a
        // 422. Absent the triple the device is renew-only (NoBinding until an install
        // surface provides a lease). A device that already holds a binding renews
        // regardless — this only governs the no-binding path.
        let enable_activate = !identity.machine_id.is_empty()
            && !identity.fingerprint_digest.is_empty()
            && identity.fingerprint_score >= REBIND_THRESHOLD
            && !identity.hardware_digest.is_empty()
            && !identity.instance_discriminator_hash.is_empty()
            && !identity.instance_discriminator_digest.is_empty();
        let licence_id = non_empty_env(Self::LICENCE_ID_ENV).unwrap_or_default();
        Some(Self {
            org_id,
            pinned_root_b64url,
            api_base,
            bearer_token,
            identity,
            licence_id,
            enable_activate,
            claim_code,
        })
    }

    /// Build the [`HeartbeatConfig`] from these settings.
    #[must_use]
    pub fn heartbeat_config(&self) -> HeartbeatConfig {
        HeartbeatConfig {
            org_id: self.org_id.clone(),
            // The PoP `htu` the loop signs is built from this base + org id, so it
            // matches the URL the transport POSTs to byte-for-byte (ADR-I007). Trim
            // the trailing slash to match the transport's normalisation.
            api_base: self.api_base.trim_end_matches('/').to_owned(),
            enable_activate: self.enable_activate,
            claim_code: self.claim_code.clone(),
            ..HeartbeatConfig::default()
        }
    }
}

/// The Conspect activation fingerprint-match floor (`REBIND_THRESHOLD`): activation
/// requires a score of at least this (a lower score is a server `422`). Mirrors the
/// `multiview-licence` fingerprint threshold constant (ADR-0050 §4 / brief §2).
#[cfg(feature = "heartbeat")]
const REBIND_THRESHOLD: u8 = 70;

/// Read an environment variable, returning `None` when unset or empty.
#[cfg(feature = "heartbeat")]
fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

/// Defense-in-depth: harden a secret-storage **directory** against a
/// group/world-**writable** pre-existing dir before any secret (the device key,
/// the durable nonce) is stored under it.
///
/// The secret FILES are independently 0600-verified
/// ([`DeviceKeyStore::load_existing`]) and atomically installed (`O_EXCL` + a
/// unique temp + `hard_link`), so a merely group/world-**readable** dir (the
/// normal umask-022 `0755`) is NOT a leak — others can list names, not read a
/// 0600 file. So this targets the **writable** bits only:
///
/// * not group/world-writable (`mode & 0o022 == 0`, e.g. `0700`/`0755`) → accept
///   unchanged (the common case; rejecting `0755` would break normal installs);
/// * group/world-writable **and we own it** (the dir's `uid` is our effective
///   uid) → **tighten** it (strip the group/other write bits) and continue, with
///   a one-line warning — a misconfiguration we can repair, not a hard stop;
/// * group/world-writable and **not ours** (or the tighten/confirm fails) →
///   **fail closed** with `on_error(message)`: an attacker who can write the dir
///   could pre-plant the secret paths, so we refuse to store secrets there.
///
/// Unix-only (mode is a Unix concept); a no-op on other targets, consistent with
/// the file-mode handling elsewhere in this module. `on_error` adapts the message
/// to the caller's error type (the two call sites return different ones).
#[cfg(all(feature = "heartbeat", unix))]
fn harden_secret_dir<E>(dir: &std::path::Path, on_error: impl Fn(String) -> E) -> Result<(), E> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let meta = std::fs::metadata(dir).map_err(|e| {
        on_error(format!(
            "could not stat the secret-storage dir {} to check its permissions: {e}",
            dir.display()
        ))
    })?;
    let mode = meta.permissions().mode() & 0o777;
    // Not group/world-writable (0700, 0755, …): nothing to do. A readable-only
    // dir is not a leak (the secret files are 0600).
    if mode & 0o022 == 0 {
        return Ok(());
    }
    // Group/world-writable: only the OWNER (or root) may safely tighten it. If we
    // are not the owner we cannot trust the dir (an attacker who owns it could
    // pre-plant the secret paths) → fail closed.
    let our_uid = rustix::process::geteuid().as_raw();
    if meta.uid() != our_uid {
        return Err(on_error(format!(
            "secret-storage dir {} is group/world-writable (mode {mode:#o}) and not owned by us \
             (dir uid {}, our uid {our_uid}) — refusing to store secrets in a dir an attacker \
             could pre-plant paths in",
            dir.display(),
            meta.uid()
        )));
    }
    // We own it: tighten by stripping the group/other WRITE bits (keep any read/
    // execute bits the operator set, e.g. 0755 -> 0755 is untouched above, 0777 ->
    // 0755, 0775 -> 0755, 0707 -> 0705).
    let tightened = mode & !0o022;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(tightened)).map_err(|e| {
        on_error(format!(
            "secret-storage dir {} is group/world-writable (mode {mode:#o}) and we own it but \
             could not tighten it to {tightened:#o}: {e} — refusing to store secrets",
            dir.display()
        ))
    })?;
    // Confirm the tighten actually took (a filesystem that ignores mode, or a
    // racing re-widen) — never proceed on an unverified tighten.
    let after = std::fs::metadata(dir)
        .map_err(|e| {
            on_error(format!(
                "could not re-stat the secret-storage dir {} after tightening it: {e}",
                dir.display()
            ))
        })?
        .permissions()
        .mode()
        & 0o777;
    if after & 0o022 != 0 {
        return Err(on_error(format!(
            "secret-storage dir {} is still group/world-writable (mode {after:#o}) after an \
             attempt to tighten it — refusing to store secrets",
            dir.display()
        )));
    }
    tracing::warn!(
        dir = %dir.display(),
        was = format_args!("{mode:#o}"),
        now = format_args!("{after:#o}"),
        "tightened an over-permissive (group/world-writable) secret-storage directory"
    );
    Ok(())
}

/// A durable, file-backed [`NonceStore`](multiview_licence::heartbeat::NonceStore)
/// for the idempotency mint counter — a tiny decimal file beside the lease state,
/// so the per-operation nonce survives a restart and a post-restart operation
/// never reuses a prior lifetime's key. The leaf crate does no I/O; this is the
/// cli-boundary implementation it injects via `with_clock_and_nonce`.
///
/// `load` reads the persisted high-water counter and **fails closed**: an ABSENT
/// file is `Ok(0)` (a fresh device legitimately starts at 0), but a PRESENT file
/// that is corrupt/unparseable, reads the literal `0` (which a healthy `commit`
/// never writes — the mint is always `>= 1` — so a present 0 is truncation/tamper),
/// or is unreadable for any other reason returns an `Err` — never a silent 0 that
/// would reset the high-water and re-mint a colliding key after a restart. `commit`
/// persists the new high-water with the **crash-durable** write-temp → fsync-temp →
/// rename → fsync-parent-dir protocol (so the value survives a power loss right
/// after `commit` returns, not just a torn write) and **propagates** every step's
/// failure (write, the two fsyncs, rename) as an `Err` — an un-persisted key must
/// block the mutation, not continue best-effort. The heartbeat client gates the
/// mint on these, so a nonce-store failure keeps last-good (never off air, never a
/// colliding-key mutation).
///
/// **Operational assumption (the flock is advisory):** the interprocess
/// cross-process uniqueness guarantee holds only for **cooperating owners that all
/// use this `FileNonceStore`** on a **local filesystem with working `flock`
/// semantics**. It does NOT defend against a process reaching the same lease-state
/// dir through a different code path, nor against network/overlay filesystems
/// (NFS/overlayfs) where advisory locks may be unreliable — those configurations
/// are out of scope for the nonce guard.
#[cfg(feature = "heartbeat")]
struct FileNonceStore {
    path: std::path::PathBuf,
    /// The exclusive advisory lock holder for the whole process lifetime. Held so
    /// a SECOND `multiview` process pointed at the same lease-state dir cannot
    /// concurrently mint colliding Idempotency-Keys: that process's
    /// [`FileNonceStore::in_dir`] fails to take the lock and its heartbeat declines
    /// to start (fail closed). The OS releases the lock when this `File` is dropped
    /// (process exit or `EntitlementPlane` teardown), so a crashed owner never
    /// strands the lock. `_lock` is never read — its lifetime IS the guarantee.
    _lock: std::fs::File,
}

#[cfg(feature = "heartbeat")]
impl FileNonceStore {
    /// Open the durable nonce store in `dir` (the lease-state dir), taking a
    /// **non-blocking exclusive advisory lock** on `<dir>/idempotency-nonce.lock`
    /// so this process is the sole minter of the nonce file. The data lives in
    /// `<dir>/idempotency-nonce`.
    ///
    /// # Errors
    /// [`NonceError`](multiview_licence::heartbeat::NonceError) when the lock dir
    /// cannot be created/opened, or when ANOTHER process already holds the lock
    /// (a second heartbeat owner on the same lease dir) — the caller then keeps the
    /// heartbeat OFF (fail closed; the entitlement plane keeps last-good).
    fn in_dir(dir: &str) -> Result<Self, multiview_licence::heartbeat::NonceError> {
        use multiview_licence::heartbeat::NonceError;
        let dir_path = std::path::Path::new(dir);
        // Ensure the lease-state dir exists so the lock/data files can be created
        // (the offline file-drop watcher tolerates an absent dir; the nonce owner
        // must materialise it to hold the lock).
        std::fs::create_dir_all(dir_path).map_err(|e| {
            NonceError::new(format!("could not create the lease-state dir {dir}: {e}"))
        })?;
        // Defense-in-depth: tighten an owned group/world-writable dir (fail closed
        // on one we don't own) before the lock/data files are created — an attacker
        // with write access could otherwise pre-plant those paths. A readable-only
        // (0755) dir is accepted unchanged (not a leak; the data file is created by
        // the crash-durable writer). Unix-only.
        #[cfg(unix)]
        harden_secret_dir(dir_path, NonceError::new)?;
        let lock_path = dir_path.join("idempotency-nonce.lock");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| {
                NonceError::new(format!(
                    "could not open the idempotency-nonce lock {}: {e}",
                    lock_path.display()
                ))
            })?;
        // Non-blocking exclusive lock: if a second owner holds it, fail closed
        // rather than block the spawn. Released automatically when `lock` drops.
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).map_err(
            |e| {
                NonceError::new(format!(
                    "another process holds the idempotency-nonce lock {} \
                     (a second heartbeat owner on this lease dir?): {e}",
                    lock_path.display()
                ))
            },
        )?;
        Ok(Self {
            path: dir_path.join("idempotency-nonce"),
            _lock: lock,
        })
    }
}

#[cfg(feature = "heartbeat")]
impl multiview_licence::heartbeat::NonceStore for FileNonceStore {
    fn load(&self) -> Result<u64, multiview_licence::heartbeat::NonceError> {
        use multiview_licence::heartbeat::NonceError;
        match std::fs::read_to_string(&self.path) {
            // A PRESENT value that does not parse is NOT trustworthy — fail closed
            // (never a silent 0 that resets the high-water and re-mints a colliding
            // key after a restart).
            Ok(s) => {
                let value = s.trim().parse::<u64>().map_err(|_| {
                    NonceError::new(format!(
                        "idempotency nonce file {} is present but unparseable",
                        self.path.display()
                    ))
                })?;
                // `commit` only ever persists a value >= 1 (the mint is
                // `max(durable).saturating_add(1)`), so a PRESENT 0 is impossible
                // from a healthy write — it can only be truncation / a partial write
                // / tampering. Reject it (fail closed); trusting it would reset the
                // high-water and re-mint a colliding key after a restart. Only an
                // ABSENT file (the NotFound arm below) is the trusted-zero fresh start.
                if value == 0 {
                    return Err(NonceError::new(format!(
                        "idempotency nonce file {} is present but reads 0 (commit never \
                         writes 0 — truncation/partial-write/tamper); refusing to trust it",
                        self.path.display()
                    )));
                }
                Ok(value)
            }
            // Absent on a fresh device (the common case) — a trusted start at 0.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            // Present-but-unreadable (permissions, I/O error) is untrustworthy too.
            Err(e) => Err(NonceError::new(format!(
                "could not read the idempotency nonce file {}: {e}",
                self.path.display()
            ))),
        }
    }

    fn commit(&self, value: u64) -> Result<(), multiview_licence::heartbeat::NonceError> {
        use std::io::Write as _;

        use multiview_licence::heartbeat::NonceError;
        // CRASH-DURABLE atomic write (write-temp → fsync temp → rename → fsync dir):
        // a crash leaves either the old value or the new one, never a truncated
        // counter, AND the persisted high-water survives a power loss right after
        // `commit` returns — so the next process never re-mints a colliding key.
        // EVERY step fails closed (the caller's gated mint refuses to send a
        // possibly-colliding mutation), never best-effort. No unwrap/panic.
        let tmp = self.path.with_extension("tmp");
        // 1. Write the new value to the temp file.
        let mut file = std::fs::File::create(&tmp).map_err(|e| {
            NonceError::new(format!(
                "could not create the idempotency nonce temp {}: {e}",
                tmp.display()
            ))
        })?;
        file.write_all(value.to_string().as_bytes()).map_err(|e| {
            NonceError::new(format!(
                "could not write the idempotency nonce to {}: {e}",
                tmp.display()
            ))
        })?;
        // 2. fsync the TEMP file BEFORE the rename, so its data is on stable storage
        //    before it becomes the live file (a rename of un-fsync'd data can roll
        //    back the contents after a power loss).
        file.sync_all().map_err(|e| {
            NonceError::new(format!(
                "could not fsync the idempotency nonce temp {}: {e}",
                tmp.display()
            ))
        })?;
        drop(file);
        // 3. Atomically replace the live file.
        std::fs::rename(&tmp, &self.path).map_err(|e| {
            NonceError::new(format!(
                "could not finalise the idempotency nonce file {}: {e}",
                self.path.display()
            ))
        })?;
        // 4. fsync the PARENT DIRECTORY so the rename's directory entry is itself
        //    durable (otherwise a power loss can lose the entry and the file reverts
        //    to the old name/contents). Opening a directory read-only and calling
        //    `sync_all` is the portable way to fsync it on Linux/macOS.
        let parent = self.path.parent().ok_or_else(|| {
            NonceError::new(format!(
                "idempotency nonce path {} has no parent directory to fsync",
                self.path.display()
            ))
        })?;
        let dir = std::fs::File::open(parent).map_err(|e| {
            NonceError::new(format!(
                "could not open the lease-state dir {} to fsync: {e}",
                parent.display()
            ))
        })?;
        dir.sync_all().map_err(|e| {
            NonceError::new(format!(
                "could not fsync the lease-state dir {}: {e}",
                parent.display()
            ))
        })?;
        Ok(())
    }
}

/// The durable per-instance Ed25519 **device keypair** for the device-PoP proof
/// (CONSPECT-3 D2, ADR-I007). Generated ONCE (the only RNG use in the product, at
/// this cli boundary — the leaf crate stays no-RNG) and persisted beside the lease
/// state (`<lease-dir>/device-key.ed25519`, the 32-byte seed, `0600`), so it
/// survives a restart and the device keeps a **stable identity** (the server
/// verifies continuity against the STORED key and binds its RFC 7638 thumbprint as
/// the lease `cnf_jkt`). The leaf crate's [`DeviceSigner`](multiview_licence::heartbeat::DeviceSigner)
/// seam is implemented over a LOADED key — Ed25519 signing is deterministic (RFC
/// 8032), so no RNG enters the signing path.
///
/// **Fail closed (ADR-I007).** A present-but-corrupt/unreadable key file returns an
/// `Err` (never a silent regenerate — a NEW key would break server-side continuity
/// and be rejected as a different device); the caller then declines to start the
/// heartbeat and keeps last-good (never off air). Only an ABSENT file generates a
/// fresh keypair (a genuinely-new device's first boot).
#[cfg(feature = "heartbeat")]
struct DeviceKeyStore {
    key: ed25519_dalek::SigningKey,
}

#[cfg(feature = "heartbeat")]
impl DeviceKeyStore {
    /// The persisted seed filename inside the lease-state dir.
    const FILE: &'static str = "device-key.ed25519";

    /// Load the persisted device keypair, or generate + persist one on first boot.
    ///
    /// **Concurrency-safe first boot (panel major #4):** generation uses an ATOMIC
    /// create-once (`O_EXCL` on the final path) — the first process to win persists
    /// its seed durably; every loser observes `AlreadyExists` and RELOADS the
    /// winner, so all callers return a signer whose seed is exactly the one durably
    /// on disk (no overwrite race, no signer holding a seed that isn't persisted).
    ///
    /// # Errors
    /// [`HeartbeatError::Transport`](multiview_licence::heartbeat::HeartbeatError) (a
    /// generic boundary error type the caller already treats as keep-last-good) when
    /// the lease dir cannot be created, a present key file is corrupt / unreadable /
    /// at weak permissions, or a freshly-generated key cannot be durably persisted —
    /// all fail closed.
    fn load_or_generate(dir: &str) -> Result<Self, multiview_licence::heartbeat::HeartbeatError> {
        use multiview_licence::heartbeat::HeartbeatError;
        let dir_path = std::path::Path::new(dir);
        std::fs::create_dir_all(dir_path).map_err(|e| {
            HeartbeatError::Transport(format!("could not create the lease-state dir {dir}: {e}"))
        })?;
        // Defense-in-depth: tighten an owned group/world-writable dir (fail closed
        // on one we don't own) BEFORE storing the device-key secret — an attacker
        // with write access could otherwise pre-plant the key path. The key FILE is
        // separately 0600-verified at load + O_EXCL-installed, so a readable-only
        // (0755) dir is accepted unchanged (not a leak). Unix-only.
        #[cfg(unix)]
        harden_secret_dir(dir_path, HeartbeatError::Transport)?;
        let path = dir_path.join(Self::FILE);
        // Fast path: an existing key. Verified for perms + shape, fail-closed.
        if let Some(key) = Self::load_existing(&path)? {
            return Ok(Self { key });
        }
        // Absent: generate a fresh key and try to install it atomically (O_EXCL).
        // Draw the 32 seed bytes straight from the OS CSPRNG (ed25519-dalek 3.x
        // aligns to rand_core 0.10, which no longer ships `OsRng`; `getrandom::fill`
        // is the entropy source). Fail CLOSED on an entropy error — keep last-good,
        // never mint a low-entropy identity (no panic, unlike a `generate`+`UnwrapErr`
        // path). `SigningKey::from_bytes` over a uniformly-random seed is exactly the
        // key `SigningKey::generate` would have produced from the same RNG bytes.
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|e| {
            HeartbeatError::Transport(format!(
                "failed to draw OS entropy for a fresh device key: {e}"
            ))
        })?;
        let key = ed25519_dalek::SigningKey::from_bytes(&seed);
        match Self::install_new(&path, &key.to_bytes())? {
            // We won the create-once race: our key is the durable identity.
            InstallOutcome::Installed => Ok(Self { key }),
            // A concurrent starter won — reload the durable winner (NOT our key).
            InstallOutcome::AlreadyExists => match Self::load_existing(&path)? {
                Some(key) => Ok(Self { key }),
                // Vanished between AlreadyExists and the reload (extremely unlikely;
                // a concurrent delete) — fail closed rather than silently regenerate.
                None => Err(HeartbeatError::Transport(format!(
                    "device key file {} vanished during a concurrent first-boot install; \
                     keeping last-good",
                    path.display()
                ))),
            },
        }
    }

    /// Load an EXISTING device key, fail-closed on a weak-perm / corrupt / unreadable
    /// / symlinked file; `Ok(None)` only when the file is genuinely ABSENT (a fresh
    /// device).
    ///
    /// **Inode-bound, no TOCTOU (panel round-2 major #2):** on Unix the file is
    /// opened ONCE with `O_NOFOLLOW` (a symlink at the path is refused — the classic
    /// swap vector), then the **open fd** is `fstat`'d (via `File::metadata`) to
    /// verify it is a **regular file** with **exactly** mode `0600`, and the bytes
    /// are read from that **same fd**. So the perms checked and the bytes accepted
    /// provably come from one inode — a concurrent replacer cannot swap the path
    /// between the check and the read. A broader mode (e.g. world-readable `0644`)
    /// means the signing secret is exposed → fail closed (never trust, never
    /// regenerate — that would break server-side key continuity).
    fn load_existing(
        path: &std::path::Path,
    ) -> Result<Option<ed25519_dalek::SigningKey>, multiview_licence::heartbeat::HeartbeatError>
    {
        use std::io::Read as _;

        use multiview_licence::heartbeat::HeartbeatError;

        // Open ONCE. On Unix open via `rustix` with `O_NOFOLLOW` so a symlink at the
        // key path is refused (ELOOP) rather than followed — the classic swap vector
        // — and so the fd, the fstat below, and the read all bind to ONE inode (no
        // `unsafe`; rustix is a SAFE wrapper, already a dep for flock). On a non-Unix
        // target the mode concept does not apply — a plain `std::fs` open is used.
        #[cfg(unix)]
        let file: std::fs::File = match rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        ) {
            Ok(fd) => std::fs::File::from(fd),
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(e) => {
                return Err(HeartbeatError::Transport(format!(
                    "could not open the device key file {} (symlinked? unreadable?): {e}",
                    path.display()
                )))
            }
        };
        #[cfg(not(unix))]
        let file: std::fs::File = match std::fs::OpenOptions::new().read(true).open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(HeartbeatError::Transport(format!(
                    "could not open the device key file {}: {e}",
                    path.display()
                )))
            }
        };
        // fstat the OPEN fd (not the path) — binds the checks to this exact inode.
        let meta = file.metadata().map_err(|e| {
            HeartbeatError::Transport(format!(
                "could not stat the open device key file {}: {e}",
                path.display()
            ))
        })?;
        if !meta.is_file() {
            return Err(HeartbeatError::Transport(format!(
                "device key path {} is not a regular file; refusing to use it",
                path.display()
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = meta.permissions().mode() & 0o777;
            if mode != 0o600 {
                return Err(HeartbeatError::Transport(format!(
                    "device key file {} has insecure permissions {mode:#o} (want 0600); \
                     refusing to use an exposed signing secret",
                    path.display()
                )));
            }
        }
        // Read from the SAME fd we fstat'd. Cap the read so a (corrupt) huge file
        // can't balloon memory — the seed is exactly 33 bytes' worth at most; 33
        // lets us detect "too long" as corruption rather than silently truncating.
        let mut bytes = Vec::with_capacity(33);
        file.take(33).read_to_end(&mut bytes).map_err(|e| {
            HeartbeatError::Transport(format!(
                "could not read the device key file {}: {e}",
                path.display()
            ))
        })?;
        // A PRESENT key file MUST be exactly a 32-byte seed — anything else (short,
        // long, empty) is corruption/tamper and fails closed (never a silent
        // regenerate that would change the device identity and break continuity).
        let seed: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
            HeartbeatError::Transport(format!(
                "device key file {} is present but is not a 32-byte seed \
                 (corrupt/tamper); refusing to regenerate a new identity",
                path.display()
            ))
        })?;
        Ok(Some(ed25519_dalek::SigningKey::from_bytes(&seed)))
    }

    /// Atomically install the 32-byte `seed` as the device key, **failing closed**
    /// and **refusing to overwrite** an existing key.
    ///
    /// Protocol (mirrors `FileNonceStore`'s crash-durable write, plus a create-once
    /// guard): write the seed to a UNIQUE temp opened `O_EXCL` at mode `0600` (with
    /// an explicit `fchmod` so a restrictive umask can't widen it and a hostile
    /// pre-existing temp can't be reused — panel major #1), fsync the temp, then
    /// `hard-link` it to the final path (atomic create-once: fails `AlreadyExists`
    /// if a concurrent starter already created the key — panel major #4), unlink the
    /// temp, and fsync the PARENT DIR so the new directory entry survives a crash
    /// (the fsync Result is PROPAGATED, never swallowed — panel major #3 + rule-37).
    /// A non-durable persist would risk a silent identity regenerate on the next
    /// boot, breaking continuity (security-critical, ADR-I007).
    fn install_new(
        path: &std::path::Path,
        seed: &[u8; 32],
    ) -> Result<InstallOutcome, multiview_licence::heartbeat::HeartbeatError> {
        use std::io::Write as _;

        use multiview_licence::heartbeat::HeartbeatError;
        let parent = path.parent().ok_or_else(|| {
            HeartbeatError::Transport(format!(
                "device key path {} has no parent directory",
                path.display()
            ))
        })?;
        // A UNIQUE temp name (pid + a monotonic counter) so concurrent starters never
        // collide on one temp, and a stale broad-perm temp at a fixed name can't be
        // reused. Opened O_EXCL (`create_new`) so we never adopt a pre-existing file.
        let unique = format!(
            "{}.tmp.{}.{}",
            Self::FILE,
            std::process::id(),
            DEVICE_KEY_TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let tmp = parent.join(unique);
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true); // O_EXCL: refuse a pre-existing temp
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(0o600);
        }
        let file = opts.open(&tmp).map_err(|e| {
            HeartbeatError::Transport(format!(
                "could not create the device key temp {}: {e}",
                tmp.display()
            ))
        })?;
        // Belt-and-braces: an explicit chmod so a restrictive (group/other) umask or
        // any inherited mode can't leave the secret broader than 0600.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if let Err(e) = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)) {
                let _ = std::fs::remove_file(&tmp);
                return Err(HeartbeatError::Transport(format!(
                    "could not set 0600 on the device key temp {}: {e}",
                    tmp.display()
                )));
            }
        }
        // Write + fsync the temp BEFORE linking it into place, so the linked-in file
        // is already on stable storage. On any failure, clean up the temp.
        let write_then_sync = (|| -> std::io::Result<()> {
            let mut file = file;
            file.write_all(seed)?;
            file.sync_all()?;
            Ok(())
        })();
        if let Err(e) = write_then_sync {
            let _ = std::fs::remove_file(&tmp);
            return Err(HeartbeatError::Transport(format!(
                "could not write/fsync the device key temp {}: {e}",
                tmp.display()
            )));
        }
        // Atomic create-once: hard-link the temp to the final path. `link` fails with
        // AlreadyExists if a concurrent starter already created the key — we then
        // report AlreadyExists (the caller reloads the winner). Any OTHER error fails
        // closed. The temp is always unlinked afterwards.
        let link_result = std::fs::hard_link(&tmp, path);
        let _ = std::fs::remove_file(&tmp);
        match link_result {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Ok(InstallOutcome::AlreadyExists);
            }
            Err(e) => {
                return Err(HeartbeatError::Transport(format!(
                    "could not install the device key file {}: {e}",
                    path.display()
                )));
            }
        }
        // fsync the PARENT DIR so the new directory entry is durable (a crash right
        // after link must not lose the device identity). PROPAGATE every failure —
        // never swallow it (panel major #3 + rule-37): a non-durable identity risks a
        // silent regenerate that breaks server-side key continuity.
        let dir = std::fs::File::open(parent).map_err(|e| {
            HeartbeatError::Transport(format!(
                "could not open the lease-state dir {} to fsync: {e}",
                parent.display()
            ))
        })?;
        dir.sync_all().map_err(|e| {
            HeartbeatError::Transport(format!(
                "could not fsync the lease-state dir {}: {e}",
                parent.display()
            ))
        })?;
        Ok(InstallOutcome::Installed)
    }
}

/// The outcome of [`DeviceKeyStore::install_new`]: whether THIS call created the
/// durable key, or a concurrent starter already had (so the caller reloads it).
#[cfg(feature = "heartbeat")]
enum InstallOutcome {
    /// This call atomically created + durably persisted the key.
    Installed,
    /// A concurrent starter already created the key — reload the winner.
    AlreadyExists,
}

/// A monotonic counter making each device-key temp name unique within a process
/// (combined with the pid), so concurrent installs never collide on one temp.
#[cfg(feature = "heartbeat")]
static DEVICE_KEY_TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(feature = "heartbeat")]
impl multiview_licence::heartbeat::DeviceSigner for DeviceKeyStore {
    fn public_key_raw(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }
    fn sign(&self, message: &[u8]) -> [u8; 64] {
        use ed25519_dalek::Signer as _;
        self.key.sign(message).to_bytes()
    }
}

/// Load (or, on first boot, generate + persist) the device keypair from `dir`
/// and return its **raw 32-byte Ed25519 public key**.
///
/// This is the display-node bootstrap's reuse seam (DEV-B5 / ADR-0045): a node
/// shares the *same* persisted device identity the licence heartbeat already
/// loads ([`DeviceKeyStore`] — atomic create-once, `0600`, `O_NOFOLLOW`,
/// TOCTOU-safe), so there is one device key per box, never a second RNG/IO/file
/// (rule 6). The node uses only the **public** half — for its enrollment
/// request and its display pairing code.
///
/// # Errors
/// [`HeartbeatError`](multiview_licence::heartbeat::HeartbeatError) when the key
/// directory cannot be created, a present key file is corrupt / unreadable / at
/// weak permissions, or a freshly-generated key cannot be durably persisted —
/// all fail closed (never silently regenerate, which would break server-side
/// key continuity).
#[cfg(feature = "heartbeat")]
pub fn load_device_public_key(
    dir: &str,
) -> Result<[u8; 32], multiview_licence::heartbeat::HeartbeatError> {
    use multiview_licence::heartbeat::DeviceSigner as _;
    let store = DeviceKeyStore::load_or_generate(dir)?;
    Ok(store.public_key_raw())
}

/// The live Conspect HTTP transport (the cli-boundary implementation of
/// [`multiview_licence::heartbeat::LicenceServer`]). Owns the `reqwest` (rustls)
/// client so the leaf crate stays socket-free; carries the account JWT bearer +
/// a fresh `Idempotency-Key` on every mutation (ADR-0096 D2: account-JWT auth
/// today; device-PoP request-signing deferred). IPv6-first via `reqwest`'s
/// resolver; HTTPS only.
#[cfg(feature = "heartbeat")]
struct ConspectHttpServer {
    client: reqwest::Client,
    api_base: String,
    bearer_token: String,
}

#[cfg(feature = "heartbeat")]
impl ConspectHttpServer {
    /// Build the HTTPS-only transport. **Fails closed (round-6 panel):** the
    /// `https_only(true)` client is propagated with `?` — NOT `unwrap_or_default()`,
    /// which would silently fall back to a DEFAULT (non-HTTPS-only) client while
    /// every request still attaches the bearer JWT, leaking the account credential
    /// over plaintext `http://`. A failed build disables the heartbeat (the caller
    /// keeps last-good), never a credential-carrying non-HTTPS client.
    ///
    /// # Errors
    /// [`HeartbeatError::Transport`] if the HTTPS-only `reqwest` client cannot be
    /// built.
    fn new(
        mut api_base: String,
        bearer_token: String,
    ) -> Result<Self, multiview_licence::heartbeat::HeartbeatError> {
        use multiview_licence::heartbeat::HeartbeatError;
        let client = reqwest::Client::builder()
            .user_agent(concat!("multiview/", env!("CARGO_PKG_VERSION")))
            .https_only(true)
            .build()
            .map_err(|e| {
                HeartbeatError::Transport(format!("could not build the HTTPS-only client: {e}"))
            })?;
        // Normalise the base URL in place (consume the owned String — no realloc).
        api_base.truncate(api_base.trim_end_matches('/').len());
        Ok(Self {
            client,
            api_base,
            bearer_token,
        })
    }

    /// `GET` + JSON-decode against `url`, mapping every failure to a
    /// [`HeartbeatError`] the loop treats as "keep last-good".
    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: String,
    ) -> Result<T, multiview_licence::heartbeat::HeartbeatError> {
        use multiview_licence::heartbeat::HeartbeatError;
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.bearer_token)
            .send()
            .await
            .map_err(|e| HeartbeatError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(HeartbeatError::Transport(format!(
                "{} returned HTTP {}",
                url,
                resp.status()
            )));
        }
        resp.json::<T>()
            .await
            .map_err(|e| HeartbeatError::Malformed(e.to_string()))
    }

    /// `POST` the EXACT `body` bytes (content-type `application/json`) with the
    /// required `Idempotency-Key` and `Conspect-Device-PoP` headers, JSON-decode the
    /// response. The body is sent verbatim (NOT re-serialised) so it is byte-for-byte
    /// the bytes the device-PoP `sha256(body)` covered (ADR-I007 — no drift).
    ///
    /// **`Malformed` is a received-2xx contact (the load-bearing retry contract).**
    /// This fulfils the [`LicenceServer::heartbeat`] error-mapping contract that
    /// `run_once` relies on. The status is classified FIRST: a no-response send failure
    /// maps to [`Transport`] (the ambiguous, replay-only arm) before the status is even
    /// read; then a non-2xx is mapped by `heartbeat_status_error` (a `4xx` → definitive
    /// [`ServerRejected`], a `5xx` → ambiguous [`Transport`]) and returns before the
    /// body is touched. The body is decoded **only on the success path**, so the
    /// [`Malformed`] this returns can ONLY follow a RECEIVED `2xx` whose body would not
    /// parse — never a no-response or `5xx` failure. A `2xx` means the server processed
    /// the request and **burned the single-use PoP nonce**, so the leaf treats this
    /// `Malformed` as DEFINITIVE (like a `4xx`): it drops the pinned attempt + the
    /// burned nonce and recovers with a fresh `/challenge` next cycle — it is never
    /// replayed. Keeping this ordering is what makes that inference sound; do not decode
    /// the body before the status check, and never return `Malformed` for a pre-send or
    /// no-response error.
    ///
    /// [`LicenceServer::heartbeat`]: multiview_licence::heartbeat::LicenceServer::heartbeat
    /// [`ServerRejected`]: multiview_licence::heartbeat::HeartbeatError::ServerRejected
    /// [`Transport`]: multiview_licence::heartbeat::HeartbeatError::Transport
    /// [`Malformed`]: multiview_licence::heartbeat::HeartbeatError::Malformed
    async fn post_raw_json<T: serde::de::DeserializeOwned>(
        &self,
        url: String,
        body: Vec<u8>,
        idempotency_key: &str,
        pop_header: &str,
    ) -> Result<T, multiview_licence::heartbeat::HeartbeatError> {
        use multiview_licence::heartbeat::HeartbeatError;
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.bearer_token)
            .header("Idempotency-Key", idempotency_key)
            .header("Conspect-Device-PoP", pop_header)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| HeartbeatError::Transport(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            // STATUS-AWARE retry (ADR-I007 §8, round 3): a RECEIVED 4xx is a DEFINITIVE
            // server rejection (the server processed + rejected THIS request — 401
            // pop-invalid/pop-required, 409 idempotency/body-mismatch); the single-use PoP
            // nonce is burned, so the leaf drops the pinned attempt + nonce and recovers
            // with a fresh challenge (never replay a burned nonce). A 5xx is AMBIGUOUS (the
            // mutation may have committed) → Transport, replayed verbatim under the same
            // Idempotency-Key (the server dedupes; a burned nonce then surfaces as a 4xx
            // next cycle → recovery). A no-response transport error stays Transport (above).
            return Err(heartbeat_status_error(status, &url));
        }
        resp.json::<T>()
            .await
            .map_err(|e| HeartbeatError::Malformed(e.to_string()))
    }

    /// `POST` for the **lifecycle** verbs (rebind/deactivate, ADR-I009) — identical to
    /// [`post_raw_json`](Self::post_raw_json) EXCEPT a received `409` maps to
    /// [`Transport`](multiview_licence::heartbeat::HeartbeatError::Transport) (replay
    /// the SAME idempotency-key), NOT
    /// [`ServerRejected`](multiview_licence::heartbeat::HeartbeatError::ServerRejected).
    ///
    /// The live rebind/deactivate `409` is overloaded — "the same Idempotency-Key is
    /// still in progress" (both verbs), plus, for rebind, "rebinds-exhausted" /
    /// "discriminator-mismatch" / "no-live-instance" — and carries **no JSON body to
    /// disambiguate**. Replaying the SAME idempotency-key + body is the only choice
    /// correct for all of them: the server dedups, so an exhausted/mismatch re-POST
    /// returns the same `409` deterministically with **no second rebind charge**, and an
    /// in-progress request completes. Mapping `409 → ServerRejected` (drop + mint a
    /// fresh key) would double-charge the scarce 3-free-per-year budget on an ambiguous
    /// failure. The other 4xx (`401`/`403`/`404`/`422`) stay definitive `ServerRejected`;
    /// a `5xx`/no-response stays `Transport`; a `2xx` with a bad body stays `Malformed`.
    async fn post_raw_json_lifecycle<T: serde::de::DeserializeOwned>(
        &self,
        url: String,
        body: Vec<u8>,
        idempotency_key: &str,
        pop_header: &str,
    ) -> Result<T, multiview_licence::heartbeat::HeartbeatError> {
        use multiview_licence::heartbeat::HeartbeatError;
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.bearer_token)
            .header("Idempotency-Key", idempotency_key)
            .header("Conspect-Device-PoP", pop_header)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| HeartbeatError::Transport(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(lifecycle_status_error(status, &url));
        }
        resp.json::<T>()
            .await
            .map_err(|e| HeartbeatError::Malformed(e.to_string()))
    }
}

/// Classify a non-2xx **lifecycle** (rebind/deactivate) response (ADR-I009). Differs
/// from [`heartbeat_status_error`] in ONE way: a `409` maps to
/// [`Transport`](multiview_licence::heartbeat::HeartbeatError::Transport) (an ambiguous
/// "retry the same idempotency-key" — the overloaded, body-less lifecycle `409`), so an
/// in-progress / exhausted / mismatch `409` replays the SAME key (the server dedups; no
/// second rebind charge) instead of dropping the pinned attempt and minting a fresh key.
/// Every OTHER 4xx is a DEFINITIVE [`ServerRejected`] (`401`/`403`/`404`/`422` — the
/// burned-nonce recovery path); a 5xx is ambiguous [`Transport`].
///
/// [`ServerRejected`]: multiview_licence::heartbeat::HeartbeatError::ServerRejected
#[cfg(feature = "heartbeat")]
fn lifecycle_status_error(
    status: reqwest::StatusCode,
    url: &str,
) -> multiview_licence::heartbeat::HeartbeatError {
    use multiview_licence::heartbeat::HeartbeatError;
    let detail = format!("{url} returned HTTP {status}");
    if status == reqwest::StatusCode::CONFLICT {
        // 409: overloaded + body-less → replay the same idempotency-key (no double charge).
        HeartbeatError::Transport(detail)
    } else if status.is_client_error() {
        HeartbeatError::ServerRejected(detail)
    } else {
        HeartbeatError::Transport(detail)
    }
}

/// Classify a non-2xx heartbeat-POST response into the STATUS-AWARE retry error
/// (ADR-I007 §8, round 3). A RECEIVED 4xx client error is a DEFINITIVE server rejection —
/// the server processed and rejected this exact request (401 `pop-invalid`/`pop-required`,
/// 409 idempotency/body-mismatch) — so the single-use PoP nonce is burned and the leaf
/// discards the pinned retry attempt ([`HeartbeatError::ServerRejected`]) and recovers with
/// a fresh `/challenge` next cycle (never replay a burned nonce). A 5xx is AMBIGUOUS (the
/// mutation may have committed before the server errored) → [`HeartbeatError::Transport`],
/// replayed verbatim under the same `Idempotency-Key` (the server dedupes; a burned nonce
/// then surfaces as a 4xx next cycle → recovery).
#[cfg(feature = "heartbeat")]
fn heartbeat_status_error(
    status: reqwest::StatusCode,
    url: &str,
) -> multiview_licence::heartbeat::HeartbeatError {
    use multiview_licence::heartbeat::HeartbeatError;
    let detail = format!("{url} returned HTTP {status}");
    if status.is_client_error() {
        HeartbeatError::ServerRejected(detail)
    } else {
        HeartbeatError::Transport(detail)
    }
}

#[cfg(feature = "heartbeat")]
impl multiview_licence::heartbeat::LicenceServer for ConspectHttpServer {
    async fn fetch_keys(
        &self,
    ) -> Result<
        multiview_licence::heartbeat::LicensingKeys,
        multiview_licence::heartbeat::HeartbeatError,
    > {
        // The well-known document is served from the API host root, not under the
        // versioned `/v0` base path.
        let host = self
            .api_base
            .rsplit_once("/v0")
            .map_or(self.api_base.as_str(), |(h, _)| h);
        self.get_json(format!("{host}/.well-known/conspect-licensing-keys.json"))
            .await
    }

    async fn fetch_challenge(
        &self,
        org: &str,
    ) -> Result<
        multiview_licence::heartbeat::DeviceChallenge,
        multiview_licence::heartbeat::HeartbeatError,
    > {
        // GET /v0/devices/licence/challenge?orgId={org} (account-JWT bearer; the
        // operator role floor). The org id is URL-encoded so a hostile value cannot
        // break out of the query.
        let org_q = urlencode_query(org);
        self.get_json(format!(
            "{}/devices/licence/challenge?orgId={org_q}",
            self.api_base
        ))
        .await
    }

    async fn heartbeat(
        &self,
        org: &str,
        body: Vec<u8>,
        idempotency_key: &str,
        pop_header: &str,
    ) -> Result<
        multiview_licence::heartbeat::HeartbeatResponse,
        multiview_licence::heartbeat::HeartbeatError,
    > {
        self.post_raw_json(
            format!("{}/organisations/{org}/heartbeat", self.api_base),
            body,
            idempotency_key,
            pop_header,
        )
        .await
    }

    async fn activate(
        &self,
        org: &str,
        body: Vec<u8>,
        idempotency_key: &str,
        pop_header: &str,
    ) -> Result<
        multiview_licence::heartbeat::ActivateResponse,
        multiview_licence::heartbeat::HeartbeatError,
    > {
        // POST /organisations/{org}/activate — the first-contact enrolment (ADR-I008).
        // Identical transport discipline to `heartbeat`: the EXACT body bytes the PoP
        // signed over are sent verbatim, with the Idempotency-Key + Conspect-Device-PoP
        // headers, and `post_raw_json` upholds the burned-nonce error-mapping contract
        // (4xx → ServerRejected, 5xx/no-response → Transport, Malformed only after 2xx).
        self.post_raw_json(
            format!("{}/organisations/{org}/activate", self.api_base),
            body,
            idempotency_key,
            pop_header,
        )
        .await
    }

    async fn rebind(
        &self,
        org: &str,
        body: Vec<u8>,
        idempotency_key: &str,
        pop_header: &str,
    ) -> Result<
        multiview_licence::heartbeat::RebindResponse,
        multiview_licence::heartbeat::HeartbeatError,
    > {
        // POST /organisations/{org}/rebind (ADR-I009) — the device lifecycle rebind.
        // Verbatim body + Idempotency-Key + Conspect-Device-PoP; the LIFECYCLE transport
        // helper maps 409 -> Transport (replay-same-key, no double rebind charge), other
        // 4xx -> ServerRejected, 5xx/no-response -> Transport, Malformed only after 2xx.
        self.post_raw_json_lifecycle(
            format!("{}/organisations/{org}/rebind", self.api_base),
            body,
            idempotency_key,
            pop_header,
        )
        .await
    }

    async fn deactivate(
        &self,
        org: &str,
        body: Vec<u8>,
        idempotency_key: &str,
        pop_header: &str,
    ) -> Result<
        multiview_licence::heartbeat::DeactivateResponse,
        multiview_licence::heartbeat::HeartbeatError,
    > {
        // POST /organisations/{org}/deactivate (ADR-I009) — the device lifecycle seat
        // surrender. Same lifecycle transport discipline as `rebind` (409 -> Transport).
        self.post_raw_json_lifecycle(
            format!("{}/organisations/{org}/deactivate", self.api_base),
            body,
            idempotency_key,
            pop_header,
        )
        .await
    }
}

/// Minimal URL query-component percent-encoding for the `orgId` value: encode every
/// byte that is not an RFC 3986 unreserved character (`A-Za-z0-9-._~`). Total +
/// panic-free; keeps a hostile org id from breaking out of the query string.
#[cfg(feature = "heartbeat")]
fn urlencode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push(char::from(hex_upper_nibble(b >> 4)));
            out.push(char::from(hex_upper_nibble(b & 0x0f)));
        }
    }
    out
}

/// The upper-case hex digit for a nibble `0..=15` (total; `>15` clamps to `'0'`,
/// which cannot occur for a masked nibble).
#[cfg(feature = "heartbeat")]
fn hex_upper_nibble(n: u8) -> u8 {
    match n {
        0..=9 => b'0' + n,
        10..=15 => b'A' + (n - 10),
        _ => b'0',
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "heartbeat")]
    #[test]
    fn a_received_4xx_is_a_definitive_server_rejection_5xx_is_ambiguous_transport() {
        use multiview_licence::heartbeat::HeartbeatError;
        // ADR-I007 §8 round-3: a RECEIVED 4xx (401 pop-invalid/pop-required, 409
        // idempotency/body-mismatch, and other client errors) is DEFINITIVE → ServerRejected,
        // so the leaf discards the burned nonce + pinned attempt and recovers with a fresh
        // challenge; it must NOT be replayed verbatim.
        for code in [400u16, 401, 403, 404, 409, 422] {
            let status = reqwest::StatusCode::from_u16(code).unwrap();
            assert!(
                matches!(
                    heartbeat_status_error(status, "https://x/y"),
                    HeartbeatError::ServerRejected(_)
                ),
                "HTTP {code} (a received client-error response) must be a definitive ServerRejected",
            );
        }
        // A 5xx is AMBIGUOUS (the mutation may have committed) → Transport, so the SAME
        // idempotency-keyed body + nonce is replayed verbatim (the server dedupes).
        for code in [500u16, 502, 503, 504] {
            let status = reqwest::StatusCode::from_u16(code).unwrap();
            assert!(
                matches!(
                    heartbeat_status_error(status, "https://x/y"),
                    HeartbeatError::Transport(_)
                ),
                "HTTP {code} (a server error) must stay ambiguous Transport (replay-safe)",
            );
        }
    }

    #[test]
    fn a_clean_signal_does_not_watermark() {
        assert!(!WatermarkSignal::clean().watermark());
        assert!(!WatermarkSignal::default().watermark());
    }

    #[test]
    fn a_watermark_rung_signals_a_watermark() {
        assert!(WatermarkSignal::at(EnforcementLevel::Watermark).watermark());
        assert!(WatermarkSignal::at(EnforcementLevel::BlockNewInstance).watermark());
        assert!(WatermarkSignal::at(EnforcementLevel::UnlicensedBuild).watermark());
        assert!(!WatermarkSignal::at(EnforcementLevel::ConfigLocked).watermark());
        assert!(!WatermarkSignal::at(EnforcementLevel::Warning).watermark());
    }

    #[test]
    fn set_publishes_a_new_level_read_wait_free() {
        let signal = WatermarkSignal::clean();
        assert!(!signal.watermark());
        signal.set(EnforcementLevel::Watermark);
        assert!(
            signal.watermark(),
            "an off-thread set is visible to the next read"
        );
        assert_eq!(signal.level(), EnforcementLevel::Watermark);
    }

    #[test]
    fn refresh_from_an_empty_store_is_clean() {
        let store = LeaseStore::new();
        let signal = WatermarkSignal::at(EnforcementLevel::Watermark);
        refresh_signal(&store, &signal);
        assert!(
            !signal.watermark(),
            "an empty store fails toward leniency: no lapse evidence → clean"
        );
        assert_eq!(current_level(&store), None);
    }

    #[test]
    fn decode_hex_round_trips_and_rejects_garbage() {
        assert_eq!(decode_hex("00ff10"), Some(vec![0x00, 0xff, 0x10]));
        assert_eq!(decode_hex("ABcd"), Some(vec![0xab, 0xcd]));
        assert_eq!(decode_hex(""), Some(vec![]));
        // Odd length and non-hex are rejected (never a panic).
        assert_eq!(decode_hex("abc"), None);
        assert_eq!(decode_hex("zz"), None);
        assert_eq!(decode_hex("0x10"), None);
    }

    // --- Round-6 BLOCKER 1: the file-backed durable nonce FAILS CLOSED. ----------

    #[cfg(feature = "heartbeat")]
    #[test]
    fn file_nonce_store_load_is_ok_zero_when_absent() {
        // A fresh device legitimately has no nonce file → 0 is a correct, trusted
        // value (NOT an error): the in-process monotone guard covers same-lifetime
        // reuse and there is no prior lifetime to collide with.
        use multiview_licence::heartbeat::NonceStore as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FileNonceStore::in_dir(dir.path().to_str().expect("utf8 path"))
            .expect("first owner takes the lock");
        assert_eq!(
            store
                .load()
                .expect("an absent nonce file loads as a trusted 0"),
            0
        );
    }

    #[cfg(feature = "heartbeat")]
    #[test]
    fn file_nonce_store_load_errors_on_a_corrupt_file() {
        // A PRESENT but unparseable nonce file must NOT be silently trusted as 0 —
        // that would reset the high-water and re-mint a colliding key after restart.
        // load() returns an Error so the mint fails closed.
        use multiview_licence::heartbeat::NonceStore as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("idempotency-nonce");
        std::fs::write(&path, "not-a-number").expect("seed corrupt nonce");
        let store = FileNonceStore::in_dir(dir.path().to_str().expect("utf8 path"))
            .expect("first owner takes the lock");
        assert!(
            store.load().is_err(),
            "a present-but-corrupt nonce file must load as an Error, never a silent 0"
        );
    }

    #[cfg(feature = "heartbeat")]
    #[test]
    fn file_nonce_store_commit_errors_when_unwritable() {
        // commit() must PROPAGATE a write/rename failure (not log-and-continue): an
        // un-persisted high-water is exactly the cross-restart collision risk. After
        // a clean open, make the data path unwritable by replacing the nonce-file
        // location with a DIRECTORY (so write to it fails), and assert commit errors.
        use multiview_licence::heartbeat::NonceStore as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FileNonceStore::in_dir(dir.path().to_str().expect("utf8 path"))
            .expect("first owner takes the lock");
        // Replace the would-be nonce data file with a directory of the same name:
        // write-temp succeeds but the rename onto a non-empty directory fails.
        std::fs::create_dir(dir.path().join("idempotency-nonce")).expect("dir in the way");
        std::fs::create_dir(dir.path().join("idempotency-nonce").join("busy"))
            .expect("make the dir non-empty so rename-onto-it fails");
        assert!(
            store.commit(1).is_err(),
            "an unwritable nonce path must make commit() return an Error"
        );
    }

    #[cfg(feature = "heartbeat")]
    #[test]
    fn file_nonce_store_round_trips_a_committed_value() {
        // The happy path still works: a committed value reloads exactly.
        use multiview_licence::heartbeat::NonceStore as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FileNonceStore::in_dir(dir.path().to_str().expect("utf8 path"))
            .expect("first owner takes the lock");
        store.commit(7).expect("commit persists");
        assert_eq!(store.load().expect("reload"), 7);
    }

    #[cfg(feature = "heartbeat")]
    #[test]
    fn a_second_nonce_owner_on_the_same_dir_is_refused_fail_closed() {
        // INTERPROCESS GUARD (round-6 residual): two minters sharing one lease-state
        // dir could load the same high-water and mint COLLIDING keys. The first owner
        // holds a non-blocking exclusive advisory lock; a second `in_dir` on the SAME
        // dir is refused, so its heartbeat declines to start (fail closed). Releasing
        // the first (drop) then lets a new owner acquire it.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_str().expect("utf8 path");
        let first = FileNonceStore::in_dir(path).expect("first owner takes the lock");
        assert!(
            FileNonceStore::in_dir(path).is_err(),
            "a second owner on the same lease dir must be refused (no colliding minters)"
        );
        drop(first);
        // Once the first owner releases the lock, a fresh owner can take it.
        assert!(
            FileNonceStore::in_dir(path).is_ok(),
            "after the first owner drops, the lock is available again"
        );
    }

    // --- Round-8 MAJOR 2: a PRESENT '0' is untrustworthy (commit never writes 0).

    #[cfg(feature = "heartbeat")]
    #[test]
    fn file_nonce_store_load_rejects_a_present_zero() {
        // `commit` only ever persists `guard.counter.max(durable).saturating_add(1)`,
        // which is >= 1, so the durable file NEVER legitimately contains 0. A PRESENT
        // file whose contents parse to 0 can therefore only be truncation / a partial
        // write / tampering — trusting it would reset the high-water and re-mint
        // mv-{machine}-1 after a restart. load() must reject a present 0 (Err). Only
        // an ABSENT file (NotFound) is the trusted-zero fresh start.
        use multiview_licence::heartbeat::NonceStore as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("idempotency-nonce");
        std::fs::write(&path, "0").expect("seed a present zero");
        let store = FileNonceStore::in_dir(dir.path().to_str().expect("utf8 path"))
            .expect("first owner takes the lock");
        assert!(
            store.load().is_err(),
            "a PRESENT '0' must be rejected (commit never writes 0; only an absent file is a \
             trusted 0)"
        );
        // Sanity: any value >= 1 still loads fine (whitespace-tolerant).
        std::fs::write(&path, " 1\n").expect("seed a one");
        assert_eq!(store.load().expect("a present >=1 loads"), 1);
        std::fs::write(&path, "42").expect("seed 42");
        assert_eq!(store.load().expect("a present 42 loads"), 42);
    }

    #[cfg(feature = "heartbeat")]
    #[test]
    fn file_nonce_store_commit_errors_when_the_temp_write_fails() {
        // The OTHER commit-failure branch: the write-temp step itself fails (the
        // existing test only forces the rename). Put a DIRECTORY at the temp path so
        // `std::fs::write(<dir>/idempotency-nonce.tmp, ..)` fails, and assert commit
        // propagates a NonceError (not log-and-continue).
        use multiview_licence::heartbeat::NonceStore as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FileNonceStore::in_dir(dir.path().to_str().expect("utf8 path"))
            .expect("first owner takes the lock");
        // The temp path is `<path>.tmp` = `<dir>/idempotency-nonce.tmp`.
        std::fs::create_dir(dir.path().join("idempotency-nonce.tmp"))
            .expect("a dir in the way of the temp write");
        assert!(
            store.commit(1).is_err(),
            "a failing temp write must make commit() return an Error"
        );
    }

    // --- Round-6 MAJOR: the HTTP transport is HTTPS-only and FAILS CLOSED. -------

    #[cfg(feature = "heartbeat")]
    #[tokio::test]
    async fn the_transport_refuses_a_plaintext_http_base_https_only() {
        // The client must NEVER attach the bearer JWT to a plaintext http:// request
        // (credential leak). The reqwest client is built https_only(true) and the
        // constructor FAILS CLOSED (no unwrap_or_default fallback that would drop
        // https_only), so a heartbeat against an http:// base is rejected at the
        // transport with NO plaintext request leaving the host.
        use multiview_licence::heartbeat::LicenceServer as _;
        let server = ConspectHttpServer::new(
            "http://insecure.example.invalid/v0".to_owned(),
            "super-secret-bearer".to_owned(),
        )
        .expect("an https-only client builds");
        // The body bytes + a (dummy) PoP header — the https-only client must refuse
        // the http:// base BEFORE any request leaves the host (so the dummy proof is
        // never actually presented).
        let body = br#"{"bindingId":"ib_x"}"#.to_vec();
        let res = server
            .heartbeat("org_x", body, "mv-m-1", "g1g-dummy-pop")
            .await;
        assert!(
            res.is_err(),
            "an http:// base must be refused by the https-only client (no plaintext \
             credential-carrying request)"
        );
    }

    // --- CONSPECT-3 device-PoP: the durable device keypair (ADR-I007). -----------

    #[cfg(feature = "heartbeat")]
    #[test]
    fn device_key_store_generates_once_then_reloads_a_stable_identity() {
        // The device keypair must be generated ONCE and persist across restarts —
        // a stable device identity (the server verifies continuity against the
        // STORED key). A second open of the same dir reloads the SAME public key,
        // never a freshly-generated one.
        use multiview_licence::heartbeat::DeviceSigner as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_str().expect("utf8 path");
        let first = DeviceKeyStore::load_or_generate(path).expect("first generate");
        let pk1 = first.public_key_raw();
        // A second open (simulating a restart) reloads the persisted key.
        let second = DeviceKeyStore::load_or_generate(path).expect("reload persisted");
        assert_eq!(
            pk1,
            second.public_key_raw(),
            "the device key must be generate-once-then-reuse (stable identity across restart)"
        );
    }

    #[cfg(all(feature = "heartbeat", unix))]
    #[test]
    fn device_key_store_persists_with_owner_only_permissions() {
        // The private seed on disk must be 0600 (owner read/write only) — it is the
        // device's signing secret.
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_str().expect("utf8 path");
        let _ = DeviceKeyStore::load_or_generate(path).expect("generate");
        let key_path = dir.path().join("device-key.ed25519");
        let mode = std::fs::metadata(&key_path)
            .expect("key file exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the device key seed must be 0600 (owner-only)");
    }

    #[cfg(feature = "heartbeat")]
    #[test]
    fn device_key_store_signs_verifiably_with_the_persisted_key() {
        // The loaded signer must sign with the SAME key it persisted — a signature
        // verifies against the reloaded public key (continuity).
        use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};
        use multiview_licence::heartbeat::DeviceSigner as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_str().expect("utf8 path");
        let signer = DeviceKeyStore::load_or_generate(path).expect("generate");
        let msg = b"the COSE Sig_structure bytes";
        let sig = signer.sign(msg);
        let vk = VerifyingKey::from_bytes(&signer.public_key_raw()).expect("vk");
        vk.verify(msg, &Signature::from_bytes(&sig))
            .expect("the persisted key's signature must verify against its public key");
    }

    #[cfg(feature = "heartbeat")]
    #[test]
    fn device_key_store_fails_closed_on_a_corrupt_key_file() {
        // A PRESENT-but-corrupt key file must FAIL CLOSED (Err), never silently
        // regenerate a NEW identity — that would break server-side key continuity
        // (the server would reject the new key as a different device). The caller
        // then declines to start the heartbeat and keeps last-good (never off air).
        let dir = tempfile::tempdir().expect("tempdir");
        let key_path = dir.path().join("device-key.ed25519");
        std::fs::write(&key_path, b"not-a-valid-32-byte-seed").expect("seed corrupt key");
        let res = DeviceKeyStore::load_or_generate(dir.path().to_str().expect("utf8 path"));
        assert!(
            res.is_err(),
            "a present-but-corrupt device key file must fail closed, never regenerate a new identity"
        );
    }

    // --- Panel majors (ADR-I007 / Codex 3-lens): device-key file lifecycle. ------

    #[cfg(all(feature = "heartbeat", unix))]
    #[test]
    fn device_key_generate_yields_0600_even_when_a_broad_perm_temp_pre_exists() {
        // SECRET EXPOSURE (panel major #1): a pre-existing device-key.tmp with broad
        // perms must NOT leave the final seed world-readable. The persist must use a
        // UNIQUE temp + O_EXCL/create_new + explicit 0600, so a stale broad-perm temp
        // cannot be truncated-and-renamed keeping its old mode.
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        // Plant a stale, world-readable temp at the fixed legacy name.
        let stale_tmp = dir.path().join("device-key.tmp");
        std::fs::write(&stale_tmp, b"junk").expect("plant stale temp");
        std::fs::set_permissions(&stale_tmp, std::fs::Permissions::from_mode(0o644))
            .expect("chmod stale temp 0644");
        let _ = DeviceKeyStore::load_or_generate(dir.path().to_str().expect("utf8 path"))
            .expect("generate succeeds despite the stale temp");
        let key_path = dir.path().join("device-key.ed25519");
        let mode = std::fs::metadata(&key_path)
            .expect("key file exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "the persisted seed must be 0600 even when a broad-perm temp pre-existed"
        );
    }

    #[cfg(all(feature = "heartbeat", unix))]
    #[test]
    fn device_key_load_fails_closed_on_a_weak_perm_existing_key() {
        // WEAK-PERM KEY (panel major #2): an existing device-key.ed25519 that is NOT
        // 0600 (e.g. 0644 — world-readable signing secret) must FAIL CLOSED rather
        // than be trusted as the signing identity. A valid 32-byte seed at 0644 is
        // the hostile case (it parses fine; only the perms are wrong).
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let key_path = dir.path().join("device-key.ed25519");
        std::fs::write(&key_path, [7u8; 32]).expect("seed a valid 32-byte key");
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o644))
            .expect("chmod key 0644");
        let res = DeviceKeyStore::load_or_generate(dir.path().to_str().expect("utf8 path"));
        assert!(
            res.is_err(),
            "a world-readable (0644) device key must fail closed, never be trusted as the signing identity"
        );
    }

    #[cfg(feature = "heartbeat")]
    #[test]
    fn device_key_concurrent_first_boot_yields_one_durable_winner() {
        // CONCURRENT FIRST-BOOT RACE (panel major #4): N processes starting at once
        // must converge on ONE persisted key, and EVERY returned signer's public key
        // must equal the key on disk (no signer whose seed isn't the durable one).
        // The atomic install refuses to overwrite an existing key, then reloads the
        // winner.
        use multiview_licence::heartbeat::DeviceSigner as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_str().expect("utf8 path").to_owned();
        let mut handles = Vec::new();
        for _ in 0..8 {
            let p = path.clone();
            handles.push(std::thread::spawn(move || {
                DeviceKeyStore::load_or_generate(&p).map(|s| s.public_key_raw())
            }));
        }
        let pubkeys: Vec<[u8; 32]> = handles
            .into_iter()
            .map(|h| h.join().expect("thread").expect("load_or_generate"))
            .collect();
        // The durable winner on disk.
        let on_disk: [u8; 32] = std::fs::read(dir.path().join("device-key.ed25519"))
            .expect("a key was persisted")
            .as_slice()
            .try_into()
            .expect("32-byte seed");
        let winner = ed25519_dalek::SigningKey::from_bytes(&on_disk)
            .verifying_key()
            .to_bytes();
        for pk in &pubkeys {
            assert_eq!(
                *pk, winner,
                "every concurrent starter must return the signer whose seed is the one durably on disk"
            );
        }
    }

    #[cfg(feature = "heartbeat")]
    #[test]
    fn device_key_persist_fails_closed_when_the_parent_dir_cannot_be_fsynced() {
        // CRASH-DURABILITY (panel major #3): the parent-dir fsync result must be
        // PROPAGATED, not swallowed. We can't easily force a dir fsync failure
        // portably, so assert the weaker observable contract the fix guarantees: a
        // freshly-generated key is durably present AND readable back as the same
        // 32-byte seed the signer holds (the happy path the durable protocol must
        // leave behind). The swallow-bug regression is caught by code review +
        // the rule-37 lint; this pins the durable round-trip.
        use multiview_licence::heartbeat::DeviceSigner as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let signer = DeviceKeyStore::load_or_generate(dir.path().to_str().expect("utf8 path"))
            .expect("generate");
        let on_disk: [u8; 32] = std::fs::read(dir.path().join("device-key.ed25519"))
            .expect("seed durably present after generate")
            .as_slice()
            .try_into()
            .expect("32-byte seed");
        assert_eq!(
            ed25519_dalek::SigningKey::from_bytes(&on_disk)
                .verifying_key()
                .to_bytes(),
            signer.public_key_raw(),
            "the durably-persisted seed must be exactly the one the returned signer holds"
        );
    }

    #[cfg(all(feature = "heartbeat", unix))]
    #[test]
    fn device_key_load_rejects_a_symlink_at_the_key_path() {
        // LOAD-SIDE TOCTOU (panel round-2 major #2): the perm-check + read must bind
        // to the SAME opened inode, and a symlink at the key path must be rejected
        // explicitly (a symlink is the classic swap vector — its target can be
        // replaced between checks, and following it reads bytes from an inode whose
        // mode we never verified). A symlink → a perfectly-valid 0600 seed elsewhere
        // must still be refused (we will not follow it).
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real-seed");
        std::fs::write(&real, [9u8; 32]).expect("write a valid seed elsewhere");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600))
                .expect("0600 the real seed");
        }
        let key_path = dir.path().join("device-key.ed25519");
        std::os::unix::fs::symlink(&real, &key_path).expect("plant a symlink at the key path");
        let res = DeviceKeyStore::load_or_generate(dir.path().to_str().expect("utf8 path"));
        assert!(
            res.is_err(),
            "a symlink at the device-key path must be refused (no symlink-following; \
             the perm-check must bind to the opened inode, not a swappable path)"
        );
    }

    // --- Defense-in-depth: the secret-storage DIRECTORY perms (task #109). --------
    //
    // The device-key FILE is already 0600-verified at load (an exposed signing
    // secret fails closed), so a merely group/world-READABLE (0755) dir is NOT a
    // leak — others can list names, not read a 0600 file. The actual risk is a
    // group/world-WRITABLE dir: an attacker with write access could pre-plant the
    // secret paths. So the directory hardening targets the WRITABLE bits only:
    // tighten an over-permissive dir we own, fail closed on one we don't — and
    // never reject a normal umask-022 0755 install (that would break installs and
    // is not a leak). Mirrors PR #199's secret-state-dir hardening.

    #[cfg(all(feature = "heartbeat", unix))]
    #[test]
    fn device_key_dir_group_world_writable_is_tightened_then_load_proceeds() {
        // A group/world-WRITABLE device-key dir we own must be TIGHTENED (the write
        // bits stripped) BEFORE any secret is stored there, and load must then
        // proceed (generate the key). A merely-permissive-but-owned dir is a
        // misconfiguration to repair, not a hard stop.
        use std::os::unix::fs::PermissionsExt as _;

        use multiview_licence::heartbeat::DeviceSigner as _;
        let dir = tempfile::tempdir().expect("tempdir");
        // 0777: group + world writable (the hostile pre-condition).
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777))
            .expect("chmod the dir 0o777");
        let signer = DeviceKeyStore::load_or_generate(dir.path().to_str().expect("utf8 path"))
            .expect("load proceeds after tightening an owned over-permissive dir");
        // The dir's group/other WRITE bits must be cleared after the call.
        let mode = std::fs::metadata(dir.path())
            .expect("dir exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode & 0o022,
            0,
            "an owned group/world-writable secret dir must be tightened (write bits stripped), \
             got {mode:#o}"
        );
        // And the key really was generated + persisted (0600), proving load did not
        // merely no-op.
        let key_path = dir.path().join("device-key.ed25519");
        assert!(
            key_path.exists(),
            "the device key is generated after tightening"
        );
        let key_mode = std::fs::metadata(&key_path)
            .expect("key file exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(key_mode, 0o600, "the persisted seed is still 0600");
        // Sanity: the returned signer is usable.
        let _ = signer.public_key_raw();
    }

    #[cfg(all(feature = "heartbeat", unix))]
    #[test]
    fn device_key_dir_world_readable_0755_is_accepted_unchanged() {
        // A standard umask-022 install leaves the dir 0755 (world-READABLE, NOT
        // writable). That is NOT a leak (the key file is 0600) and MUST be accepted
        // unchanged — tightening or rejecting it would break normal installs.
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755))
            .expect("chmod the dir 0o755");
        let _ = DeviceKeyStore::load_or_generate(dir.path().to_str().expect("utf8 path"))
            .expect("a 0755 (readable, non-writable) dir is accepted");
        let mode = std::fs::metadata(dir.path())
            .expect("dir exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o755,
            "a 0755 (readable, non-writable) secret dir must be left UNCHANGED — \
             not a leak, must not break umask-022 installs"
        );
    }

    #[cfg(all(feature = "heartbeat", unix))]
    #[test]
    fn nonce_dir_group_world_writable_is_tightened_then_store_opens() {
        // The SAME writable-bits-only hardening applies to the durable nonce dir
        // (FileNonceStore::in_dir) — a group/world-writable lease-state dir we own
        // is tightened before the lock/data files are created, then the store opens.
        use std::os::unix::fs::PermissionsExt as _;

        use multiview_licence::heartbeat::NonceStore as _;
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777))
            .expect("chmod the dir 0o777");
        let store = FileNonceStore::in_dir(dir.path().to_str().expect("utf8 path"))
            .expect("the nonce store opens after tightening an owned over-permissive dir");
        let mode = std::fs::metadata(dir.path())
            .expect("dir exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode & 0o022,
            0,
            "an owned group/world-writable nonce dir must be tightened (write bits stripped), \
             got {mode:#o}"
        );
        // The store is functional (a fresh dir loads a trusted 0).
        assert_eq!(store.load().expect("load the fresh nonce"), 0);
    }

    #[cfg(all(feature = "heartbeat", unix))]
    #[test]
    fn nonce_dir_world_readable_0755_is_accepted_unchanged() {
        // The nonce dir, like the key dir, must not reject a normal 0755 install.
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755))
            .expect("chmod the dir 0o755");
        let _ = FileNonceStore::in_dir(dir.path().to_str().expect("utf8 path"))
            .expect("a 0755 (readable, non-writable) nonce dir is accepted");
        let mode = std::fs::metadata(dir.path())
            .expect("dir exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o755,
            "a 0755 (readable, non-writable) nonce dir must be left UNCHANGED"
        );
    }
}
