//! The off-hot-path **timing-status publisher** (ADR-M010, DEV-C1): derives
//! the per-program outbound presentation epoch at ~1 Hz and pushes
//! [`multiview_events::TimingStatus`] — `{stream_id, epoch, link_offset_ns,
//! clock_source, clock_quality, groups}` — onto the engine's outbound
//! drop-oldest broadcast (the same channel the control-WS pump forwards on
//! `Topic::Devices`, conflated latest-wins per ADR-RT007), while mirroring the
//! same epoch into the shared HLS-PDT cell so every ADR-M010 surface agrees
//! on one anchor.
//!
//! ## Invariant #1 / #10 argument
//!
//! This task never touches the engine: it reads the run's epoch **anchor**
//! (the immutable tick-0 seed + the shared monotonic [`TimeSource`] — a
//! wait-free read) from a lock-free slot the run fills at clock-seed time,
//! samples the wall clock, and publishes through `EnginePublisher`'s
//! never-blocking `broadcast::send` plus the `SharedEpoch` cell. A stalled WS
//! client lags (drop-oldest); a dead PTP device or denied kernel read only
//! degrades the published quality label. Nothing here can stall, pace, or
//! feed back into the tick loop — the engine-side chaos test
//! (`epoch_observation_does_not_change_the_tick_stream`) pins the property.
//!
//! ## PTP (`ptp` feature)
//!
//! With the `ptp` feature and a configured `[timing] ptp_phc` device the task
//! also owns a [`PhcSampler`](multiview_engine::ptp::phc::PhcSampler): each
//! cycle takes one PHC reading (a cheap fd `clock_gettime`) and publishes the
//! tracker's `ReferenceStatus` into the wait-free handle the epoch sampler
//! reads — PTP then outranks the system clock while disciplined (ADR-T012).
//! Like the rest of the `phc` module this leg is compile-verified only (no
//! PTP NIC exists in CI); the selection logic itself is fully tested over
//! injected statuses.

use std::sync::Arc;
use std::time::Duration;

use multiview_control::EngineStateSnapshot;
use multiview_engine::epoch::{
    EpochAnchor, EpochSampler, EpochSamplerConfig, EpochStatus, SystemWallSampler, WallClockSampler,
};
use multiview_engine::sysref::NtpQuery;
use multiview_engine::{EnginePublisher, StopSignal};
use multiview_events::{Event, TimingStatus};
use multiview_output::SharedEpoch;

/// The publication cadence: 1 Hz — the ADR-M010 "low cadence, conflated
/// latest-wins" envelope (the epoch is an affine map that stays valid when
/// stale; consumers free-run between updates).
pub const SAMPLE_PERIOD: Duration = Duration::from_secs(1);

/// The shared run-anchor slot: the run path stores an
/// [`EpochAnchor`] (tick-0 seed + the run's monotonic source) when the output
/// clock seeds; the timing task picks it up lazily with a lock-free load.
pub type EpochAnchorSlot = Arc<arc_swap::ArcSwapOption<EpochAnchor>>;

/// An empty anchor slot (no run seeded yet).
#[must_use]
pub fn anchor_slot() -> EpochAnchorSlot {
    Arc::new(arc_swap::ArcSwapOption::empty())
}

/// The per-deployment knobs the spawned task publishes with.
#[derive(Debug, Clone)]
pub struct TimingStatusOptions {
    /// The program/output stream id the epoch maps (the legacy single-program
    /// daemon publishes the reserved `"main"`).
    pub stream_id: String,
    /// The fixed receiver-side link offset (ns) from `[timing]`
    /// (`link_offset_ms`; AES67 semantics applied to video — uniformity over
    /// smallness).
    pub link_offset_ns: i64,
    /// The optional PHC device path from `[timing] ptp_phc`, sampled only by
    /// a `ptp`-feature build. The binary's startup gate
    /// ([`crate::timing_gate`]) rejects a configured PHC in a non-`ptp` build
    /// before any run starts; the non-`ptp` task body additionally warns and
    /// rides the system clock as defence-in-depth for library callers.
    pub ptp_phc: Option<String>,
    /// The PTP-leg TAI−UTC timescale conversion in integer nanoseconds, from
    /// `[timing] ptp_utc_offset_s` (`TimingConfig::ptp_utc_offset_ns()`):
    /// subtracted from the PHC-derived estimate so the published epoch is
    /// always UTC (the PHC under standard linuxptp carries TAI). See the
    /// *Timescale* section of [`multiview_engine::epoch`].
    pub ptp_utc_offset_ns: i64,
}

