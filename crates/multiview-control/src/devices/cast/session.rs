//! The Cast **session actor** (DEV-D2, ADR-M011): the supervised
//! control-plane task that owns one CASTV2 conversation with a Cast device
//! and drives the DEV-A3 [`DeviceLifecycle`] from it.
//!
//! ## Lifecycle (ADR-M011 / managed-devices §5.1)
//!
//! On spawn the actor **establishes**: connect (TLS via the seam) → virtual
//! `CONNECT` to the platform receiver → `LAUNCH` the Default Media Receiver
//! (`CC1AD845`) → `CONNECT` to the launched app's transport → `LOAD` the
//! device-reachable HLS rendition (`streamType: LIVE`, explicit
//! `hlsVideoSegmentFormat`). Established, it heartbeats: **PING every 10 s,
//! the session is dead after 20 s without inbound traffic** (a PONG is the
//! guaranteed generator; any inbound frame proves channel liveness), then
//! **reconnects on a 5 s cadence**. A reconnect with a previously
//! established session **asks first** (`GET_STATUS`): when our receiver
//! session survived the blip the actor re-`CONNECT`s to its transport and
//! resumes supervising — playback is untouched; only when the session is
//! gone with nothing else running does it re-`LAUNCH` + re-`LOAD`. A media
//! `IDLE` status, a rejected `LOAD` (`LOAD_FAILED` / `LOAD_CANCELLED` /
//! media-namespace `INVALID_REQUEST`), or a **vanished media session** (an
//! empty post-LOAD `MEDIA_STATUS`) **re-`LOAD`s after the reload delay**
//! (delayed so an instantly-failing rendition cannot drive a LOAD storm).
//!
//! Reconnects are **address-based**: the actor re-dials the authority it was
//! built with. Re-resolving a moved device by its mDNS UUID is the DEV-A5
//! discovery driver's job — no discovery infrastructure exists on this base
//! yet, so a DHCP-moved device rides UNREACHABLE until re-adopted at its new
//! address (documented limitation, not silent behaviour).
//!
//! ## Preemption is surfaced, never fought
//!
//! The CASTV2 protocol has no sender authentication: any LAN client can take
//! the device. Preemption is keyed on **session identity**, never the app
//! id: our receiver `sessionId` gone while another sender's app runs (a
//! foreign app, or a *new session of the same* Default Media Receiver — the
//! pychromecast/Home-Assistant pattern), or our `mediaSessionId` replaced by
//! an active foreign media session, marks the session preempted. The actor
//! publishes `device.error` ("preempted…"), drives `DEGRADED`, and stops
//! supervising the media — it never re-`LAUNCH`es over the other sender
//! (ADR-M011); later reconnects re-establish the management channel only.
//! Our app gone with **nothing** else running (at most the receiver's idle
//! screen) is *not* a preemption — the app idled out or crashed, and the
//! actor re-establishes (re-`LAUNCH` + re-`LOAD`). The operator resolves a
//! real preemption by stopping/restarting the session (or re-adopting the
//! device).
//!
//! ## Isolation (invariant #10)
//!
//! Pure control plane: the actor publishes through the [`DeviceBroadcaster`]
//! (drop-oldest broadcast + latest-wins registry) and never touches the
//! engine; its control channel is the same bounded non-blocking `mpsc` every
//! poller uses. A hung device can at worst stall its own task. The media
//! path is untouched — the device fetches an HLS rendition that is already
//! being served (encode-once preserved; invariant #1 trivially safe).

use std::collections::VecDeque;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use multiview_events::{DeviceCapabilities, DeviceState, DeviceStatus, SyncCapability};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::media::CastMediaTarget;
use super::protocol::{
    self, CastFrame, InboundMessage, MediaStatusEntry, PlayerState, DEFAULT_MEDIA_RECEIVER_APP_ID,
    PLATFORM_RECEIVER_ID,
};
use crate::devices::broadcaster::DeviceBroadcaster;
use crate::devices::state_machine::{DeviceLifecycle, LifecycleEvent};
use crate::devices::zowietek::poller::{PollerControl, PollerHandle, PollerStep};

/// The bounded control-channel depth (mirrors the zowietek poller: a handful
/// of in-flight operator commands is ample; beyond it `try_send` sheds).
const CONTROL_CHANNEL_DEPTH: usize = 16;

/// A transport-level failure on the cast channel seam (connect refused, a
/// dropped socket, a write failure). The session actor maps every one onto
/// the supervised-reconnect path — never a panic, never a crash.
#[derive(Debug, Error)]
#[error("cast channel error: {message}")]
pub struct CastChannelError {
    /// A human-readable description.
    pub message: String,
}

impl CastChannelError {
    /// Build an error from any displayable cause.
    #[must_use]
    pub fn new(message: impl std::fmt::Display) -> Self {
        Self {
            message: message.to_string(),
        }
    }
}

/// One open CASTV2 conversation (TLS + framing established): the seam that
/// keeps every line of driver logic socket-free testable. The real
/// implementation ([`TlsCastChannel`](super::net::TlsCastChannel)) lives
/// behind the off-by-default `cast` feature; [`ScriptedChannel`] satisfies it
/// for tests.
pub trait CastChannel: Send {
    /// Send one frame to the device.
    fn send(
        &mut self,
        frame: CastFrame,
    ) -> impl Future<Output = Result<(), CastChannelError>> + Send;
    /// Receive the next frame from the device (pends until one arrives; an
    /// `Err` means the channel is dead).
    fn recv(&mut self) -> impl Future<Output = Result<CastFrame, CastChannelError>> + Send;
}

