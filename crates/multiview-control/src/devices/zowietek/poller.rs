//! The `zowietek` **poller actor** (DEV-A4, ADR-M009): the supervised
//! control-plane task that owns one [`ZowietekDriver`] and drives the DEV-A3
//! [`DeviceLifecycle`] from live device state.
//!
//! ## What the actor does
//!
//! On spawn it **adopts** the device (login → probe → enumerate the three
//! facets → publish), then **polls** the status groups at the configured cadence
//! (≤1 Hz per the brief), feeding each outcome through the
//! [`DeviceLifecycle`] transition table as a [`LifecycleEvent`] — never an
//! ad-hoc target state. A device-reported fault is a `DeviceFault` (→ `DEGRADED`);
//! a recovered poll is a `Recover` (→ `ONLINE`); a dropped socket is `Unreachable`
//! (→ `UNREACHABLE`); a credential rejection is `AuthRejected` (→ `AUTH_FAILED`).
//! The published `device.status` is exactly the lifecycle's output state.
//!
//! ## `desired_mode` convergence on adopt and reconnect
//!
//! A device declared with `desired_mode` (the config-as-code field) is
//! **re-converged onto that mode** whenever an adopt or reconnect step reaches
//! `ONLINE`: the actor runs the driver's close-before-open
//! [`converge_mode`](super::ZowietekDriver::converge_mode) (declared DEV-class
//! impact, `device.mode` published) without any operator command, so a box that
//! powered up — or came back from a reboot — in the wrong mode is restored to
//! its declared mode. Convergence never runs unless `ONLINE` was actually
//! reached (in particular, never after `AUTH_FAILED`), and a convergence
//! failure is logged and retried on the next adopt/reconnect pass — it never
//! crashes the actor. An operator `set-mode` updates the actor's desired mode,
//! so the last operator intent is what later passes re-converge onto.
//!
//! ## Supervised reconnect + the `AUTH_FAILED` breaker
//!
//! On `UNREACHABLE` the actor reconnects with **capped exponential backoff plus
//! full jitter** (the same scheme inputs use — `multiview-input`'s `reconnect`),
//! re-converging `desired_mode` on success (above). The breaker is the A3 state
//! machine: a `Reconnect` from `AUTH_FAILED` is a no-op (the transition table
//! ignores it), so after a credential rejection the actor issues **no** further
//! login — no reconnect storm against a device that rejected our secret. Only a
//! [`secret_updated`](ZowietekPoller::secret_updated) (the operator updated the
//! stored `secret_ref`) re-arms a probe (`AUTH_FAILED` → `ADOPTING`).
//!
//! ## Isolation (invariant #10)
//!
//! Everything here is control-plane. The actor publishes through the
//! [`DeviceBroadcaster`] (drop-oldest broadcast + latest-wins registry) and
//! mutates only control-plane stores; it never makes the engine await anything,
//! and a hung device can at worst stall its own task. The control channel
//! ([`PollerControl`]) is a bounded, non-blocking `mpsc` — `set-mode` dispatch
//! cannot back-pressure the engine.

use std::sync::Arc;
use std::time::Duration;

use multiview_events::DeviceState;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::client::{ZowietekClientError, ZowietekTransport};
use super::ZowietekDriver;
use crate::devices::registry::DeviceStatusRegistry;
use crate::devices::state_machine::{DeviceLifecycle, LifecycleEvent};

/// Full-scale value for the injected jitter fraction (mirrors
/// `multiview_input::reconnect::JITTER_SCALE`): a `jitter` of `JITTER_SCALE`
/// selects the full backoff ceiling; `0` selects no delay; values in between
/// interpolate linearly.
const JITTER_SCALE: u64 = 1_000_000;

/// The bounded depth of a poller's control channel. A handful of in-flight
/// operator commands (set-mode / secret update) is ample; beyond it the channel
/// is full and a new command is shed rather than queued (drop-newest at the
/// sender via `try_send`), so the route can never block on the actor (inv #10).
const CONTROL_CHANNEL_DEPTH: usize = 16;