impl TimingStatusOptions {
    /// The sampler tuning the spawned task publishes with: the epoch-path
    /// defaults (the conservative never-assume-locked system classifier, the
    /// standard re-anchor policy) with this deployment's configured PTP
    /// timescale conversion applied.
    #[must_use]
    pub fn epoch_sampler_config(&self) -> EpochSamplerConfig {
        EpochSamplerConfig {
            ptp_utc_offset_ns: self.ptp_utc_offset_ns,
            ..EpochSamplerConfig::new_default()
        }
    }
}

/// Derive one epoch sample, publish it as `timing.status`, and mirror the
/// epoch into the HLS-PDT cell. Returns the published snapshot.
///
/// Factored out of the task loop so the full publication contract is testable
/// with injected wall/NTP seams and a test publisher.
pub fn publish_once<W: WallClockSampler, Q: NtpQuery>(
    sampler: &mut EpochSampler<W, Q>,
    publisher: &EnginePublisher<EngineStateSnapshot, Event>,
    hls_epoch: &SharedEpoch,
    stream_id: &str,
    link_offset_ns: i64,
) -> EpochStatus {
    let status = sampler.sample_once();
    // One anchor, every surface: the HLS segmenter reads this cell at each
    // segment close (off the hot path), so PDT and the WS epoch agree. A
    // STEPPED re-anchor (the sampler's gross-discontinuity case) goes through
    // the stepped seam — it bumps the cell's generation so the HLS driver
    // marks the next closed segment `EXT-X-DISCONTINUITY` (wall-clock-sync
    // §3); anchor/hold/slew keep the generation (one continuous map).
    match status.update {
        multiview_engine::epoch::EpochUpdate::Stepped { .. } => {
            hls_epoch.set_stepped(status.epoch);
        }
        _ => hls_epoch.set(status.epoch),
    }
    // Non-blocking drop-oldest publish (invariant #10): a slow/absent WS
    // client lags; the publish itself can never wait.
    let _seq = publisher.publish_event(Event::TimingStatus(TimingStatus {
        stream_id: stream_id.to_owned(),
        epoch: status.epoch,
        link_offset_ns,
        clock_source: status.source,
        clock_quality: status.quality,
        // Sync-group skew measurement is DEV-C3; until then the honest value
        // is "none measured", never a fabricated tier.
        groups: vec![],
    }));
    status
}

/// Spawn the ~1 Hz timing-status task: waits (lazily, lock-free) for the run
/// to publish its [`EpochAnchor`], then derives + publishes the epoch each
/// period until `stop` is raised.
///
/// The task self-stops within one period of the [`StopSignal`] being raised.
pub fn spawn(
    publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    anchor: EpochAnchorSlot,
    hls_epoch: SharedEpoch,
    options: TimingStatusOptions,
    stop: StopSignal,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        run(&publisher, &anchor, &hls_epoch, &options, &stop).await;
    })
}

/// The task body: anchor lazily, then sample → publish on the fixed cadence.
async fn run(
    publisher: &EnginePublisher<EngineStateSnapshot, Event>,
    anchor: &EpochAnchorSlot,
    hls_epoch: &SharedEpoch,
    options: &TimingStatusOptions,
    stop: &StopSignal,
) {
    let mut ptp = PtpLeg::open(options.ptp_phc.as_deref());
    let mut sampler: Option<EpochSampler<SystemWallSampler, SysNtpQuery>> = None;
    let mut ticker = tokio::time::interval(SAMPLE_PERIOD);
    loop {
        if stop.is_stopped() {
            return;
        }
        ticker.tick().await;
        if stop.is_stopped() {
            return;
        }
        // Lazily bind to the run's anchor once the clock has seeded. A run
        // that never seeds (startup failure) publishes nothing — honest.
        if sampler.is_none() {
            if let Some(a) = anchor.load_full() {
                let mut s = EpochSampler::new(
                    a.seed_nanos(),
                    SystemWallSampler::new(a.time()),
                    SysNtpQuery::new(),
                    options.epoch_sampler_config(),
                );
                if let Some(handle) = ptp.status_handle() {
                    s = s.with_ptp(handle);
                }
                sampler = Some(s);
            }
        }
        let Some(s) = sampler.as_mut() else {
            continue;
        };
        // Drive the PTP leg first (one cheap PHC read per period under the
        // `ptp` feature; a no-op otherwise) so the selection sees fresh state.
        ptp.sample(anchor);
        let _status = publish_once(
            s,
            publisher,
            hls_epoch,
            &options.stream_id,
            options.link_offset_ns,
        );
    }
}