/// Opens [`CastChannel`]s to a device authority (`host[:port]`). The real
/// implementation ([`TlsCastConnector`](super::net::TlsCastConnector)) is
/// feature-gated; [`ScriptedConnector`] scripts connects for tests.
pub trait CastConnector: Send + Sync {
    /// The channel type this connector opens.
    type Channel: CastChannel + Send + 'static;
    /// Open a channel to `authority` (e.g. `[2001:db8::20]:8009`).
    fn connect(
        &self,
        authority: &str,
    ) -> impl Future<Output = Result<Self::Channel, CastChannelError>> + Send;
}

/// A shared connector handle: the factory holds one connector for every
/// device it spawns; each actor dials through the same `Arc`.
impl<C: CastConnector> CastConnector for Arc<C> {
    type Channel = C::Channel;

    fn connect(
        &self,
        authority: &str,
    ) -> impl Future<Output = Result<Self::Channel, CastChannelError>> + Send {
        C::connect(self, authority)
    }
}

/// Invoked at the moment the receiver **accepts** this actor's `LOAD` — the
/// first `MEDIA_STATUS` attributing an active media session to us (DEV-D3.1).
///
/// Fired once per LOAD adoption (a supervised re-LOAD that is accepted again
/// re-fires it; the consumer decides idempotence — the session store's
/// started-at stamp is first-write-wins). The closure runs on the actor task:
/// it must be cheap and non-blocking (the cast wiring's stamp is one bounded
/// `Mutex`-guarded map write — pure control plane, invariant #10).
pub type LoadAcceptedHook = Arc<dyn Fn() + Send + Sync>;

/// Session-actor timing (the ADR-M011 numbers as defaults).
///
/// A plain record (not `#[non_exhaustive]`) so tests can build it directly,
/// mirroring [`PollerConfig`](crate::devices::PollerConfig).
#[derive(Debug, Clone, Copy)]
pub struct CastSessionConfig {
    /// The sender heartbeat cadence (PING every 10 s).
    pub ping_interval: Duration,
    /// The session is dead after this long without inbound traffic (20 s; a
    /// PONG is the guaranteed inbound generator).
    pub pong_timeout: Duration,
    /// The supervised-reconnect cadence after a dead/refused channel (5 s).
    pub reconnect_delay: Duration,
    /// How long after a media IDLE / rejected LOAD / vanished media session
    /// the supervisor re-LOADs (bounds an instantly-failing rendition to one
    /// LOAD per delay — never a storm).
    pub reload_delay: Duration,
    /// How long the establishment waits for the LAUNCH (and a reconnect for
    /// its `GET_STATUS` answer) before the attempt is declared unreachable.
    pub launch_timeout: Duration,
}

impl Default for CastSessionConfig {
    fn default() -> Self {
        Self {
            ping_interval: Duration::from_secs(10),
            pong_timeout: Duration::from_secs(20),
            reconnect_delay: Duration::from_secs(5),
            reload_delay: Duration::from_secs(5),
            launch_timeout: Duration::from_secs(10),
        }
    }
}

impl CastSessionConfig {
    /// Test timings: millisecond reconnect/launch/reload cadences so
    /// real-time tests never sleep long, with a **generous liveness window**
    /// (the pong timeout) so an established scripted session idling on a
    /// silent channel — the scripts rarely answer PINGs — is not declared
    /// dead mid-test. Heartbeat-expiry behaviour has its own paused-clock
    /// tests over the real ADR-M011 numbers ([`Default`]).
    #[must_use]
    pub fn test_fast() -> Self {
        Self {
            ping_interval: Duration::from_millis(10),
            pong_timeout: Duration::from_secs(60),
            reconnect_delay: Duration::from_millis(5),
            reload_delay: Duration::from_millis(5),
            launch_timeout: Duration::from_millis(10),
        }
    }
}

/// The fixed probed capability flags a Cast endpoint maps onto (ADR-M008
/// §2.3): a decode-and-display media target with **no sync surface at all**
/// (Tier D — never a synchronized-canvas participant, ADR-M010/M011).
const CAST_CAPABILITIES: DeviceCapabilities = DeviceCapabilities {
    encode: false,
    decode: true,
    display: true,
    sync: SyncCapability::None,
    audio: true,
    reboot: false,
    firmware_update: false,
};

/// The launched receiver app this session drives (from `RECEIVER_STATUS`).
#[derive(Debug, Clone)]
struct LaunchedApp {
    /// The receiver-side session id (what a STOP names).
    session_id: String,
    /// The transport id media-namespace messages address.
    transport_id: String,
}

/// The outcome of the reconnect's GET_STATUS-first triage
/// ([`CastSessionActor::reconnect_triage`]).
enum ReconnectTriage<Ch> {
    /// The attempt resolved in the triage: our session survived (ONLINE),
    /// another sender took the device (preempted, DEGRADED), or the channel
    /// died (UNREACHABLE).
    Resolved(PollerStep),
    /// Our session is gone with nothing else running: the caller
    /// re-establishes (LAUNCH + LOAD) on the still-open channel.
    ReEstablish(Ch),
}