/// Supervised-reconnect tuning for a poller actor.
///
/// A plain configuration record (not `#[non_exhaustive]` so tests can build it
/// directly). [`PollerConfig::from_reconnect_policy`] maps a config-as-code
/// [`ReconnectPolicy`](multiview_config::ReconnectPolicy) onto it.
#[derive(Debug, Clone, Copy)]
pub struct PollerConfig {
    /// The steady-state poll period once ONLINE (≤1 Hz per group; the brief's
    /// status cadence). The default is one second.
    pub poll_period: Duration,
    /// The first-retry backoff ceiling after an UNREACHABLE drop.
    pub backoff_base: Duration,
    /// The backoff ceiling cap (capped exponential growth).
    pub backoff_max: Duration,
    /// The multiplicative backoff growth factor (typically 2).
    pub backoff_factor: u32,
}

impl Default for PollerConfig {
    fn default() -> Self {
        Self {
            poll_period: Duration::from_secs(1),
            backoff_base: Duration::from_millis(500),
            backoff_max: Duration::from_secs(30),
            backoff_factor: 2,
        }
    }
}

impl PollerConfig {
    /// A fast config for tests: sub-millisecond cadences so a paused/real clock
    /// drives the loop quickly without real sleeps dominating.
    #[must_use]
    pub fn test_fast() -> Self {
        Self {
            poll_period: Duration::from_millis(1),
            backoff_base: Duration::from_millis(1),
            backoff_max: Duration::from_millis(4),
            backoff_factor: 2,
        }
    }

    /// Map a config-as-code [`ReconnectPolicy`](multiview_config::ReconnectPolicy)
    /// onto the backoff bounds, keeping the default poll period and factor.
    #[must_use]
    pub fn from_reconnect_policy(policy: multiview_config::ReconnectPolicy) -> Self {
        Self {
            backoff_base: Duration::from_millis(u64::from(policy.initial_ms)),
            backoff_max: Duration::from_millis(u64::from(policy.max_ms)),
            ..Self::default()
        }
    }
}

/// Capped exponential backoff with full jitter (the supervised-reconnect scheme
/// inputs use). The delay before reconnect attempt `n` is drawn from
/// `[0, ceiling_n]`, `ceiling_n = min(max, base * factor^(n-1))`.
///
/// The jitter source is injected and integer-valued so the policy is exact and
/// deterministically testable; the live actor passes a fresh uniform value each
/// call.
#[derive(Debug)]
struct Backoff {
    base: Duration,
    max: Duration,
    factor: u32,
    attempts: u32,
}

impl Backoff {
    fn new(config: &PollerConfig) -> Self {
        Self {
            base: config.backoff_base,
            max: config.backoff_max,
            factor: config.backoff_factor,
            attempts: 0,
        }
    }

    /// Reset the schedule after a successful reconnect.
    fn reset(&mut self) {
        self.attempts = 0;
    }

    /// The current ceiling in nanoseconds, saturating at the configured max.
    fn ceiling_ns(&self) -> u64 {
        let base_ns = duration_to_nanos(self.base);
        let max_ns = duration_to_nanos(self.max);
        let factor = u64::from(self.factor.max(1));
        let mut ceiling = base_ns;
        for _ in 0..self.attempts {
            ceiling = ceiling.saturating_mul(factor);
            if ceiling >= max_ns {
                return max_ns;
            }
        }
        ceiling.min(max_ns)
    }

    /// Record one failed attempt and return the delay before the next retry,
    /// selecting a point within the full-jitter window `[0, ceiling]`.
    fn next_delay(&mut self, jitter: u64) -> Duration {
        let ceiling_ns = self.ceiling_ns();
        self.attempts = self.attempts.saturating_add(1);
        let frac = jitter.min(JITTER_SCALE);
        let delay_ns =
            u128::from(ceiling_ns).saturating_mul(u128::from(frac)) / u128::from(JITTER_SCALE);
        let delay_ns = u64::try_from(delay_ns).unwrap_or(u64::MAX);
        Duration::from_nanos(delay_ns)
    }
}

fn duration_to_nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// A fresh uniform jitter fraction in `[0, JITTER_SCALE]`, drawn from the same
/// CSPRNG (`getrandom`, via `uuid::Uuid::new_v4`) the rest of this crate uses —
/// no extra `rand` dependency. The first 8 bytes of a v4 UUID are random; mapped
/// modulo `JITTER_SCALE + 1` they give a uniform-enough spread for jitter (the
/// tiny modulo bias is irrelevant to a backoff window).
fn random_jitter() -> u64 {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let mut acc = [0u8; 8];
    acc.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(acc) % (JITTER_SCALE + 1)
}

