//! The `zowietek` driver (DEV-A4, ADR-M009): the control-plane poller/actor that
//! manages one `ZowieBox`-class encoder/decoder appliance over the
//! vendor-published HTTP API. **Supports `ZowieBox`** — Multiview is independent
//! of and not endorsed by `ZowieTek` (the managed-devices brief, vendor posture).
//!
//! ## What the driver does
//!
//! * **Probe + adopt** — log in, probe the device's workmode/capabilities, and
//!   drive the DEV-A3 [`DeviceLifecycle`] `ADOPTING → ONLINE` (or `AUTH_FAILED` /
//!   `UNREACHABLE`), publishing `device.adopted` + a conflated `device.status`
//!   through the [`DeviceBroadcaster`].
//! * **Poll status** — re-poll status groups at ≤1 Hz per group (stream/publish
//!   status, decode table, decoder state, HDMI input, CPU temperature, storage)
//!   and publish the conflated `device.status` snapshot. A device-reported
//!   fault degrades the device; a dropped channel rides to `UNREACHABLE`.
//! * **Supervised reconnect** — on `UNREACHABLE`, exponential backoff + jitter +
//!   a breaker (the same shape inputs use); a reconnect (or adopt) that reaches
//!   `ONLINE` re-converges the device onto its declared `desired_mode` (the
//!   [`poller`](self::poller) runs [`ZowietekDriver::converge_mode`]).
//! * **Three facets** (ADR-M009) — the source facet enumerates the served
//!   RTSP/NDI mounts as [`SourceCandidate`]s; the output facet exposes the
//!   decode-table slots as [`OutputTarget`]s; the management facet covers
//!   reboot/temperature/users/tally (the long-running actions). The enumerated
//!   facets are mirrored into the [`DeviceDriverRegistry`] the A3 projection
//!   routes read.
//! * **Mode convergence** ([`ModeConvergence`]) — the vendor API exposes no
//!   encoder/decoder mode endpoint and no vendor SDK is present in this repo, so
//!   the driver converges `desired_mode` close-before-open through the decode
//!   table (see [`ModeConvergence`] for the honest gap), declaring the
//!   device-side (DEV-class) impact **before** apply. The poller runs this
//!   convergence whenever adopt/reconnect reaches `ONLINE` and on an operator
//!   `set-mode`.
//!
//! ## Isolation (invariant #10)
//!
//! Everything here is control-plane: the driver talks HTTP to its device on its
//! own task, publishes status into the engine's drop-oldest broadcast via the
//! broadcaster, and mutates only control-plane stores (the status + driver
//! registries). The engine never produces, forwards, or awaits anything the
//! driver does; a hung device can at worst stall its own driver task.

pub mod client;
pub mod poller;
pub mod runtime;

use std::sync::Arc;

use multiview_config::DeviceDriver;
use multiview_events::DeviceState;
use serde_json::Value;

use self::client::{
    BitrateBps, RpcVerb, ZowietekClient, ZowietekClientError, ZowietekSession, ZowietekTransport,
};
use super::broadcaster::{mode_impact_detail, DeviceBroadcaster};
use super::driver_registry::DeviceDriverRegistry;
use super::projection::{OutputTarget, SourceCandidate};

/// The verified RTSP server port on the `ZowieBox` family (managed-devices
/// brief §3.2 — verified on firmware 2026-06-10).
const RTSP_PORT: u16 = 8554;

/// The verified served RTSP mounts (main/sub), undocumented in the vendor API
/// doc but verified on firmware (managed-devices.md §3.2). Each entry is
/// `(candidate id, mount path)`.
const SERVED_RTSP_MOUNTS: [(&str, &str); 2] = [("main", "/main/av"), ("sub", "/sub/av")];

/// The encoder/decoder workmode a device reports, parsed from its `venc`/probe
/// response. The vendor API has no mode endpoint; the workmode is read from the
/// probe shape and enforced device-side (a decode call on an encoder-mode box
/// rejects with status `00004`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WorkMode {
    /// The box encodes its HDMI/SDI input (offers source candidates).
    Encoder,
    /// The box decodes a stream to its HDMI/SDI output (offers output targets).
    Decoder,
}