/// The supervised session actor for one Cast device / ad-hoc session.
///
/// Construct with [`CastSessionActor::new`]; drive it step-by-step (the unit
/// the tests exercise: [`connect_step`](Self::connect_step) /
/// [`pump_step`](Self::pump_step)) or run it to completion with
/// [`spawn`](Self::spawn), which returns the same transport-erased
/// [`PollerHandle`] every device actor uses (DEV-A4 registry machinery).
pub struct CastSessionActor<C: CastConnector> {
    /// The id this actor publishes under: a device id (saved cast device) or
    /// an ephemeral `cast-session-…` id.
    device_id: String,
    connector: C,
    /// The device authority dialled on (re)connect (`host[:port]`).
    authority: String,
    media: CastMediaTarget,
    broadcaster: DeviceBroadcaster,
    lifecycle: DeviceLifecycle,
    config: CastSessionConfig,
    /// The open channel, when up.
    channel: Option<C::Channel>,
    /// The receiver app session we established and believe is ours. Kept
    /// across channel death: the reconnect's `GET_STATUS` check compares its
    /// `session_id` against what actually runs to decide survived / gone /
    /// preempted.
    app: Option<LaunchedApp>,
    /// Our media session within the app, once adopted from the first active
    /// `MEDIA_STATUS` after a LOAD (`None` = a LOAD answer is awaited).
    /// Statuses for other media sessions are never attributed to us.
    media_session_id: Option<i64>,
    /// Monotonic request-id counter (CASTV2 request correlation).
    request_id: u32,
    /// The last inbound traffic (heartbeat liveness; PONGs and every other
    /// inbound frame refresh it).
    last_inbound: Instant,
    /// A pending re-LOAD deadline (IDLE / rejected LOAD / vanished media
    /// session), when scheduled.
    reload_due: Option<Instant>,
    /// Another sender took the device: supervise the channel only, never the
    /// media (no re-LAUNCH/re-LOAD), until the operator restarts the session.
    preempted: bool,
    /// The last published player-state mode token (conflated status field).
    mode: &'static str,
    /// Fired when the receiver accepts our LOAD (the started-at stamp's
    /// trigger); [`None`] when nothing consumes the moment.
    load_accepted: Option<LoadAcceptedHook>,
}