/// An operator command dispatched to a running poller actor over its bounded,
/// non-blocking control channel.
///
/// `#[non_exhaustive]` so future device actions (reboot/identify wired to the
/// live transport) can join without a breaking change.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PollerControl {
    /// Converge the device to `mode` (the `set-mode` route's dispatch): the
    /// actor runs the driver's plan → converge close-before-open and publishes
    /// the DEV-class `device.mode` outcome.
    SetMode {
        /// The desired converged work mode (driver vocabulary, e.g. `decoder`).
        mode: String,
    },
    /// The operator updated the stored credential (`secret_ref`): re-arm a probe
    /// out of `AUTH_FAILED` (drives the lifecycle breaker closed).
    SecretUpdated,
}

/// The outcome of one poller step — what the lifecycle resolved to (or that the
/// breaker is open). Returned by the step functions so the loop and the tests
/// can observe the driven transition without scraping the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PollerStep {
    /// The lifecycle is `ONLINE`.
    Online,
    /// The lifecycle is `DEGRADED` (device-reported fault; channel still up).
    Degraded,
    /// The lifecycle is `UNREACHABLE` (the channel dropped).
    Unreachable,
    /// The lifecycle is `AUTH_FAILED` (a credential rejection opened the breaker).
    AuthFailed,
    /// A reconnect was attempted while the breaker is OPEN: a no-op (no login
    /// issued) — the device stays `AUTH_FAILED`.
    BreakerOpen,
}

/// A handle to a spawned poller actor: the join handle plus the bounded control
/// channel sender. Dropping the handle **aborts** the task (the device is gone /
/// the control plane is shutting down), so no actor outlives its registration.
#[derive(Debug)]
pub struct PollerHandle {
    control: mpsc::Sender<PollerControl>,
    /// The actor task. `Option` so both [`PollerHandle::into_join_handle`] and
    /// [`Drop`] can `take()` it (a `Drop` type cannot move a field out
    /// otherwise); `None` only after the handle has been consumed.
    task: Option<JoinHandle<()>>,
}

impl PollerHandle {
    /// Try to dispatch a control command without blocking (drop-newest if the
    /// channel is full or the actor is gone). Returns `true` if enqueued.
    ///
    /// Non-blocking by construction (`try_send`): the route caller never awaits
    /// the actor, so dispatch can never back-pressure anything (inv #10).
    // The bool is an advisory enqueued/shed indicator, fire-and-forget at the
    // route boundary (the `set-mode` 202 stands either way); a caller may
    // legitimately ignore it, so `#[must_use]` would mislead.
    #[allow(clippy::must_use_candidate)]
    pub fn try_dispatch(&self, command: PollerControl) -> bool {
        self.control.try_send(command).is_ok()
    }

    /// Consume the handle, returning the underlying [`JoinHandle`] (e.g. to
    /// `abort()` + await it deterministically in a test or at shutdown).
    #[must_use]
    pub fn into_join_handle(mut self) -> JoinHandle<()> {
        // Take the task so the `Drop` impl below does not also abort it (we hand
        // ownership to the caller). `unwrap_or_else` is unreachable in practice
        // (the field is only `None` after this consumes it), but stays
        // panic-free by spawning a trivial already-finished task as a fallback.
        self.task.take().unwrap_or_else(|| tokio::spawn(async {}))
    }
}