impl WorkMode {
    /// The driver token (`"encoder"` / `"decoder"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Encoder => "encoder",
            Self::Decoder => "decoder",
        }
    }

    /// Parse a driver token, if recognised.
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        match token {
            "encoder" => Some(Self::Encoder),
            "decoder" => Some(Self::Decoder),
            _ => None,
        }
    }
}

/// The plan to converge a device to a desired workmode (ADR-M009 §3.3).
///
/// The driver computes this **before** apply so the route can declare the
/// device-side (DEV-class) impact. Convergence is close-before-open: the device
/// enforces mutual exclusion (a decode-table call rejects in encoder workmode),
/// so a switch is *stop-current → start-next*, which restarts the device's
/// pipeline (the declared impact). Converging to the current mode is a no-op.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ModeConvergence {
    /// The device is already in the desired mode — nothing to do, no restart.
    AlreadyConverged {
        /// The mode the device is already in.
        mode: String,
    },
    /// The device must switch modes (close-before-open; the pipeline restarts).
    Switch {
        /// The mode being switched away from.
        from: String,
        /// The mode being switched to.
        to: String,
        /// The human-readable device-side impact declared before apply.
        impact_detail: String,
    },
}

impl ModeConvergence {
    /// The human-readable declared impact statement shown to the operator
    /// before apply (empty disruption text for a no-op).
    #[must_use]
    pub fn declared_impact(&self) -> String {
        match self {
            Self::AlreadyConverged { mode } => {
                format!("device is already in {mode:?}; no change, no disruption")
            }
            Self::Switch { impact_detail, .. } => impact_detail.clone(),
        }
    }

    /// Whether applying this plan restarts the device pipeline (true for a real
    /// switch, false for a no-op).
    #[must_use]
    pub const fn restarts_device(&self) -> bool {
        matches!(self, Self::Switch { .. })
    }
}

/// The `zowietek` driver for one device.
///
/// Holds the typed [`ZowietekClient`] (over a transport seam), the
/// [`DeviceBroadcaster`] it publishes lifecycle/status through, and the
/// [`DeviceDriverRegistry`] it mirrors enumerated facets into. The driver caches
/// the last probed workmode so mode-convergence planning needs no extra
/// round-trip.
pub struct ZowietekDriver<T: ZowietekTransport> {
    device_id: String,
    client: ZowietekClient<T>,
    broadcaster: DeviceBroadcaster,
    drivers: Arc<DeviceDriverRegistry>,
    /// The session established at adopt/reconnect; `None` before the first
    /// successful login. Guarded so poll/converge calls share one session.
    session: std::sync::Mutex<Option<ZowietekSession>>,
    /// The last probed workmode, cached for convergence planning.
    workmode: std::sync::Mutex<Option<WorkMode>>,
}

impl<T: ZowietekTransport> ZowietekDriver<T> {
    /// Build a driver for `device_id` over `transport`, publishing through
    /// `broadcaster` and mirroring facets into `drivers`.
    #[must_use]
    pub fn new(
        device_id: &str,
        transport: Arc<T>,
        broadcaster: DeviceBroadcaster,
        drivers: Arc<DeviceDriverRegistry>,
        username: &str,
        password: &str,
    ) -> Self {
        Self {
            device_id: device_id.to_owned(),
            client: ZowietekClient::new(transport, username, password),
            broadcaster,
            drivers,
            session: std::sync::Mutex::new(None),
            workmode: std::sync::Mutex::new(None),
        }
    }

    /// The device id this driver manages.
    #[must_use]
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// The last probed [`WorkMode`], if any (cached at adopt/reconnect). The
    /// poller consults this to decide which facets to enumerate: an encoder-mode
    /// box has no decode table, so the output-target read is skipped rather than
    /// issued and rejected (`00004`).
    #[must_use]
    pub fn workmode(&self) -> Option<WorkMode> {
        self.workmode.lock().ok().and_then(|g| *g)
    }