impl<C: CastConnector + 'static> CastSessionActor<C> {
    /// Build a session actor publishing under `device_id`, dialling
    /// `authority` through `connector`, `LOAD`ing `media`.
    #[must_use]
    pub fn new(
        device_id: &str,
        connector: C,
        authority: &str,
        media: CastMediaTarget,
        broadcaster: DeviceBroadcaster,
        config: CastSessionConfig,
    ) -> Self {
        Self {
            device_id: device_id.to_owned(),
            connector,
            authority: authority.to_owned(),
            media,
            broadcaster,
            lifecycle: DeviceLifecycle::new(),
            config,
            channel: None,
            app: None,
            media_session_id: None,
            request_id: 0,
            last_inbound: Instant::now(),
            reload_due: None,
            preempted: false,
            mode: "connecting",
            load_accepted: None,
        }
    }

    /// Attach the [`LoadAcceptedHook`] fired when the receiver accepts this
    /// actor's `LOAD` (the ephemeral session's started-at stamp, DEV-D3.1).
    #[must_use]
    pub fn with_load_accepted_hook(mut self, hook: LoadAcceptedHook) -> Self {
        self.load_accepted = Some(hook);
        self
    }

    /// The id this actor publishes under.
    #[must_use]
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// The current lifecycle state.
    #[must_use]
    pub fn state(&self) -> DeviceState {
        self.lifecycle.state()
    }

    /// The latest conflated `device.status` snapshot published for this
    /// session (read back from the broadcaster's latest-wins registry).
    #[must_use]
    pub fn published_status(&self) -> Option<DeviceStatus> {
        self.broadcaster.registry().snapshot(&self.device_id)
    }

    /// The next CASTV2 request id (wrapping; correlation only).
    fn next_request_id(&mut self) -> u32 {
        self.request_id = self.request_id.wrapping_add(1);
        self.request_id
    }

    /// Drive the lifecycle by `event` and publish the conflated status (the
    /// published state is always the transition table's output, carrying the
    /// current player-state `mode` and the fixed Cast capability flags).
    fn drive(&mut self, event: LifecycleEvent) -> PollerStep {
        self.lifecycle.apply(event);
        self.publish_status();
        step_for(self.lifecycle.state())
    }

    /// Publish the current lifecycle state + mode as the conflated
    /// `device.status` (latest-wins; ring-excluded per ADR-RT007).
    fn publish_status(&self) {
        let mut status = DeviceStatus::new(&self.device_id, self.lifecycle.state());
        status.mode = Some(self.mode.to_owned());
        status.capabilities = Some(CAST_CAPABILITIES);
        let _seq = self.broadcaster.publish_status(status);
    }

    /// Tear the channel down and ride the lifecycle to UNREACHABLE (the
    /// supervised-reconnect path). The session identity (`app`,
    /// `media_session_id`) is kept: the reconnect's `GET_STATUS` check
    /// decides whether the receiver session survived the blip.
    fn channel_died(&mut self) -> PollerStep {
        self.channel = None;
        self.reload_due = None;
        self.drive(LifecycleEvent::Unreachable)
    }

    /// Mark the session preempted: another sender's app or media session
    /// replaced ours. Surfaces the `device.error`, drops our claimed
    /// identity, and cancels any pending re-LOAD — from here the actor
    /// supervises the management channel only (ADR-M011: never fight).
    fn note_preempted(&mut self) {
        self.preempted = true;
        self.app = None;
        self.media_session_id = None;
        self.reload_due = None;
        self.mode = "preempted";
        let _seq = self.broadcaster.error(
            &self.device_id,
            "cast session preempted by another sender; restart the session to reclaim the device",
        );
    }

    /// One full establishment attempt: connect → CONNECT → LAUNCH `CC1AD845`
    /// → (pump until `RECEIVER_STATUS` carries the app) → CONNECT to the app
    /// transport → LOAD the rendition. Returns the driven step:
    /// [`PollerStep::Online`] on success, [`PollerStep::Unreachable`] when
    /// the device cannot be reached (or the LAUNCH timed out), and
    /// [`PollerStep::Degraded`] on a LAUNCH refusal (a device fault).
    ///
    /// A **reconnect** (a previously-established session is remembered) asks
    /// first: `GET_STATUS` → our session still running ⇒ re-CONNECT to its
    /// transport only (no re-LAUNCH/re-LOAD — playback untouched); nothing
    /// (or only the idle screen) running ⇒ re-establish; another sender's
    /// app/session running ⇒ preempted (hands-off, DEGRADED).
    ///
    /// While **preempted**, the attempt re-establishes the management
    /// channel only (CONNECT, heartbeat) and stays DEGRADED — it never
    /// re-LAUNCHes over the other sender's session (ADR-M011).
    pub async fn connect_step(&mut self) -> PollerStep {
        let mut channel = match self.connector.connect(&self.authority).await {
            Ok(channel) => channel,
            Err(err) => {
                tracing::debug!(
                    device = %self.device_id,
                    error = %err,
                    "cast connect refused; supervised reconnect continues"
                );
                return self.channel_died();
            }
        };
        self.last_inbound = Instant::now();
        if let Err(err) = channel
            .send(protocol::connect_frame(PLATFORM_RECEIVER_ID))
            .await
        {
            tracing::debug!(device = %self.device_id, error = %err, "cast CONNECT failed");
            return self.channel_died();
        }
        if self.preempted {
            // Hands-off: management channel up, media left to the other
            // sender. Reconnect (channel up) then DeviceFault (still not our
            // media) — the published end state is DEGRADED.
            self.channel = Some(channel);
            let _ = self.drive(LifecycleEvent::Reconnect);
            return self.drive(LifecycleEvent::DeviceFault);
        }

        // Reconnecting with a previously-established receiver session: ask
        // the receiver what actually runs before touching anything. A 20 s
        // blip on a healthy receiver must not restart playback, and a
        // preemption that happened during the blip must not be stomped.
        if let Some(prior) = self.app.clone() {
            match self.reconnect_triage(channel, &prior).await {
                ReconnectTriage::Resolved(step) => return step,
                ReconnectTriage::ReEstablish(open) => channel = open,
            }
        }

        let launch_id = self.next_request_id();
        if let Err(err) = channel
            .send(protocol::launch_frame(
                launch_id,
                DEFAULT_MEDIA_RECEIVER_APP_ID,
            ))
            .await
        {
            tracing::debug!(device = %self.device_id, error = %err, "cast LAUNCH send failed");
            return self.channel_died();
        }

        // Pump until the receiver reports our app (or the launch times out /
        // is refused), answering heartbeats meanwhile.
        let app = match self.pump_for_app(&mut channel).await {
            Ok(app) => app,
            Err(step) => return step,
        };

        // CONNECT to the app's transport, then LOAD the rendition on it.
        if channel
            .send(protocol::connect_frame(&app.transport_id))
            .await
            .is_err()
        {
            return self.channel_died();
        }
        let load_id = self.next_request_id();
        if channel
            .send(protocol::load_frame(
                load_id,
                &app.transport_id,
                &self.media,
            ))
            .await
            .is_err()
        {
            return self.channel_died();
        }
        self.app = Some(app);
        self.channel = Some(channel);
        self.reload_due = None;
        // The LOAD's media session is adopted from its first active
        // MEDIA_STATUS; until then no media is attributed to us.
        self.media_session_id = None;
        self.mode = "loading";
        // ProbeOk converges every reachable source state onto ONLINE
        // (ADOPTING/DEGRADED directly; UNREACHABLE via its ProbeOk edge).
        self.drive(LifecycleEvent::ProbeOk)
    }

    /// The launch-establishment pump: receive frames (answering heartbeats)
    /// until a `RECEIVER_STATUS` reports the Default Media Receiver running,
    /// bounded by the launch timeout. `Err` carries the driven step the
    /// failed establishment attempt resolves to (the channel/lifecycle are
    /// already torn down/driven for the supervised reconnect).
    async fn pump_for_app(&mut self, channel: &mut C::Channel) -> Result<LaunchedApp, PollerStep> {
        let deadline = Instant::now() + self.config.launch_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let frame = match tokio::time::timeout(remaining, channel.recv()).await {
                Ok(Ok(frame)) => frame,
                Ok(Err(err)) => {
                    tracing::debug!(device = %self.device_id, error = %err, "cast channel dropped during LAUNCH");
                    return Err(self.channel_died());
                }
                Err(_elapsed) => {
                    tracing::debug!(device = %self.device_id, "cast LAUNCH timed out");
                    return Err(self.channel_died());
                }
            };
            self.last_inbound = Instant::now();
            match protocol::decode(&frame) {
                InboundMessage::Ping => {
                    if channel.send(protocol::pong_frame()).await.is_err() {
                        return Err(self.channel_died());
                    }
                }
                InboundMessage::ReceiverStatus(status) => {
                    if let Some(app) = status
                        .applications
                        .iter()
                        .find(|a| a.app_id == DEFAULT_MEDIA_RECEIVER_APP_ID)
                    {
                        return Ok(LaunchedApp {
                            session_id: app.session_id.clone(),
                            transport_id: app.transport_id.clone(),
                        });
                    }
                    // A status without our app while launching is normal
                    // (the platform broadcasts status); keep waiting.
                }
                InboundMessage::LaunchError { reason } => {
                    tracing::warn!(
                        device = %self.device_id,
                        reason = reason.as_deref().unwrap_or("unspecified"),
                        "cast LAUNCH refused; retrying on the reconnect cadence"
                    );
                    // A refusal is a device fault on a PROVEN-reachable device
                    // (it answered the LAUNCH): drive ProbeOk then DeviceFault
                    // — the lifecycle table has no direct ADOPTING→DEGRADED
                    // edge, exactly the two-event drive the preemption path
                    // uses. The channel is dropped so the supervised 5 s
                    // reconnect re-LAUNCHes.
                    self.channel = None;
                    self.app = None;
                    let _ = self.drive(LifecycleEvent::ProbeOk);
                    return Err(self.drive(LifecycleEvent::DeviceFault));
                }
                InboundMessage::CloseConnection => {
                    return Err(self.channel_died());
                }
                // Pong / MediaStatus / LoadError / Unknown (and anything a
                // future decoder adds — `InboundMessage` is
                // `#[non_exhaustive]`) refresh the liveness stamp above and
                // are otherwise irrelevant while launching (no LOAD of ours
                // is outstanding yet).
                _ => {}
            }
        }
    }

    /// The reconnect's GET_STATUS-first triage on a freshly-opened
    /// `channel`, given the `prior` session we remembered across the blip:
    /// our session still running ⇒ re-CONNECT to its transport and resume
    /// (no re-LAUNCH/re-LOAD); another sender's app running ⇒ preempted
    /// (channel kept, hands-off, DEGRADED); nothing but the idle screen ⇒
    /// the caller re-establishes on the returned channel.
    async fn reconnect_triage(
        &mut self,
        mut channel: C::Channel,
        prior: &LaunchedApp,
    ) -> ReconnectTriage<C::Channel> {
        let status_id = self.next_request_id();
        if let Err(err) = channel.send(protocol::get_status_frame(status_id)).await {
            tracing::debug!(device = %self.device_id, error = %err, "cast GET_STATUS send failed");
            return ReconnectTriage::Resolved(self.channel_died());
        }
        let status = match self.pump_for_receiver_status(&mut channel).await {
            Ok(status) => status,
            Err(step) => return ReconnectTriage::Resolved(step),
        };
        if let Some(ours) = status
            .applications
            .iter()
            .find(|a| a.session_id == prior.session_id)
        {
            // Our receiver session survived the blip: re-CONNECT to its
            // transport and resume supervising — no re-LAUNCH, no re-LOAD,
            // playback untouched.
            if channel
                .send(protocol::connect_frame(&ours.transport_id))
                .await
                .is_err()
            {
                return ReconnectTriage::Resolved(self.channel_died());
            }
            self.app = Some(LaunchedApp {
                session_id: ours.session_id.clone(),
                transport_id: ours.transport_id.clone(),
            });
            self.channel = Some(channel);
            self.reload_due = None;
            return ReconnectTriage::Resolved(self.drive(LifecycleEvent::ProbeOk));
        }
        if status.applications.iter().any(|a| !a.is_idle_screen) {
            // Our session is gone and another sender's app runs (the idle
            // screen does not count): preempted during the blip — keep the
            // management channel, stay hands-off.
            self.channel = Some(channel);
            self.note_preempted();
            let _ = self.drive(LifecycleEvent::Reconnect);
            return ReconnectTriage::Resolved(self.drive(LifecycleEvent::DeviceFault));
        }
        // Our session is gone and nothing (or only the idle screen) runs:
        // the app idled out or crashed — the caller re-establishes.
        self.app = None;
        self.media_session_id = None;
        ReconnectTriage::ReEstablish(channel)
    }

    /// The reconnect `GET_STATUS` pump: receive frames (answering
    /// heartbeats) until the first `RECEIVER_STATUS` arrives, bounded by the
    /// same window as a LAUNCH answer. `Err` carries the driven step the
    /// failed attempt resolves to (the channel/lifecycle are already torn
    /// down/driven for the supervised reconnect).
    async fn pump_for_receiver_status(
        &mut self,
        channel: &mut C::Channel,
    ) -> Result<protocol::ReceiverStatusInfo, PollerStep> {
        let deadline = Instant::now() + self.config.launch_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let frame = match tokio::time::timeout(remaining, channel.recv()).await {
                Ok(Ok(frame)) => frame,
                Ok(Err(err)) => {
                    tracing::debug!(device = %self.device_id, error = %err, "cast channel dropped during GET_STATUS");
                    return Err(self.channel_died());
                }
                Err(_elapsed) => {
                    tracing::debug!(device = %self.device_id, "cast GET_STATUS timed out");
                    return Err(self.channel_died());
                }
            };
            self.last_inbound = Instant::now();
            match protocol::decode(&frame) {
                InboundMessage::Ping => {
                    if channel.send(protocol::pong_frame()).await.is_err() {
                        return Err(self.channel_died());
                    }
                }
                InboundMessage::ReceiverStatus(status) => return Ok(status),
                InboundMessage::CloseConnection => return Err(self.channel_died()),
                // Anything else just refreshes the liveness stamp above
                // while we wait for the status answer.
                _ => {}
            }
        }
    }

    /// Receive and handle exactly one inbound frame (the step the tests
    /// drive; the spawned loop runs the same handler). Returns the driven
    /// step — [`PollerStep::Unreachable`] when the channel died.
    pub async fn pump_step(&mut self) -> PollerStep {
        let Some(channel) = self.channel.as_mut() else {
            return step_for(self.lifecycle.state());
        };
        match channel.recv().await {
            Ok(frame) => self.handle_inbound(frame).await,
            Err(err) => {
                tracing::debug!(device = %self.device_id, error = %err, "cast channel dropped");
                self.channel_died()
            }
        }
    }

    /// Handle one decoded inbound frame.
    async fn handle_inbound(&mut self, frame: CastFrame) -> PollerStep {
        self.last_inbound = Instant::now();
        match protocol::decode(&frame) {
            InboundMessage::Ping => {
                if let Some(channel) = self.channel.as_mut() {
                    if channel.send(protocol::pong_frame()).await.is_err() {
                        return self.channel_died();
                    }
                }
            }
            InboundMessage::CloseConnection => return self.channel_died(),
            InboundMessage::ReceiverStatus(status) => {
                let Some(app) = self.app.as_ref() else {
                    // No claimed session (pre-establishment or preempted):
                    // a platform status broadcast carries nothing for us.
                    return step_for(self.lifecycle.state());
                };
                let ours_running = status
                    .applications
                    .iter()
                    .any(|a| a.session_id == app.session_id);
                if ours_running {
                    return step_for(self.lifecycle.state());
                }
                if status.applications.iter().any(|a| !a.is_idle_screen) {
                    // Our session was replaced by another sender's app — a
                    // foreign app, or a NEW session of the same Default
                    // Media Receiver: surface it, degrade, go hands-off
                    // (ADR-M011 — never fight).
                    self.note_preempted();
                    return self.drive(LifecycleEvent::DeviceFault);
                }
                // Our app died (idle-kill / crash) and nothing replaced it
                // (at most the idle screen): not a preemption. Drop the dead
                // identity and the channel; the supervised reconnect
                // re-establishes (re-LAUNCH + re-LOAD).
                self.app = None;
                self.media_session_id = None;
                self.reload_due = None;
                self.mode = "relaunching";
                self.channel = None;
                return self.drive(LifecycleEvent::DeviceFault);
            }
            InboundMessage::MediaStatus(entries) => {
                if self.preempted {
                    // The media on the device is another sender's: never
                    // attribute it to this session (a foreign PLAYING must
                    // not un-degrade a preempted session).
                } else if self.app.is_some() {
                    return self.handle_media_status(&entries);
                }
            }
            InboundMessage::LoadError { kind, reason } => {
                if !self.preempted && self.app.is_some() {
                    // The receiver answered our LOAD with a rejection: the
                    // TV is blank even though the channel heartbeats.
                    // Degrade honestly and schedule the bounded re-LOAD.
                    tracing::warn!(
                        device = %self.device_id,
                        kind = %kind,
                        reason = reason.as_deref().unwrap_or("unspecified"),
                        "cast LOAD rejected; re-LOAD scheduled"
                    );
                    self.media_session_id = None;
                    self.mode = "load-failed";
                    if self.reload_due.is_none() {
                        self.reload_due = Some(Instant::now() + self.config.reload_delay);
                    }
                    return self.drive(LifecycleEvent::DeviceFault);
                }
            }
            InboundMessage::LaunchError { reason } => {
                tracing::warn!(
                    device = %self.device_id,
                    reason = reason.as_deref().unwrap_or("unspecified"),
                    "cast receiver reported a launch error"
                );
                return self.drive(LifecycleEvent::DeviceFault);
            }
            // Pong / Unknown — and any kind a future decoder adds
            // (`InboundMessage` is `#[non_exhaustive]`) — only refresh the
            // liveness stamp above (a PONG is heartbeat traffic, an unknown
            // message is tolerated per the ADR-M011 drift posture).
            _ => {}
        }
        step_for(self.lifecycle.state())
    }

    /// Attribute a `MEDIA_STATUS` to our media session and drive the
    /// lifecycle from it. Only called established and not preempted.
    fn handle_media_status(&mut self, entries: &[MediaStatusEntry]) -> PollerStep {
        if entries.is_empty() {
            // Post-LOAD, an empty status array means the media session was
            // torn down receiver-side without a final IDLE: degrade and
            // schedule the bounded re-LOAD.
            if self.reload_due.is_none() {
                tracing::info!(
                    device = %self.device_id,
                    "cast media session vanished; re-LOAD scheduled"
                );
                self.reload_due = Some(Instant::now() + self.config.reload_delay);
            }
            self.media_session_id = None;
            self.mode = "no-media";
            return self.drive(LifecycleEvent::DeviceFault);
        }
        if let Some(ours) = self.media_session_id {
            if let Some(entry) = entries.iter().find(|e| e.media_session_id == Some(ours)) {
                return self.handle_player_state(entry.player_state, entry.idle_reason.as_deref());
            }
            if entries.iter().any(|e| e.player_state != PlayerState::Idle) {
                // Our media session is gone and another sender's media is
                // ACTIVE on the same app (the join-style takeover — no
                // receiver-status change at all): preempted.
                self.note_preempted();
                return self.drive(LifecycleEvent::DeviceFault);
            }
            // Only foreign IDLE rows: a dead session's tail — ignored
            // (never a preemption signal, never attributed to us).
            return step_for(self.lifecycle.state());
        }
        // A LOAD answer is awaited: adopt the first ACTIVE row as our media
        // session (a dying session's IDLE tail is never adopted). A raced
        // foreign LOAD can be mis-adopted here; the session-identity keys
        // above re-detect that as a preemption on the next status that
        // excludes it.
        if let Some(entry) = entries.iter().find(|e| e.player_state != PlayerState::Idle) {
            self.media_session_id = entry.media_session_id;
            // The receiver ACCEPTED our LOAD — an active media session is
            // attributed to us for the first time. This is the one moment a
            // cast session verifiably *started* (a REST accept or a bare LOAD
            // dispatch would lie about loads the receiver then rejects), so
            // it is what fires the started-at stamp (DEV-D3.1).
            if let Some(hook) = self.load_accepted.as_ref() {
                hook();
            }
            return self.handle_player_state(entry.player_state, entry.idle_reason.as_deref());
        }
        if let Some(entry) = entries.first() {
            // Every row IDLE while our LOAD is unanswered: ride the IDLE
            // handling (degrade + bounded re-LOAD) without adopting the
            // dead session.
            return self.handle_player_state(entry.player_state, entry.idle_reason.as_deref());
        }
        step_for(self.lifecycle.state())
    }

    /// Map a reported player state onto the lifecycle + conflated mode:
    /// PLAYING/BUFFERING/PAUSED recover (→ ONLINE); IDLE degrades and
    /// schedules the supervised re-LOAD (unless preempted).
    fn handle_player_state(&mut self, state: PlayerState, idle_reason: Option<&str>) -> PollerStep {
        self.mode = state.mode_token();
        match state {
            PlayerState::Playing | PlayerState::Buffering | PlayerState::Paused => {
                // Healthy playback cancels any pending IDLE re-LOAD (the
                // media recovered on its own).
                self.reload_due = None;
                self.drive(LifecycleEvent::Recover)
            }
            PlayerState::Idle => {
                if !self.preempted && self.reload_due.is_none() {
                    tracing::info!(
                        device = %self.device_id,
                        idle_reason = idle_reason.unwrap_or("unspecified"),
                        "cast media went IDLE; re-LOAD scheduled"
                    );
                    self.reload_due = Some(Instant::now() + self.config.reload_delay);
                }
                self.drive(LifecycleEvent::DeviceFault)
            } // The match is exhaustive in this crate even though `PlayerState`
              // is `#[non_exhaustive]` (the attribute only gates downstream
              // crates): a future player state must pick its lifecycle mapping
              // here explicitly — the decoder already maps unknown wire tokens
              // to `InboundMessage::Unknown` before this point.
        }
    }

    /// Re-LOAD the rendition (the supervisor's recovery after an IDLE, a
    /// rejected LOAD, or a vanished media session).
    async fn reload(&mut self) {
        self.reload_due = None;
        let Some(app) = self.app.clone() else {
            return;
        };
        // The re-LOAD creates a new receiver media session; ours is adopted
        // from its first active MEDIA_STATUS.
        self.media_session_id = None;
        let load_id = self.next_request_id();
        let frame = protocol::load_frame(load_id, &app.transport_id, &self.media);
        if let Some(channel) = self.channel.as_mut() {
            if channel.send(frame).await.is_err() {
                let _ = self.channel_died();
            }
        }
    }

    /// Handle one operator control command. Returns `false` when the actor
    /// must exit (a [`PollerControl::StopCast`] teardown).
    async fn handle_control(&mut self, command: PollerControl) -> bool {
        match command {
            PollerControl::SetVolume { percent } => {
                let request_id = self.next_request_id();
                let frame = protocol::set_volume_frame(request_id, percent);
                if let Some(channel) = self.channel.as_mut() {
                    if channel.send(frame).await.is_err() {
                        let _ = self.channel_died();
                    }
                } else {
                    tracing::debug!(
                        device = %self.device_id,
                        "cast SET_VOLUME dropped: no channel (session reconnecting)"
                    );
                }
                true
            }
            PollerControl::StopCast => {
                // Best-effort receiver STOP (this is what clears the TV — the
                // Default Media Receiver keeps playing when a sender merely
                // disconnects), then a courtesy CLOSE, then exit.
                let stop_id = self.next_request_id();
                if let (Some(channel), Some(app)) = (self.channel.as_mut(), self.app.as_ref()) {
                    let _ = channel
                        .send(protocol::stop_frame(stop_id, &app.session_id))
                        .await;
                    let _ = channel
                        .send(protocol::close_frame(PLATFORM_RECEIVER_ID))
                        .await;
                }
                false
            }
            PollerControl::SetMode { ref mode } => {
                tracing::debug!(
                    device = %self.device_id,
                    mode = %mode,
                    "set-mode is not a cast verb; ignored (cast devices have no work modes)"
                );
                true
            }
            PollerControl::SecretUpdated => {
                tracing::debug!(
                    device = %self.device_id,
                    "secret-updated is not a cast verb; ignored (CASTV2 has no sender auth)"
                );
                true
            } // Exhaustive in this crate (`#[non_exhaustive]` only gates
              // downstream crates): a future control verb must decide its cast
              // behaviour here explicitly.
        }
    }

    /// Spawn the actor as a supervised control-plane task, returning the
    /// transport-erased [`PollerHandle`] the DEV-A4 registry machinery
    /// manages (tombstones, replace-by-id, graceful stop).
    #[must_use]
    pub fn spawn(self) -> PollerHandle {
        let (control_tx, control_rx) = mpsc::channel(CONTROL_CHANNEL_DEPTH);
        let task = tokio::spawn(self.run(control_rx));
        PollerHandle::new(control_tx, task)
    }

    /// The supervised run loop: establish, then pump frames / heartbeat /
    /// re-LOAD / control until the handle is dropped (abort) or a
    /// [`PollerControl::StopCast`] exits it voluntarily.
    async fn run(mut self, mut control: mpsc::Receiver<PollerControl>) {
        loop {
            // (Re-)establish while the channel is down, retrying on the
            // 5 s cadence and servicing control commands between attempts.
            while self.channel.is_none() {
                let step = self.connect_step().await;
                if self.channel.is_some() {
                    break;
                }
                let _ = step;
                if self.reconnect_pause(&mut control).await {
                    return;
                }
            }

            // Steady state on the open channel.
            let mut next_ping = Instant::now() + self.config.ping_interval;
            loop {
                let event = {
                    let reload_due = self.reload_due;
                    let Some(channel) = self.channel.as_mut() else {
                        break;
                    };
                    tokio::select! {
                        biased;
                        maybe = control.recv() => LoopEvent::Control(maybe),
                        () = tokio::time::sleep_until(next_ping) => LoopEvent::PingTick,
                        () = sleep_until_or_pend(reload_due) => LoopEvent::ReloadDue,
                        result = channel.recv() => LoopEvent::Inbound(result),
                    }
                };
                match event {
                    LoopEvent::Control(None) => return,
                    LoopEvent::Control(Some(command)) => {
                        if !self.handle_control(command).await {
                            return;
                        }
                    }
                    LoopEvent::PingTick => {
                        if self.last_inbound.elapsed() >= self.config.pong_timeout {
                            // Dead: 20 s without inbound traffic (no PONG).
                            tracing::info!(
                                device = %self.device_id,
                                "cast session heartbeat expired; reconnecting"
                            );
                            let _ = self.channel_died();
                            break;
                        }
                        let ping = protocol::ping_frame();
                        if let Some(channel) = self.channel.as_mut() {
                            if channel.send(ping).await.is_err() {
                                let _ = self.channel_died();
                                break;
                            }
                        }
                        next_ping = Instant::now() + self.config.ping_interval;
                    }
                    LoopEvent::ReloadDue => self.reload().await,
                    LoopEvent::Inbound(Ok(frame)) => {
                        let _ = self.handle_inbound(frame).await;
                    }
                    LoopEvent::Inbound(Err(err)) => {
                        tracing::debug!(
                            device = %self.device_id,
                            error = %err,
                            "cast channel dropped; reconnecting"
                        );
                        let _ = self.channel_died();
                        break;
                    }
                }
                if self.channel.is_none() {
                    break;
                }
            }

            // Channel down: wait the reconnect cadence (servicing control),
            // then loop back to the establishment phase.
            if self.reconnect_pause(&mut control).await {
                return;
            }
        }
    }

    /// Wait one reconnect cadence, servicing operator control commands while
    /// waiting. Returns `true` when the actor must exit (a
    /// [`PollerControl::StopCast`] teardown ran, or the control channel
    /// closed — the handle is gone).
    async fn reconnect_pause(&mut self, control: &mut mpsc::Receiver<PollerControl>) -> bool {
        let retry = tokio::time::sleep(self.config.reconnect_delay);
        tokio::pin!(retry);
        loop {
            tokio::select! {
                biased;
                maybe = control.recv() => match maybe {
                    Some(command) => {
                        if !self.handle_control(command).await {
                            return true;
                        }
                    }
                    None => return true,
                },
                () = &mut retry => return false,
            }
        }
    }
}