impl Drop for PollerHandle {
    fn drop(&mut self) {
        // Stop the actor when its handle is dropped (device removal / shutdown):
        // the device is gone, so its supervised poller must not keep probing.
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// The supervised poller actor for one `zowietek` device.
///
/// Owns the [`ZowietekDriver`] (and thus the typed client over the transport
/// seam), the [`DeviceLifecycle`] it drives, and the latest-wins
/// [`DeviceStatusRegistry`] it reflects the lifecycle state into. Construct with
/// [`ZowietekPoller::new`]; drive it step-by-step (the unit of behaviour the
/// tests exercise) or run it to completion with [`ZowietekPoller::spawn`].
pub struct ZowietekPoller<T: ZowietekTransport> {
    device_id: String,
    driver: ZowietekDriver<T>,
    status: Arc<DeviceStatusRegistry>,
    lifecycle: DeviceLifecycle,
    /// The device's management host (a bracketed IPv6 literal or hostname),
    /// addressing the served RTSP mounts the source facet enumerates.
    host: String,
    config: PollerConfig,
    backoff: Backoff,
    /// The mode this device is converged onto whenever an adopt/reconnect step
    /// reaches `ONLINE` — seeded from the config-as-code `desired_mode` field
    /// (threaded by the factory) and updated by an operator `set-mode`. `None`
    /// means the device keeps whatever mode it is in (no convergence).
    desired_mode: Option<String>,
}

impl<T: ZowietekTransport> ZowietekPoller<T> {
    /// Build a poller for `device_id` over `driver`, reflecting the lifecycle
    /// into `status` and enumerating the source facet at `host`.
    ///
    /// The lifecycle starts in `ADOPTING` (the registry-entry start state); the
    /// first [`adopt_step`](ZowietekPoller::adopt_step) resolves it.
    #[must_use]
    pub fn new(
        device_id: &str,
        driver: ZowietekDriver<T>,
        status: Arc<DeviceStatusRegistry>,
        host: &str,
        config: PollerConfig,
    ) -> Self {
        let backoff = Backoff::new(&config);
        Self {
            device_id: device_id.to_owned(),
            driver,
            status,
            lifecycle: DeviceLifecycle::new(),
            host: host.to_owned(),
            config,
            backoff,
            desired_mode: None,
        }
    }

    /// Set the `desired_mode` this poller converges the device onto whenever an
    /// adopt/reconnect step reaches `ONLINE` (the config-as-code
    /// `Device::desired_mode`, threaded by the factory). `None` (the default)
    /// performs no convergence — the device keeps the mode it powered up in.
    #[must_use]
    pub fn with_desired_mode(mut self, desired_mode: Option<String>) -> Self {
        self.desired_mode = desired_mode;
        self
    }

    /// The device id this poller manages.
    #[must_use]
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// The latest conflated `device.status` snapshot the broadcaster published
    /// for this device (read from the shared status registry), if any. The same
    /// value `GET /devices/{id}/status` serves — a direct read-back so a caller
    /// (or test) can confirm the published status without subscribing.
    #[must_use]
    pub fn published_status(&self) -> Option<multiview_events::DeviceStatus> {
        self.status.snapshot(&self.device_id)
    }

    /// The current lifecycle state.
    #[must_use]
    pub fn state(&self) -> DeviceState {
        self.lifecycle.state()
    }

    /// Adopt the device: login → probe → enumerate the facets → drive the
    /// lifecycle to `ONLINE` (or `AUTH_FAILED` / `UNREACHABLE`) and publish,
    /// then converge `desired_mode` (when declared) onto the freshly-ONLINE
    /// device — close-before-open, DEV-class impact, `device.mode` published.
    ///
    /// Refuses to re-login while the breaker is open (`AUTH_FAILED`): the lifecycle
    /// only re-arms a probe on a [`secret_updated`](ZowietekPoller::secret_updated),
    /// so an adopt step in `AUTH_FAILED` is a [`PollerStep::BreakerOpen`] no-op.
    /// `desired_mode` convergence runs **only** when this step actually reached
    /// `ONLINE` — never after `AUTH_FAILED`/`UNREACHABLE`.
    pub async fn adopt_step(&mut self) -> PollerStep {
        if self.breaker_open() {
            return PollerStep::BreakerOpen;
        }
        match self.driver.probe_and_adopt().await {
            Ok(()) => {
                let step = self.drive(LifecycleEvent::ProbeOk);
                // Enumerate the facets so the projection routes serve real data
                // at runtime (the source facet is I/O-free; the output facet is
                // best-effort — an encoder-mode box has no decode table).
                self.enumerate_facets().await;
                if step == PollerStep::Online {
                    self.converge_desired_mode().await;
                }
                step
            }
            Err(err) => self.drive_from_error(&err),
        }
    }

    /// Poll the device's status groups once and drive the lifecycle from the
    /// outcome: a device-reported fault degrades it (`DeviceFault`), a healthy
    /// poll recovers it (`Recover`), a dropped channel rides to UNREACHABLE
    /// (`Unreachable`), a credential rejection opens the breaker (`AuthRejected`).
    pub async fn poll_step(&mut self) -> PollerStep {
        if self.breaker_open() {
            return PollerStep::BreakerOpen;
        }
        match self.driver.poll_status().await {
            Ok(healthy) => {
                let event = if healthy {
                    LifecycleEvent::Recover
                } else {
                    LifecycleEvent::DeviceFault
                };
                self.drive(event)
            }
            Err(err) => self.drive_from_error(&err),
        }
    }

    /// Attempt one supervised reconnect: re-establish the channel, re-converge
    /// to `ONLINE` (driving `Reconnect`), then re-converge `desired_mode` (when
    /// declared) — a box that came back from a reboot in the wrong mode is
    /// restored to its declared mode without an operator command.
    ///
    /// While the breaker is open (`AUTH_FAILED`) this is a **no-op** — it issues no
    /// login (the `Reconnect` event is ignored by the transition table), so a
    /// device that rejected our secret is never hammered. The next
    /// [`secret_updated`](ZowietekPoller::secret_updated) re-arms a probe.
    /// `desired_mode` convergence runs **only** when this step actually reached
    /// `ONLINE`.
    pub async fn reconnect_step(&mut self) -> PollerStep {
        if self.breaker_open() {
            // The A3 breaker: no login, no storm. Drive the (ignored) Reconnect
            // through the transition table (it is a no-op from AUTH_FAILED, so
            // the state is unchanged and republished latest-wins), but issue zero
            // I/O and report the breaker-open outcome distinctly.
            let _ = self.drive(LifecycleEvent::Reconnect);
            return PollerStep::BreakerOpen;
        }
        match self.driver.reconnect().await {
            Ok(()) => {
                self.backoff.reset();
                let step = self.drive(LifecycleEvent::Reconnect);
                if step == PollerStep::Online {
                    self.converge_desired_mode().await;
                }
                step
            }
            Err(err) => self.drive_from_error(&err),
        }
    }

    /// The operator updated the stored credential: re-arm a probe out of
    /// `AUTH_FAILED` (drives the lifecycle breaker closed; `AUTH_FAILED` →
    /// `ADOPTING`).
    pub fn secret_updated(&mut self) {
        let _ = self.drive(LifecycleEvent::SecretUpdated);
        self.backoff.reset();
    }

    /// Handle one operator [`PollerControl`] command (the dispatch the spawned
    /// loop runs on each received control message).
    pub async fn handle_control(&mut self, command: PollerControl) {
        match command {
            PollerControl::SetMode { mode } => {
                // The operator's set-mode is the new desired mode: record it so
                // every later adopt/reconnect pass re-converges onto the last
                // operator intent (and a failed apply below is retried there).
                self.desired_mode = Some(mode.clone());
                // Run the close-before-open convergence; the driver declares the
                // DEV-class impact and publishes the device.mode outcome. A
                // failure is published as a device.mode Failed by the driver and
                // surfaced here — re-converged on the next adopt/reconnect pass
                // (the desired mode recorded above).
                if let Err(err) = self.driver.converge_mode(&mode).await {
                    tracing::warn!(
                        device = %self.device_id,
                        mode = %mode,
                        error = %err,
                        "zowietek set-mode convergence failed; re-converges on the next adopt/reconnect pass"
                    );
                }
            }
            PollerControl::SecretUpdated => self.secret_updated(),
        }
    }

    /// Converge the device onto its declared `desired_mode`, if any — the pass
    /// run after an adopt/reconnect step reaches `ONLINE`. A no-op when no
    /// desired mode is declared or the device is already in it (the driver's
    /// plan short-circuits). A convergence failure is logged and left for the
    /// next adopt/reconnect pass — it never crashes the poller.
    async fn converge_desired_mode(&mut self) {
        let Some(desired) = self.desired_mode.clone() else {
            return;
        };
        if let Err(err) = self.driver.converge_mode(&desired).await {
            tracing::warn!(
                device = %self.device_id,
                mode = %desired,
                error = %err,
                "zowietek desired_mode convergence failed after coming ONLINE; \
                 re-converges on the next adopt/reconnect pass"
            );
        }
    }

    /// Whether the supervised-reconnect breaker is open (the device is in
    /// `AUTH_FAILED`): no login is issued until a secret update re-arms a probe.
    #[must_use]
    fn breaker_open(&self) -> bool {
        self.lifecycle.state() == DeviceState::AuthFailed
    }

    /// Drive the lifecycle by `event` and publish the resulting state as the
    /// conflated `device.status` (via the broadcaster behind the driver), then
    /// map it to a [`PollerStep`]. The published state is **always** the
    /// transition table's output — never an ad-hoc target.
    fn drive(&mut self, event: LifecycleEvent) -> PollerStep {
        self.lifecycle.apply(event);
        let state = self.lifecycle.state();
        // Reflect the lifecycle output into the latest-wins status registry +
        // publish the conflated status through the broadcaster (drop-oldest; inv
        // #10). The broadcaster's registry IS the shared `self.status` registry,
        // so `published_status()` reads back exactly this value.
        self.driver.publish_state(state);
        step_for(state)
    }

    /// Map a client error to its lifecycle event and drive it (`AUTH_FAILED` on a
    /// credential rejection, `UNREACHABLE` otherwise).
    fn drive_from_error(&mut self, err: &ZowietekClientError) -> PollerStep {
        let event = lifecycle_event_for(err);
        self.drive(event)
    }

    /// Enumerate the device's facets into the driver registry the projection
    /// routes read, **workmode-aware** (ADR-M009 §3.2/§3.3):
    ///
    /// * the **source facet** (served RTSP mounts) is always enumerated — it is
    ///   I/O-free and an encoder serves source candidates;
    /// * the **output facet** (the live decode table) is enumerated **only for a
    ///   decoder-mode box**. An encoder-mode box has no decode table and would
    ///   reject the read with status `00004`, so the call is skipped rather than
    ///   issued-and-rejected (no needless wire round-trip, no spurious error).
    ///
    /// Best-effort: a facet read failure is logged, never an adoption failure.
    async fn enumerate_facets(&self) {
        if let Err(err) = self.driver.enumerate_source_candidates(&self.host) {
            tracing::debug!(
                device = %self.device_id,
                error = %err,
                "zowietek source-candidate enumeration skipped"
            );
        }
        // Only a decoder-mode box has a decode table to enumerate as output
        // targets; for an encoder we skip the read entirely (it would reject).
        if self.driver.workmode() == Some(super::WorkMode::Decoder) {
            if let Err(err) = self.driver.enumerate_output_targets().await {
                tracing::debug!(
                    device = %self.device_id,
                    error = %err,
                    "zowietek output-target enumeration failed on a decode-mode box"
                );
            }
        }
    }

    /// Spawn the actor as a supervised control-plane task, returning a
    /// [`PollerHandle`]. The task adopts, then runs the poll/reconnect loop,
    /// servicing control commands, until the handle is dropped (abort) or the
    /// runtime stops.
    #[must_use]
    pub fn spawn(self) -> PollerHandle
    where
        T: 'static,
    {
        let (control_tx, control_rx) = mpsc::channel(CONTROL_CHANNEL_DEPTH);
        let task = tokio::spawn(self.run(control_rx));
        PollerHandle {
            control: control_tx,
            task: Some(task),
        }
    }

    /// The supervised run loop: adopt, then drive the lifecycle from polls and
    /// reconnects on the configured cadence, interleaving operator control
    /// commands. Never returns under normal operation (the task is aborted on
    /// handle drop / shutdown).
    async fn run(mut self, mut control: mpsc::Receiver<PollerControl>) {
        // Adopt first (drains an early control command if one is already queued).
        loop {
            tokio::select! {
                biased;
                Some(command) = control.recv() => self.handle_control(command).await,
                () = self.adopt_or_wait() => break,
            }
        }

        // Steady state: poll on cadence; on UNREACHABLE, reconnect with
        // backoff+jitter; service control commands as they arrive.
        loop {
            let next = self.next_delay();
            tokio::select! {
                biased;
                maybe = control.recv() => match maybe {
                    Some(command) => self.handle_control(command).await,
                    // Every sender dropped (handle gone): stop the actor.
                    None => break,
                },
                () = tokio::time::sleep(next) => {
                    self.tick().await;
                }
            }
        }
    }

    /// Adopt the device once; on failure wait a backoff before the loop retries
    /// (so a device that is down at boot does not busy-loop the adopt).
    async fn adopt_or_wait(&mut self) {
        match self.adopt_step().await {
            PollerStep::Online | PollerStep::Degraded => {}
            // Down / breaker-open at boot: back off before the run loop's poll
            // path takes over the supervised reconnect.
            PollerStep::Unreachable | PollerStep::AuthFailed | PollerStep::BreakerOpen => {
                tokio::time::sleep(self.next_delay()).await;
            }
        }
    }

    /// One steady-state tick: poll when reachable, else attempt a supervised
    /// reconnect. The lifecycle decides which (the breaker no-ops a reconnect in
    /// `AUTH_FAILED`).
    async fn tick(&mut self) {
        match self.lifecycle.state() {
            DeviceState::Unreachable => {
                let _ = self.reconnect_step().await;
            }
            DeviceState::AuthFailed => {
                // Breaker open: do nothing on the timer (no storm). A secret
                // update (a control command) is the only path out.
            }
            _ => {
                let _ = self.poll_step().await;
            }
        }
    }

    /// The delay before the next loop action: the steady poll period when
    /// reachable, else a backoff+jitter draw while UNREACHABLE.
    fn next_delay(&mut self) -> Duration {
        if self.lifecycle.state() == DeviceState::Unreachable {
            self.backoff.next_delay(random_jitter())
        } else {
            self.config.poll_period
        }
    }
}

/// The lifecycle event a client error implies: a credential rejection opens the
/// breaker (`AuthRejected`); anything else rides to UNREACHABLE (`Unreachable`).
fn lifecycle_event_for(err: &ZowietekClientError) -> LifecycleEvent {
    match err {
        ZowietekClientError::Status { status, .. } if is_auth_status(status) => {
            LifecycleEvent::AuthRejected
        }
        _ => LifecycleEvent::Unreachable,
    }
}

/// Whether a status code is a credential-rejection code (`00002`/`00003`).
fn is_auth_status(status: &str) -> bool {
    matches!(status.trim().parse::<u32>(), Ok(2 | 3))
}

/// Map a lifecycle [`DeviceState`] to the poller's step vocabulary.
fn step_for(state: DeviceState) -> PollerStep {
    match state {
        DeviceState::Online => PollerStep::Online,
        DeviceState::Degraded => PollerStep::Degraded,
        DeviceState::AuthFailed => PollerStep::AuthFailed,
        // `UNREACHABLE` maps to Unreachable; `ADOPTING`/`DISCOVERED` never
        // persist as a published poll outcome here, and any other non-terminal
        // state is treated as "not reachable" (the supervised-reconnect path).
        _ => PollerStep::Unreachable,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{random_jitter, step_for, Backoff, PollerConfig, JITTER_SCALE};
    use multiview_events::DeviceState;

    #[test]
    fn backoff_grows_capped_and_resets() {
        let config = PollerConfig {
            backoff_base: std::time::Duration::from_millis(100),
            backoff_max: std::time::Duration::from_millis(800),
            backoff_factor: 2,
            ..PollerConfig::default()
        };
        let mut b = Backoff::new(&config);
        // Full jitter selects the full ceiling, which grows 100, 200, 400, 800,
        // then caps at 800.
        let ms: Vec<u128> = (0..5)
            .map(|_| b.next_delay(JITTER_SCALE).as_millis())
            .collect();
        assert_eq!(ms, vec![100, 200, 400, 800, 800], "capped exponential");
        b.reset();
        assert_eq!(b.next_delay(JITTER_SCALE).as_millis(), 100, "reset to base");
    }

    #[test]
    fn zero_jitter_is_zero_delay_full_jitter_is_the_ceiling() {
        let mut b = Backoff::new(&PollerConfig::default());
        assert_eq!(b.next_delay(0), std::time::Duration::ZERO, "no jitter");
    }

    #[test]
    fn random_jitter_is_in_range() {
        for _ in 0..256 {
            assert!(random_jitter() <= JITTER_SCALE);
        }
    }

    #[test]
    fn step_maps_each_terminal_state() {
        assert_eq!(step_for(DeviceState::Online), super::PollerStep::Online);
        assert_eq!(step_for(DeviceState::Degraded), super::PollerStep::Degraded);
        assert_eq!(
            step_for(DeviceState::Unreachable),
            super::PollerStep::Unreachable
        );
        assert_eq!(
            step_for(DeviceState::AuthFailed),
            super::PollerStep::AuthFailed
        );
    }
}