    /// Probe and adopt the device: login → probe workmode → drive the lifecycle
    /// to ONLINE and publish `device.adopted` + a conflated `device.status`.
    ///
    /// On a credential rejection the lifecycle opens the breaker (`AUTH_FAILED`);
    /// on a dropped channel it rides to `UNREACHABLE`. Both surface the typed
    /// error to the caller after publishing the lifecycle state.
    ///
    /// # Errors
    ///
    /// The [`ZowietekClientError`] from login/probe, after the lifecycle and
    /// status have been published so the registry reflects the failure.
    pub async fn probe_and_adopt(&self) -> Result<(), ZowietekClientError> {
        // The adopted lifecycle event (lossless) + ADOPTING status seed.
        self.broadcaster
            .adopted(&self.device_id, DeviceDriver::Zowietek, None);
        match self.establish_and_probe().await {
            Ok(mode) => {
                self.set_workmode(mode);
                self.broadcaster
                    .status(&self.device_id, DeviceState::Online);
                Ok(())
            }
            Err(err) => {
                self.publish_failure(&err);
                Err(err)
            }
        }
    }

    /// Re-establish the management channel after `UNREACHABLE` and re-converge:
    /// login → probe → ONLINE, exactly as adopt does but without re-publishing
    /// `device.adopted`.
    ///
    /// # Errors
    ///
    /// The [`ZowietekClientError`] from login/probe (the state is republished
    /// before returning).
    pub async fn reconnect(&self) -> Result<(), ZowietekClientError> {
        match self.establish_and_probe().await {
            Ok(mode) => {
                self.set_workmode(mode);
                self.broadcaster
                    .status(&self.device_id, DeviceState::Online);
                Ok(())
            }
            Err(err) => {
                self.publish_failure(&err);
                Err(err)
            }
        }
    }

    /// Poll the device's status groups once (≤1 Hz per group is the *caller's*
    /// cadence; one call here is one pass) and publish the conflated
    /// `device.status`. A device-reported unhealthy stream degrades the device.
    ///
    /// # Errors
    ///
    /// [`ZowietekClientError`] if the management channel fails mid-poll (the
    /// device rides to `UNREACHABLE`, republished before returning).
    pub async fn poll_once(&self) -> Result<(), ZowietekClientError> {
        let session = self.session_clone()?;
        // Poll the stream/decode status (one representative status group). A
        // real deployment polls each group on its own ≤1 Hz cadence; the busy
        // backoff in the client keeps the device's rate limit honoured.
        match self
            .client
            .get_info(&session, "streamplay", "streamplay", Value::Null)
            .await
        {
            Ok(data) => {
                let healthy = streams_healthy(&data);
                let state = if healthy {
                    DeviceState::Online
                } else {
                    DeviceState::Degraded
                };
                self.broadcaster.status(&self.device_id, state);
                Ok(())
            }
            Err(err) => {
                self.publish_failure(&err);
                Err(err)
            }
        }
    }

    /// Poll the device's status groups once and **return** whether all streams
    /// are healthy — without itself publishing a lifecycle state.
    ///
    /// This is the seam the [`poller`](crate::devices::zowietek::poller) actor
    /// uses: the poller maps the result into a [`LifecycleEvent`](crate::devices::LifecycleEvent)
    /// (`Recover` / `DeviceFault`), drives the DEV-A3 state machine, and
    /// publishes the **lifecycle's** output state via
    /// [`publish_state`](ZowietekDriver::publish_state) — so the published
    /// `device.status` is always the transition table's output, never an ad-hoc
    /// target. (The standalone [`poll_once`](ZowietekDriver::poll_once) publishes
    /// directly; it predates the poller and is retained for direct use.)
    ///
    /// # Errors
    ///
    /// [`ZowietekClientError`] if the management channel fails mid-poll (the
    /// poller rides the device to `UNREACHABLE` from the error).
    pub async fn poll_status(&self) -> Result<bool, ZowietekClientError> {
        let session = self.session_clone()?;
        let data = self
            .client
            .get_info(&session, "streamplay", "streamplay", Value::Null)
            .await?;
        Ok(streams_healthy(&data))
    }