/// One steady-loop wakeup.
enum LoopEvent {
    /// An operator control command (or the channel closed).
    Control(Option<PollerControl>),
    /// The heartbeat timer fired.
    PingTick,
    /// The IDLE re-LOAD deadline passed.
    ReloadDue,
    /// An inbound frame (or the channel died).
    Inbound(Result<CastFrame, CastChannelError>),
}

/// Sleep until `deadline`, or pend forever when none is scheduled.
async fn sleep_until_or_pend(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Map a lifecycle state onto the shared poller step vocabulary.
fn step_for(state: DeviceState) -> PollerStep {
    match state {
        DeviceState::Online => PollerStep::Online,
        DeviceState::Degraded => PollerStep::Degraded,
        DeviceState::AuthFailed => PollerStep::AuthFailed,
        _ => PollerStep::Unreachable,
    }
}

// ---------------------------------------------------------------------------
// The scripted seam (always compiled): socket-free channels for tests.
// ---------------------------------------------------------------------------

/// One scripted inbound event on a [`ScriptedChannel`].
#[derive(Debug, Clone)]
pub enum ScriptedInbound {
    /// Deliver this frame.
    Frame(CastFrame),
    /// Sleep this long, then continue with the next event (paused-clock
    /// tests use this to sequence inbound traffic against the actor's
    /// timers).
    Wait(Duration),
    /// Pend forever (a silent-but-open channel).
    Hang,
    /// Fail the receive (the channel dropped).
    Drop,
}

/// The log of frames an actor sent on a scripted channel, shared with the
/// test that scripted it.
pub type SentFrames = Arc<std::sync::Mutex<Vec<CastFrame>>>;

/// A scripted [`CastChannel`]: replays a fixed inbound script and logs every
/// sent frame. The whole driver logic above the seam runs against this —
/// socket-free by construction.
#[derive(Debug)]
pub struct ScriptedChannel {
    script: VecDeque<ScriptedInbound>,
    sent: SentFrames,
}

impl ScriptedChannel {
    /// Build a channel that replays `script`, returning the shared sent-log.
    #[must_use]
    pub fn new(script: Vec<ScriptedInbound>) -> (Self, SentFrames) {
        let sent: SentFrames = Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            Self {
                script: script.into(),
                sent: Arc::clone(&sent),
            },
            sent,
        )
    }
}