/// The live kernel NTP-discipline query under the `ntp` feature; the honest
/// "unavailable" fallback otherwise (the tracker then reports its configured
/// assumed state — the same contract `multiview-cli::wallclock` documents).
#[cfg(feature = "ntp")]
type SysNtpQuery = multiview_engine::sysref::live::SystemNtpQuery;

/// See the `ntp`-feature alias: without the feature no kernel read exists, so
/// the query is honestly "unavailable" and the system tracker falls back to
/// its configured assumed state.
#[cfg(not(feature = "ntp"))]
#[derive(Debug, Default)]
struct SysNtpQuery;

#[cfg(not(feature = "ntp"))]
impl SysNtpQuery {
    const fn new() -> Self {
        Self
    }
}

#[cfg(not(feature = "ntp"))]
impl NtpQuery for SysNtpQuery {
    fn read(&mut self) -> Option<multiview_engine::sysref::NtpReading> {
        None
    }
}

/// The PTP sampling leg: a real PHC sampler under the `ptp` feature, a
/// documented no-op otherwise.
#[cfg(feature = "ptp")]
struct PtpLeg {
    sampler:
        Option<multiview_engine::ptp::phc::PhcSampler<multiview_engine::ptp::phc::RealPhcSource>>,
}

#[cfg(feature = "ptp")]
impl PtpLeg {
    /// Open the configured PHC device, logging (not failing) when it cannot
    /// be opened — the epoch then rides the system-clock leg honestly.
    fn open(path: Option<&str>) -> Self {
        let sampler = path.and_then(|p| {
            match multiview_engine::ptp::phc::RealPhcSource::open(p) {
                Ok(source) => Some(multiview_engine::ptp::phc::PhcSampler::new(
                    source,
                    multiview_engine::ptp::ReferenceConfig::default(),
                )),
                Err(e) => {
                    tracing::warn!(device = %p, error = %e,
                        "timing: cannot open the PTP hardware clock; the epoch rides the system clock");
                    None
                }
            }
        });
        Self { sampler }
    }

    /// The wait-free status handle the epoch sampler reads, when a PHC is open.
    fn status_handle(
        &self,
    ) -> Option<multiview_engine::LatestState<multiview_engine::ptp::ReferenceStatus>> {
        self.sampler
            .as_ref()
            .map(multiview_engine::ptp::phc::PhcSampler::status_handle)
    }

    /// Take one PHC reading at the run's monotonic "now" (staleness/holdover
    /// fire through the tracker when the read fails).
    fn sample(&mut self, anchor: &EpochAnchorSlot) {
        if let (Some(sampler), Some(a)) = (self.sampler.as_mut(), anchor.load_full()) {
            let now_ns = a.time().now_nanos();
            let _status = sampler.sample_once(now_ns);
        }
    }
}

/// Without the `ptp` feature no PHC can be sampled. The `multiview` binary
/// never reaches this with a configured `ptp_phc` — the startup gate
/// ([`crate::timing_gate`]) fails the run first — so the warn-and-ride-the-
/// system-clock path below is defence-in-depth for library callers only.
#[cfg(not(feature = "ptp"))]
struct PtpLeg;

#[cfg(not(feature = "ptp"))]
impl PtpLeg {
    fn open(path: Option<&str>) -> Self {
        if let Some(p) = path {
            tracing::warn!(device = %p,
                "timing: ptp_phc is configured but this build lacks the `ptp` feature; \
                 the epoch rides the system clock");
        }
        Self
    }

    // Signature parity with the `ptp`-feature variant: the task body calls
    // `ptp.status_handle()` / `ptp.sample(..)` uniformly across both builds,
    // so these must stay methods even though the stub carries no state.
    #[allow(clippy::unused_self)]
    fn status_handle(
        &self,
    ) -> Option<multiview_engine::LatestState<multiview_engine::ptp::ReferenceStatus>> {
        None
    }

    // See `status_handle`: cfg-variant signature parity.
    #[allow(clippy::unused_self)]
    fn sample(&mut self, _anchor: &EpochAnchorSlot) {}
}