    /// Publish an explicit lifecycle `state` as the conflated `device.status`
    /// (latest-wins) — the path the poller uses to publish the **state machine's
    /// output** after driving a [`LifecycleEvent`](crate::devices::LifecycleEvent).
    pub fn publish_state(&self, state: DeviceState) {
        self.broadcaster.status(&self.device_id, state);
    }

    // ---- Facet (a): source candidates -------------------------------------

    /// The source facet (ADR-M009 §3.2 facet (a)): enumerate the device's served
    /// RTSP mounts as bindable [`SourceCandidate`]s, addressed at `host` (the
    /// device's management host — a bracketed IPv6 literal or hostname).
    ///
    /// The served mount **paths** are undocumented in the vendor API doc but
    /// verified on current firmware (`/main/av`, `/sub/av` on port 8554), so the
    /// candidates ship turnkey (`unverified = false`) for that firmware; an
    /// operator can still override the URL for other models. The enumerated
    /// candidates are mirrored into the [`DeviceDriverRegistry`] the A3
    /// `source-candidates` route reads.
    ///
    /// # Errors
    ///
    /// Never fails today (the mounts are fixed); returns a `Result` so a future
    /// firmware probe of the served paths can surface a transport error.
    pub fn enumerate_source_candidates(
        &self,
        host: &str,
    ) -> Result<Vec<SourceCandidate>, ZowietekClientError> {
        let candidates: Vec<SourceCandidate> = SERVED_RTSP_MOUNTS
            .iter()
            .map(|(id, path)| SourceCandidate {
                id: (*id).to_owned(),
                kind: "rtsp".to_owned(),
                url: Some(format!("rtsp://{host}:{RTSP_PORT}{path}")),
                // Verified on current firmware (managed-devices.md §3.2): the
                // paths are known, so these are not operator-suppliable guesses.
                unverified: false,
            })
            .collect();
        self.drivers
            .set_source_candidates(&self.device_id, candidates.clone());
        Ok(candidates)
    }

    // ---- Facet (b): output targets ----------------------------------------

    /// The output facet (ADR-M009 §3.2 facet (b)): enumerate the device's
    /// decode-table slots as bindable [`OutputTarget`]s, read from the live
    /// `/streamplay` decode table. Mirrored into the [`DeviceDriverRegistry`] the
    /// A3 `output-targets` route reads.
    ///
    /// # Errors
    ///
    /// [`ZowietekClientError`] if the decode table cannot be read (e.g. an
    /// encoder-mode box rejects the call with status `00004`).
    pub async fn enumerate_output_targets(&self) -> Result<Vec<OutputTarget>, ZowietekClientError> {
        let session = self.session_clone()?;
        let data = self
            .client
            .get_info(&session, "streamplay", "streamplay", Value::Null)
            .await?;
        let targets = parse_decode_targets(&data);
        self.drivers
            .set_output_targets(&self.device_id, targets.clone());
        Ok(targets)
    }

    // ---- Facet (c) / management: mode convergence -------------------------

    /// Plan a convergence to `desired` against the last probed workmode
    /// (ADR-M009 §3.3). Computes the close-before-open switch (and its declared
    /// impact) or a no-op when already converged.
    ///
    /// Pure (no I/O): the route calls this to surface the declared impact before
    /// applying; [`ZowietekDriver::converge_mode`] then applies it.
    #[must_use]
    pub fn plan_mode_convergence(&self, desired: &str) -> ModeConvergence {
        let current = self.workmode_token();
        if current.as_deref() == Some(desired) {
            return ModeConvergence::AlreadyConverged {
                mode: desired.to_owned(),
            };
        }
        ModeConvergence::Switch {
            from: current.unwrap_or_else(|| "unknown".to_owned()),
            to: desired.to_owned(),
            impact_detail: mode_impact_detail(&self.device_id, desired),
        }
    }