impl CastChannel for ScriptedChannel {
    async fn send(&mut self, frame: CastFrame) -> Result<(), CastChannelError> {
        match self.sent.lock() {
            Ok(mut sent) => sent.push(frame),
            // A test that panicked while holding the log lock is already
            // failing; keep the channel usable rather than cascade.
            Err(poisoned) => poisoned.into_inner().push(frame),
        }
        Ok(())
    }

    async fn recv(&mut self) -> Result<CastFrame, CastChannelError> {
        loop {
            match self.script.pop_front() {
                Some(ScriptedInbound::Frame(frame)) => return Ok(frame),
                Some(ScriptedInbound::Wait(duration)) => tokio::time::sleep(duration).await,
                Some(ScriptedInbound::Hang) | None => std::future::pending().await,
                Some(ScriptedInbound::Drop) => {
                    return Err(CastChannelError::new("scripted channel drop"));
                }
            }
        }
    }
}

/// A scripted [`CastConnector`]: each `connect` hands out the next scripted
/// channel; with none left, the connect is refused. Counts connects so tests
/// can assert the reconnect cadence.
#[derive(Debug)]
pub struct ScriptedConnector {
    channels: std::sync::Mutex<VecDeque<ScriptedChannel>>,
    connects: Arc<std::sync::atomic::AtomicUsize>,
}

impl ScriptedConnector {
    /// A connector handing out `channels` in order (then refusing).
    #[must_use]
    pub fn new(channels: Vec<ScriptedChannel>) -> Self {
        Self {
            channels: std::sync::Mutex::new(channels.into()),
            connects: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// A shared counter of `connect` attempts (successful or refused).
    #[must_use]
    pub fn connect_count(&self) -> Arc<std::sync::atomic::AtomicUsize> {
        Arc::clone(&self.connects)
    }
}

impl CastConnector for ScriptedConnector {
    type Channel = ScriptedChannel;

    async fn connect(&self, _authority: &str) -> Result<ScriptedChannel, CastChannelError> {
        self.connects
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let next = match self.channels.lock() {
            Ok(mut channels) => channels.pop_front(),
            Err(poisoned) => poisoned.into_inner().pop_front(),
        };
        next.ok_or_else(|| CastChannelError::new("scripted connect refused"))
    }
}