    /// Converge the device to `desired` workmode close-before-open, publishing
    /// `device.mode` (DEV-class impact) at the start of a real switch.
    ///
    /// The vendor API has no mode endpoint and no vendor SDK is present in this
    /// repo, so convergence operates the decode table directly: a switch
    /// **closes** the current decode/encode activity, then **opens** the target
    /// (the device enforces close-before-open with dedicated status codes). A
    /// no-op plan publishes nothing and touches the device not at all.
    ///
    /// # Errors
    ///
    /// [`ZowietekClientError`] if the close or open step fails on the wire.
    pub async fn converge_mode(&self, desired: &str) -> Result<(), ZowietekClientError> {
        let plan = self.plan_mode_convergence(desired);
        let ModeConvergence::Switch { to, .. } = &plan else {
            // Already converged: no restart, no event.
            return Ok(());
        };
        // Declare the DEV-class impact at the start (instant-apply doctrine).
        self.broadcaster.mode_started(&self.device_id, to);
        let session = self.session_clone()?;
        // Close-before-open: stop the current pipeline, THEN start the target.
        // The vendor decode table is the only documented lever (see the type
        // doc); the close must precede the open. A failure of either step
        // publishes a `device.mode` Failed so the operator sees the convergence
        // did not complete (the driver re-converges on the next pass).
        if let Err(err) = self.apply_mode_switch(&session, to).await {
            self.broadcaster.mode_failed(&self.device_id, to);
            return Err(err);
        }
        if let Some(mode) = WorkMode::parse(to) {
            self.set_workmode(mode);
        }
        self.broadcaster.mode_finished(&self.device_id, to);
        Ok(())
    }

    // ---- Internals ---------------------------------------------------------

    /// Apply the close-before-open switch on the decode table: stop the current
    /// pipeline, then start the target. The close precedes the open.
    async fn apply_mode_switch(
        &self,
        session: &ZowietekSession,
        to: &str,
    ) -> Result<(), ZowietekClientError> {
        self.client
            .request(
                session,
                RpcVerb::SetInfo,
                "streamplay",
                "streamplay",
                "stop",
                Value::Null,
            )
            .await?;
        self.client
            .request(
                session,
                RpcVerb::SetInfo,
                "streamplay",
                "streamplay",
                "start",
                serde_json::json!({ "mode": to }),
            )
            .await?;
        Ok(())
    }

    /// Login + probe the workmode, replacing the cached session on success.
    async fn establish_and_probe(&self) -> Result<WorkMode, ZowietekClientError> {
        let session = self.client.login().await?;
        let mode = self.probe_workmode(&session).await?;
        self.set_session(session);
        Ok(mode)
    }

    /// Probe the device's workmode from its `venc` response shape.
    ///
    /// Also reads the encoder bitrate (when the response carries one) and
    /// validates it through [`BitrateBps`], so a kbps-shaped value the vendor
    /// doc's unit ambiguity could leak is caught at runtime (logged), not
    /// silently trusted. The bitrate is advisory telemetry — it never changes the
    /// returned workmode — so an absent or implausible field degrades to a debug
    /// log, never a probe failure.
    async fn probe_workmode(
        &self,
        session: &ZowietekSession,
    ) -> Result<WorkMode, ZowietekClientError> {
        let data = self
            .client
            .get_info(session, "venc", "venc", serde_json::json!({ "ch": 0 }))
            .await?;
        self.note_probed_bitrate(&data);
        // The probe reports a `workmode` token; default to encoder (the box's
        // power-on default) when the field is absent.
        let token = data
            .get("workmode")
            .and_then(Value::as_str)
            .unwrap_or("encoder");
        Ok(WorkMode::parse(token).unwrap_or(WorkMode::Encoder))
    }

    /// Read and magnitude-validate the encoder bitrate from a `venc` probe
    /// response through [`BitrateBps`] (the bps-vs-kbps guard). Best-effort
    /// telemetry: an absent field is silent; an implausible (kbps-shaped) field
    /// is logged for hardware-verification follow-up, never trusted as a bps
    /// value, and never fails the probe.
    fn note_probed_bitrate(&self, data: &Value) {
        let Some(field) = data.get("bitrate").and_then(Value::as_u64) else {
            return;
        };
        match BitrateBps::from_field(field) {
            Ok(bitrate) => tracing::debug!(
                device = %self.device_id,
                bitrate_bps = bitrate.get(),
                "zowietek probe: encoder bitrate (validated bps)"
            ),
            Err(err) => tracing::warn!(
                device = %self.device_id,
                field,
                error = %err,
                "zowietek probe: reported bitrate is implausibly small for bps \
                 (kbps-shaped?); not trusted — verify on hardware"
            ),
        }
    }

    /// Publish the lifecycle state implied by a client error (`AUTH_FAILED` for
    /// a credential rejection, `UNREACHABLE` otherwise) and a `device.error`.
    fn publish_failure(&self, err: &ZowietekClientError) {
        let state = lifecycle_state_for(err);
        self.broadcaster.status(&self.device_id, state);
        self.broadcaster.error(&self.device_id, &err.to_string());
    }

    fn set_session(&self, session: ZowietekSession) {
        if let Ok(mut guard) = self.session.lock() {
            *guard = Some(session);
        }
    }

    fn session_clone(&self) -> Result<ZowietekSession, ZowietekClientError> {
        self.session
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .ok_or_else(|| ZowietekClientError::Unreachable {
                module: "system".to_owned(),
            })
    }

    fn set_workmode(&self, mode: WorkMode) {
        if let Ok(mut guard) = self.workmode.lock() {
            *guard = Some(mode);
        }
    }

    fn workmode_token(&self) -> Option<String> {
        self.workmode
            .lock()
            .ok()
            .and_then(|g| *g)
            .map(|m| m.as_str().to_owned())
    }
}

/// The lifecycle state a client error implies: a credential rejection opens the
/// breaker (`AUTH_FAILED`); anything else rides to `UNREACHABLE`.
fn lifecycle_state_for(err: &ZowietekClientError) -> DeviceState {
    match err {
        ZowietekClientError::Status { status, .. } if is_auth_status(status) => {
            DeviceState::AuthFailed
        }
        _ => DeviceState::Unreachable,
    }
}

/// Whether a status code is a credential-rejection code (`00002`/`00003` family,
/// observed as login failures). Codes are compared numerically.
fn is_auth_status(status: &str) -> bool {
    matches!(status.trim().parse::<u32>(), Ok(2 | 3))
}

/// Whether a device-reported `/streamplay` status reports all streams healthy.
/// An empty/absent stream list is treated as healthy (nothing is failing).
fn streams_healthy(data: &Value) -> bool {
    match data.get("streams").and_then(Value::as_array) {
        Some(streams) => streams
            .iter()
            .all(|s| s.get("healthy").and_then(Value::as_bool).unwrap_or(true)),
        None => true,
    }
}

/// Parse the `/streamplay` decode table into bindable [`OutputTarget`]s. Each
/// entry's `proto` becomes the target transport kind; the `index` names the slot.
fn parse_decode_targets(data: &Value) -> Vec<OutputTarget> {
    let Some(entries) = data.get("entries").and_then(Value::as_array) else {
        return Vec::new();
    };
    entries
        .iter()
        .enumerate()
        .map(|(position, entry)| {
            let index = entry
                .get("index")
                .and_then(Value::as_u64)
                .unwrap_or_else(|| u64::try_from(position).unwrap_or(0));
            let kind = entry
                .get("proto")
                .and_then(Value::as_str)
                .unwrap_or("rtsp")
                .to_owned();
            OutputTarget {
                id: format!("slot-{index}"),
                kind,
                label: Some(format!("decode slot {index}")),
            }
        })
        .collect()
}
