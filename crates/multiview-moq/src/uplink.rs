//! Publisher-side uplink budgeting, separate from every viewer's downlink control.
//!
//! The publisher measures *its own* publishing connection (the QUIC counters of the session
//! that carries the shared ladder upstream) and admits how much of the admitted ladder it is
//! willing to serve. It is deliberately not fed by per-viewer selection: a viewer's own ABR
//! decision changes what that viewer subscribes to, never what the publisher produces.
//!
//! Accounting is network-only. The active set's cost is the sum of the active renditions'
//! video bitrates, plus the independent audio bitrate, plus an explicit overhead/reserve.
//! Encoder CPU/GPU/session cost is a different quantity in different units and is admitted by
//! the encoder owner's own typed budget; it is never summed into a bitrate here.
//!
//! Evidence is explicit. A transport that reports nothing, reports counters that moved
//! backwards, or leaves a gap longer than the staleness bound is [`UplinkHealth::Unknown`]:
//! missing loss is never read as zero loss, and nothing is inferred from an absent estimate.
//! Unknown-capacity upshifts remain disabled until real delivery feedback is available.
//! Send counters and congestion-controller estimates cannot prove receiver delivery.

use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};

/// Raw counters of the publishing connection, straight from its transport.
///
/// Counter semantics, verified against the pinned transport's own documentation
/// (`web-transport-trait` 0.4 / `moq-net` 0.3 `session::Stats`): `bytes_sent` counts bytes
/// *sent*, including retransmissions and overhead, and `bytes_lost` counts loss detected by
/// retransmission or acknowledgement. There is **no acknowledgement/drain counter** in this
/// transport, so no field here may be read as "the peer acknowledged these bytes". A probe
/// therefore judges what was *offered* against loss and RTT, never against an invented ack.
///
/// Every counter is optional because the transport may simply not report it; `None` means
/// "not reported", never zero, so a missing counter cannot be mistaken for a healthy one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QuicSample {
    /// Total bytes sent, including retransmissions and overhead.
    pub bytes_sent: Option<u64>,
    /// Total bytes lost.
    pub bytes_lost: Option<u64>,
    pub packets_sent: Option<u64>,
    pub packets_lost: Option<u64>,
    /// The congestion controller's send estimate, when the transport reports one.
    pub estimated_send_rate_bps: Option<u64>,
    pub rtt_micros: Option<u64>,
}

impl QuicSample {
    /// Adapt the pinned transport's own statistics, preserving "not reported" as `None`.
    pub fn from_session_stats(stats: moq_net::session::Stats) -> Self {
        Self {
            bytes_sent: stats.bytes_sent,
            bytes_lost: stats.bytes_lost,
            packets_sent: stats.packets_sent,
            packets_lost: stats.packets_lost,
            estimated_send_rate_bps: stats.estimated_send_rate.map(|rate| rate.as_bps()),
            rtt_micros: stats
                .rtt
                .map(|rtt| u64::try_from(rtt.as_micros()).unwrap_or(u64::MAX)),
        }
    }

    /// Whether this transport reports the byte counters an interval ratio needs.
    pub fn counters_reported(&self) -> bool {
        self.bytes_sent.is_some() && self.bytes_lost.is_some()
    }
}

/// What one interval's counters actually evidence. There is no implicit zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UplinkEvidence {
    /// Both counters were reported and did not regress: the ratio describes this interval.
    /// `sent_bytes` is the transport's *send* counter delta (retransmissions and overhead
    /// included) — it is not an acknowledgement and is never treated as drained data.
    Reported {
        loss_ppm: u32,
        sent_bytes: u64,
        lost_bytes: u64,
    },
    /// The transport reported no counters for this interval.
    Unreported,
    /// Counters moved backwards (reconnect/reset): no ratio is meaningful.
    Reset,
    /// The interval is longer than the staleness bound, so deltas span a gap.
    Stale,
    /// No baseline yet: the first reported sample only establishes one.
    Unbaselined,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceGap {
    Unreported,
    Reset,
    Stale,
    Unbaselined,
}

/// The controller's explicit view of the publishing connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UplinkHealth {
    /// No usable evidence this interval: nothing is inferred, nothing is raised.
    Unknown(EvidenceGap),
    Healthy {
        loss_ppm: u32,
    },
    Losing {
        loss_ppm: u32,
    },
}

/// One interval observation: the media the publisher actually wrote, over how long, plus the
/// connection counters at the end of the interval.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UplinkReading {
    pub published_bytes: u64,
    pub elapsed: Duration,
    pub counters: QuicSample,
}

impl UplinkReading {
    /// A reading whose transport reported nothing at all.
    pub fn unreported(published_bytes: u64, elapsed: Duration) -> Self {
        Self {
            published_bytes,
            elapsed,
            counters: QuicSample::default(),
        }
    }

    pub fn published_bps(&self) -> Option<u64> {
        let micros = u64::try_from(self.elapsed.as_micros()).ok()?;
        if micros == 0 {
            return None;
        }
        Some(
            self.published_bytes
                .saturating_mul(8)
                .saturating_mul(1_000_000)
                / micros,
        )
    }
}

/// Classify one interval against the previous baseline.
pub fn classify(
    baseline: Option<QuicSample>,
    reading: &UplinkReading,
    stale_after: Duration,
) -> UplinkEvidence {
    if reading.elapsed > stale_after {
        return UplinkEvidence::Stale;
    }
    let counters = reading.counters;
    let (Some(sent), Some(lost)) = (counters.bytes_sent, counters.bytes_lost) else {
        return UplinkEvidence::Unreported;
    };
    let Some(previous) = baseline else {
        return UplinkEvidence::Unbaselined;
    };
    let (Some(previous_sent), Some(previous_lost)) = (previous.bytes_sent, previous.bytes_lost)
    else {
        return UplinkEvidence::Unbaselined;
    };
    if sent < previous_sent || lost < previous_lost {
        return UplinkEvidence::Reset;
    }
    let sent_bytes = sent - previous_sent;
    let lost_bytes = lost - previous_lost;
    // The transport's counters are absolute; only the interval delta is meaningful.
    let loss_ppm = match lost_bytes.saturating_mul(1_000_000).checked_div(sent_bytes) {
        Some(ratio) => u32::try_from(ratio.min(1_000_000)).unwrap_or(1_000_000),
        None if lost_bytes == 0 => 0,
        None => 1_000_000,
    };
    UplinkEvidence::Reported {
        loss_ppm,
        sent_bytes,
        lost_bytes,
    }
}

/// Folds the publishing connection's own statistics and the media bytes the publisher wrote
/// into the interval readings the controller consumes, so production wiring is one call per
/// sampling interval rather than a caller-assembled estimate.
#[derive(Debug, Default)]
pub struct UplinkMeter {
    published_bytes: u64,
    readings: u64,
}

impl UplinkMeter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Count media bytes published since the last reading.
    pub fn record_published(&mut self, bytes: u64) {
        self.published_bytes = self.published_bytes.saturating_add(bytes);
    }

    /// Take one reading from the session's statistics over `elapsed`, consuming the bytes
    /// recorded since the previous reading. Unreported fields stay unreported.
    pub fn reading(&mut self, stats: moq_net::session::Stats, elapsed: Duration) -> UplinkReading {
        self.readings += 1;
        UplinkReading {
            published_bytes: std::mem::take(&mut self.published_bytes),
            elapsed,
            counters: QuicSample::from_session_stats(stats),
        }
    }

    pub fn readings(&self) -> u64 {
        self.readings
    }
}

/// Cost knobs. Every one of them changes a decision, not just documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UplinkConfig {
    /// Consecutive over-capacity observations before an explicit downshift.
    pub downshift_after_over: u32,
    /// Loss above this ratio (parts per million) forces an immediate explicit downshift.
    pub loss_limit_ppm: u32,
    /// Offload margin a probe must clear: the probed active-set cost is multiplied by this
    /// (parts per thousand, 1200 = 1.2x) and the publisher must actually push that rate
    /// before the rung is admitted. With a fixed ladder that is normally unreachable, which
    /// is why the unknown-capacity path is off by default.
    pub upshift_margin_permille: u32,
    /// Healthy RTT observations required before the rolling baseline may veto a probe. An
    /// unvalidated baseline never vetoes.
    pub rtt_baseline_samples: u32,
    /// Consecutive draining observations before a probed rung is admitted.
    pub probe_observations: u32,
    /// Observations a probe may run before it is abandoned without a raise.
    pub probe_timeout_observations: u32,
    /// Observations to wait after an abandoned probe before trying again.
    pub probe_cooldown_observations: u32,
    /// An *unverified* congestion-controller estimate is discounted to this fraction (parts
    /// per thousand) before it may gate a probe. It never justifies a raise.
    pub probe_gate_permille: u32,
    /// Observations after a level change before another probe may start.
    pub settle_observations: u32,
    /// An interval longer than this carries no usable continuous-window evidence.
    pub stale_after: Duration,
    /// During a probe, a reported RTT above this fraction (parts per thousand) of the healthy
    /// baseline reads as queue growth and fails the probe.
    pub rtt_growth_limit_permille: u32,
    /// Explicit network overhead/reserve applied on top of the media bitrates (parts per
    /// thousand), covering transport framing and headroom. Not an encoder cost.
    pub overhead_permille: u32,
}

impl Default for UplinkConfig {
    fn default() -> Self {
        Self {
            downshift_after_over: 2,
            loss_limit_ppm: 10_000,
            upshift_margin_permille: 1_200,
            rtt_baseline_samples: 3,
            probe_observations: 4,
            probe_timeout_observations: 12,
            probe_cooldown_observations: 8,
            probe_gate_permille: 800,
            settle_observations: 4,
            stale_after: Duration::from_secs(3),
            rtt_growth_limit_permille: 1_500,
            overhead_permille: 100,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetState {
    /// Capacity was never stated: only the lowest rung is admitted.
    Unknown,
    /// Capacity was stated by configuration, so the granted prefix is arithmetic.
    Known,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReduceReason {
    /// The publishing connection is losing data.
    CongestionLoss,
    /// The granted prefix sits below the currently served level.
    CapacityExceeded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Directive {
    /// No change.
    Hold,
    /// Reserved for a future receiver-confirmed probe; never emitted by this controller.
    Probe {
        level: usize,
    },
    /// The probe did not survive its window: stop offering it and stay where we are.
    AbandonProbe {
        level: usize,
    },
    Raise {
        from: usize,
        to: usize,
    },
    /// An explicit, reported reduction. Never a silent one.
    Reduce {
        from: usize,
        to: usize,
        reason: ReduceReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalReason {
    /// The publisher's uplink budget does not currently fund this level.
    AboveAdmittedLevel,
    /// Not even the lowest level is funded once audio and overhead are accounted.
    Unfunded,
    UnknownRendition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Granted {
        level: usize,
    },
    Refused {
        requested: usize,
        admitted: usize,
        reason: RefusalReason,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetError {
    EmptyLadder,
    LevelsNotAscending { at: usize },
    ZeroCost { at: usize },
}

impl fmt::Display for BudgetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyLadder => write!(f, "uplink budget needs at least one level"),
            Self::LevelsNotAscending { at } => {
                write!(
                    f,
                    "uplink budget level {at} does not cost more than its predecessor"
                )
            }
            Self::ZeroCost { at } => write!(f, "uplink budget level {at} costs nothing"),
        }
    }
}

impl std::error::Error for BudgetError {}

#[derive(Debug, Clone, Copy)]
struct Probe {
    level: usize,
}

/// Publisher uplink controller. One instance per publication, never one per viewer.
#[derive(Debug)]
pub struct UplinkBudget {
    config: UplinkConfig,
    capacity_bps: Option<u64>,
    level_costs: Vec<u64>,
    reserved_audio_bps: u64,
    level: usize,
    over: u32,
    settle: u32,
    measured_bps: u64,
    health: UplinkHealth,
    last_loss_ppm: Option<u32>,
    baseline: Option<QuicSample>,
    rtt_baseline_micros: Option<u64>,
    rtt_samples: u32,
    probe: Option<Probe>,
    observed: u64,
    changes: u64,
}

impl UplinkBudget {
    /// `level_costs_bps` are the admitted video bitrates per rung; `reserved_audio_bps` is the
    /// independent audio bitrate. Network overhead is applied by
    /// [`UplinkConfig::overhead_permille`]. No encoder cost belongs in either argument.
    pub fn new(
        capacity_bps: Option<u64>,
        level_costs_bps: Vec<u64>,
        reserved_audio_bps: u64,
        config: UplinkConfig,
    ) -> Result<Self, BudgetError> {
        if level_costs_bps.is_empty() {
            return Err(BudgetError::EmptyLadder);
        }
        for (at, cost) in level_costs_bps.iter().enumerate() {
            if *cost == 0 {
                return Err(BudgetError::ZeroCost { at });
            }
            if at > 0 && *cost <= level_costs_bps[at - 1] {
                return Err(BudgetError::LevelsNotAscending { at });
            }
        }
        let mut budget = Self {
            config,
            capacity_bps,
            level_costs: level_costs_bps,
            reserved_audio_bps,
            level: 0,
            over: 0,
            settle: 0,
            measured_bps: 0,
            health: UplinkHealth::Unknown(EvidenceGap::Unbaselined),
            last_loss_ppm: None,
            baseline: None,
            rtt_baseline_micros: None,
            rtt_samples: 0,
            probe: None,
            observed: 0,
            changes: 0,
        };
        if let Some(capacity) = capacity_bps {
            budget.level = budget.granted_level(capacity).unwrap_or(0);
        }
        Ok(budget)
    }

    /// Build a controller from an admitted ladder: rungs are the ladder's admitted video
    /// costs and the reservation is the ladder's independent audio bitrate.
    pub fn from_ladder(
        ladder: &crate::ladder::SharedLadder,
        capacity_bps: Option<u64>,
        config: UplinkConfig,
    ) -> Result<Self, BudgetError> {
        Self::new(
            capacity_bps,
            ladder.level_costs_bps(),
            ladder.reserved_audio_bps(),
            config,
        )
    }

    pub fn config(&self) -> &UplinkConfig {
        &self.config
    }
    pub fn state(&self) -> BudgetState {
        match self.capacity_bps {
            Some(_) => BudgetState::Known,
            None => BudgetState::Unknown,
        }
    }
    pub fn capacity_bps(&self) -> Option<u64> {
        self.capacity_bps
    }
    pub fn reserved_audio_bps(&self) -> u64 {
        self.reserved_audio_bps
    }
    pub fn level(&self) -> usize {
        self.level
    }
    pub fn levels(&self) -> usize {
        self.level_costs.len()
    }
    pub fn measured_bps(&self) -> u64 {
        self.measured_bps
    }
    /// The last loss ratio the transport actually reported; `None` when it reported none.
    pub fn last_loss_ppm(&self) -> Option<u32> {
        self.last_loss_ppm
    }
    pub fn health(&self) -> UplinkHealth {
        self.health
    }
    pub fn probing(&self) -> Option<usize> {
        self.probe.map(|probe| probe.level)
    }
    /// The rolling healthy-RTT baseline, once healthy samples have advanced it.
    pub fn rtt_baseline_micros(&self) -> Option<u64> {
        self.rtt_baseline_micros
    }

    pub fn changes(&self) -> u64 {
        self.changes
    }
    pub fn observations(&self) -> u64 {
        self.observed
    }

    /// The total network cost of serving rungs `0..=level`: the sum of their video bitrates,
    /// plus the independent audio bitrate, plus the explicit overhead/reserve.
    pub fn active_cost_bps(&self, level: usize) -> Option<u64> {
        let mut media = self.reserved_audio_bps;
        for cost in self.level_costs.iter().take(level + 1) {
            media = media.checked_add(*cost)?;
        }
        let scale = 1_000u64.checked_add(u64::from(self.config.overhead_permille))?;
        media
            .checked_mul(scale)
            .map(|scaled| scaled.div_ceil(1_000))
    }

    /// The highest prefix whose total network cost fits `budget_bps`. `None` when even the
    /// lowest rung does not fit — the publisher still produces, it just cannot afford uplink.
    pub fn granted_level(&self, budget_bps: u64) -> Option<usize> {
        (0..self.level_costs.len()).rev().find(|level| {
            self.active_cost_bps(*level)
                .is_some_and(|cost| cost <= budget_bps)
        })
    }

    /// Whether any transport feedback has ever described an interval. Until it has, the
    /// controller is not adaptive: it holds the lowest rung and reports no evidence.
    pub fn feedback_received(&self) -> bool {
        self.last_loss_ppm.is_some()
    }

    /// Whether the stated capacity funds even the lowest rung.
    pub fn fully_funded(&self) -> bool {
        match self.capacity_bps {
            Some(capacity) => self.granted_level(capacity).is_some(),
            None => true,
        }
    }

    /// Rungs the publisher currently recommends consumers avoid. Their tracks stay published
    /// and available: this is the explicit signal, not a reduction of production output.
    pub fn stalled_levels(&self, ladder_len: usize) -> Vec<usize> {
        (self.level + 1..ladder_len).collect()
    }

    /// The rate a probe for `level` would have to push: the rung's active network cost times
    /// [`UplinkConfig::upshift_margin_permille`]. With a fixed ladder the publisher emits only
    /// its own cost, so this is normally unreachable — which is exactly why an unknown budget
    /// stays non-adaptive instead of inventing headroom.
    pub fn required_probe_bps(&self, level: usize) -> Option<u64> {
        self.active_cost_bps(level)?
            .checked_mul(u64::from(self.config.upshift_margin_permille))
            .map(|scaled| scaled.div_ceil(1_000))
    }

    /// Admit a *new* subscriber at a requested level. Existing peers are untouched; the
    /// caller keeps serving them regardless of the answer.
    pub fn admit(&self, requested: usize) -> Admission {
        if requested >= self.level_costs.len() {
            return Admission::Refused {
                requested,
                admitted: self.level,
                reason: RefusalReason::UnknownRendition,
            };
        }
        if !self.fully_funded() {
            return Admission::Refused {
                requested,
                admitted: self.level,
                reason: RefusalReason::Unfunded,
            };
        }
        if requested > self.level {
            return Admission::Refused {
                requested,
                admitted: self.level,
                reason: RefusalReason::AboveAdmittedLevel,
            };
        }
        Admission::Granted { level: requested }
    }

    /// State an operator/configuration capacity. `None` returns the controller to the
    /// conservative unknown state without raising anything and abandons any probe.
    pub fn set_capacity(&mut self, capacity_bps: Option<u64>) -> Directive {
        self.capacity_bps = capacity_bps;
        self.over = 0;
        let abandoned = self.probe.take().map(|probe| probe.level);
        let target = capacity_bps
            .and_then(|capacity| self.granted_level(capacity))
            .unwrap_or(0);
        if target < self.level {
            return self.reduce(target, ReduceReason::CapacityExceeded);
        }
        if target > self.level {
            return self.raise(target);
        }
        abandoned
            .map(|level| Directive::AbandonProbe { level })
            .unwrap_or(Directive::Hold)
    }

    /// Fold one interval observation into the controller and report what changed.
    pub fn observe(&mut self, reading: UplinkReading) -> Directive {
        self.observed += 1;
        self.settle = self.settle.saturating_sub(1);
        if let Some(published) = reading.published_bps() {
            self.measured_bps = if self.observed == 1 {
                published
            } else {
                (self
                    .measured_bps
                    .saturating_mul(3)
                    .saturating_add(published))
                    / 4
            };
        }

        let evidence = classify(self.baseline, &reading, self.config.stale_after);
        self.health = match evidence {
            UplinkEvidence::Reported { loss_ppm, .. } if loss_ppm > self.config.loss_limit_ppm => {
                self.last_loss_ppm = Some(loss_ppm);
                UplinkHealth::Losing { loss_ppm }
            }
            UplinkEvidence::Reported { loss_ppm, .. } => {
                self.last_loss_ppm = Some(loss_ppm);
                UplinkHealth::Healthy { loss_ppm }
            }
            UplinkEvidence::Unreported => {
                self.baseline = None;
                UplinkHealth::Unknown(EvidenceGap::Unreported)
            }
            UplinkEvidence::Stale => {
                self.baseline = None;
                UplinkHealth::Unknown(EvidenceGap::Stale)
            }
            UplinkEvidence::Reset => UplinkHealth::Unknown(EvidenceGap::Reset),
            UplinkEvidence::Unbaselined => UplinkHealth::Unknown(EvidenceGap::Unbaselined),
        };
        if matches!(
            evidence,
            UplinkEvidence::Reported { .. } | UplinkEvidence::Reset | UplinkEvidence::Unbaselined
        ) {
            self.baseline = Some(reading.counters);
        }

        // Loss is acted on immediately: waiting would deepen an already-lost window.
        if let UplinkHealth::Losing { .. } = self.health {
            self.over = 0;
            let abandoned = self.probe.take().map(|probe| probe.level);
            if self.level > 0 {
                return self.reduce(self.level - 1, ReduceReason::CongestionLoss);
            }
            return abandoned
                .map(|level| Directive::AbandonProbe { level })
                .unwrap_or(Directive::Hold);
        }

        // Nothing usable was reported: abandon any probe and hold. Missing loss is unknown,
        // a reset is not congestion, and nothing here may justify a raise.
        if matches!(self.health, UplinkHealth::Unknown(_)) {
            self.over = 0;
            return self
                .probe
                .take()
                .map(|probe| Directive::AbandonProbe { level: probe.level })
                .unwrap_or(Directive::Hold);
        }

        // A rolling, validated healthy-RTT baseline: never "the first sample forever". The
        // current sample is judged against the baseline *before* it is folded in, and the
        // baseline only advances on healthy observations outside a probe, so a probe is
        // judged against the state production actually settled into.
        if let UplinkHealth::Healthy { .. } = self.health
            && self.probe.is_none()
            && let Some(rtt) = reading.counters.rtt_micros
        {
            self.rtt_baseline_micros = Some(match self.rtt_baseline_micros {
                Some(baseline) => (baseline.saturating_mul(3).saturating_add(rtt)) / 4,
                None => rtt,
            });
            self.rtt_samples = self.rtt_samples.saturating_add(1);
        }

        // A stated capacity is authoritative arithmetic; no probe is needed.
        if let Some(capacity) = self.capacity_bps {
            let target = self.granted_level(capacity).unwrap_or(0);
            if target < self.level {
                self.over += 1;
                if self.over >= self.config.downshift_after_over {
                    return self.reduce(target, ReduceReason::CapacityExceeded);
                }
                return Directive::Hold;
            }
            self.over = 0;
            if target > self.level {
                return self.raise(target);
            }
            return Directive::Hold;
        }

        // The pinned transport exposes send counters, not acknowledgement or drain
        // feedback. Unknown capacity therefore cannot authorize an upshift. Do not
        // infer delivery from bytes_sent, estimates, or a stable RTT.
        Directive::Hold
    }

    fn raise(&mut self, to: usize) -> Directive {
        self.apply_change(to, ReduceReason::CapacityExceeded)
    }

    fn reduce(&mut self, to: usize, reason: ReduceReason) -> Directive {
        self.apply_change(to, reason)
    }

    fn apply_change(&mut self, to: usize, reason: ReduceReason) -> Directive {
        let from = self.level;
        let target = to.min(self.level_costs.len() - 1);
        if target == from {
            return Directive::Hold;
        }
        self.level = target;
        self.over = 0;
        self.settle = self.config.settle_observations;
        self.changes += 1;
        if target < from {
            Directive::Reduce {
                from,
                to: target,
                reason,
            }
        } else {
            Directive::Raise { from, to: target }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerReject {
    /// The peer bound is reached: retention stays bounded and the refusal is explicit.
    Capacity {
        max_peers: usize,
    },
    UnknownRendition(usize),
    /// This peer already holds a lease. Ownership is unique; the caller switches or drops it.
    Duplicate {
        level: usize,
    },
    /// The lease is no longer the registered owner (released and re-subscribed): it may not
    /// mutate registry state.
    Stale,
    /// The publisher's uplink budget refused the requested level.
    Refused {
        requested: usize,
        admitted: usize,
        reason: RefusalReason,
    },
}

/// Bounded record of which peer selected which ladder level.
///
/// Ownership is unique per peer: a peer holds at most one lease, each lease carries the epoch
/// it was granted under, and a lease whose epoch no longer matches the registry owner cannot
/// release or move that slot. Subscriptions are tracked as explicit subscribe/unsubscribe
/// events: a rendition change is a fresh subscribe on the new track *at a verified RAP* plus
/// an unsubscribe from the old one. The publisher never sends a SUBSCRIBE_UPDATE.
#[derive(Clone, Debug)]
pub struct PeerRegistry {
    inner: Arc<Mutex<Registry>>,
    ladder_len: usize,
    max_peers: usize,
}

#[derive(Default, Debug)]
struct Registry {
    active: BTreeMap<u64, Owned>,
    counts: Vec<u64>,
    next_epoch: u64,
}

#[derive(Debug, Clone, Copy)]
struct Owned {
    level: usize,
    epoch: u64,
}

/// A live peer subscription. Dropping it releases only the slot it was granted.
#[derive(Debug)]
pub struct PeerLease {
    peer: u64,
    level: usize,
    epoch: u64,
    registry: PeerRegistry,
    active: bool,
}

impl PeerRegistry {
    pub fn new(ladder_len: usize, max_peers: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Registry {
                active: BTreeMap::new(),
                counts: vec![0; ladder_len],
                next_epoch: 1,
            })),
            ladder_len,
            max_peers: max_peers.max(1),
        }
    }

    pub fn ladder_len(&self) -> usize {
        self.ladder_len
    }
    pub fn max_peers(&self) -> usize {
        self.max_peers
    }

    /// Register a subscribe. A peer that already holds a lease is refused: ownership is unique,
    /// so a late lease can never release or move the current owner's slot.
    pub fn subscribe(&self, peer: u64, level: usize) -> Result<PeerLease, PeerReject> {
        if level >= self.ladder_len {
            return Err(PeerReject::UnknownRendition(level));
        }
        let mut registry = self.inner.lock().expect("peer registry");
        if let Some(owned) = registry.active.get(&peer) {
            return Err(PeerReject::Duplicate { level: owned.level });
        }
        if registry.active.len() >= self.max_peers {
            return Err(PeerReject::Capacity {
                max_peers: self.max_peers,
            });
        }
        let epoch = registry.next_epoch;
        registry.next_epoch = registry.next_epoch.saturating_add(1);
        registry.active.insert(peer, Owned { level, epoch });
        registry.counts[level] += 1;
        drop(registry);
        Ok(PeerLease {
            peer,
            level,
            epoch,
            registry: self.clone(),
            active: true,
        })
    }

    /// Admit-and-subscribe in one step, using the publisher's uplink budget for the decision
    /// and the peer bound for retention.
    pub fn subscribe_if_admitted(
        &self,
        budget: &UplinkBudget,
        peer: u64,
        requested: usize,
    ) -> Result<PeerLease, PeerReject> {
        match budget.admit(requested) {
            Admission::Granted { level } => self.subscribe(peer, level),
            Admission::Refused {
                requested,
                admitted,
                reason,
            } => Err(PeerReject::Refused {
                requested,
                admitted,
                reason,
            }),
        }
    }

    /// Release whatever lease currently owns `peer`. Epoch-safe: only the current owner's
    /// slot is removed, counts are saturating, and a stale caller cannot corrupt the registry.
    pub fn unsubscribe(&self, peer: u64) -> bool {
        let mut registry = self.inner.lock().expect("peer registry");
        let Some(owned) = registry.active.remove(&peer) else {
            return false;
        };
        registry.counts[owned.level] = registry.counts[owned.level].saturating_sub(1);
        true
    }

    fn release(&self, peer: u64, epoch: u64) -> bool {
        let mut registry = self.inner.lock().expect("peer registry");
        match registry.active.get(&peer) {
            Some(owned) if owned.epoch == epoch => {
                let level = owned.level;
                registry.active.remove(&peer);
                registry.counts[level] = registry.counts[level].saturating_sub(1);
                true
            }
            _ => false,
        }
    }

    pub fn total(&self) -> usize {
        self.inner.lock().expect("peer registry").active.len()
    }

    /// Live peer count per ladder level.
    pub fn counts(&self) -> Vec<u64> {
        self.inner.lock().expect("peer registry").counts.clone()
    }

    pub fn level_of(&self, peer: u64) -> Option<usize> {
        self.inner
            .lock()
            .expect("peer registry")
            .active
            .get(&peer)
            .map(|owned| owned.level)
    }
}

impl PeerLease {
    pub fn peer(&self) -> u64 {
        self.peer
    }
    pub fn level(&self) -> usize {
        self.level
    }

    /// Switch rendition: unsubscribe the old level, subscribe the new. Served from the new
    /// track's next RAP-started group; no SUBSCRIBE_UPDATE is ever sent.
    pub fn switch(&mut self, level: usize) -> Result<(), PeerReject> {
        if level >= self.registry.ladder_len {
            return Err(PeerReject::UnknownRendition(level));
        }
        let mut registry = self.registry.inner.lock().expect("peer registry");
        let previous = match registry.active.get(&self.peer) {
            Some(owned) if owned.epoch == self.epoch => Some(owned.level),
            _ => None,
        };
        let Some(previous) = previous else {
            return Err(PeerReject::Stale);
        };
        registry.counts[previous] = registry.counts[previous].saturating_sub(1);
        registry.counts[level] = registry.counts[level].saturating_add(1);
        if let Some(owned) = registry.active.get_mut(&self.peer) {
            owned.level = level;
        }
        drop(registry);
        self.level = level;
        Ok(())
    }
}

impl Drop for PeerLease {
    fn drop(&mut self) {
        if self.active {
            self.registry.release(self.peer, self.epoch);
            self.active = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_capacity_orders_levels_and_reserves_audio() {
        let config = UplinkConfig {
            overhead_permille: 0,
            ..UplinkConfig::default()
        };
        let budget = UplinkBudget::new(
            Some(2_500_000),
            vec![800_000, 2_000_000, 6_000_000],
            128_000,
            config,
        )
        .unwrap();
        assert_eq!(budget.state(), BudgetState::Known);
        assert_eq!(budget.active_cost_bps(0), Some(928_000));
        assert_eq!(budget.active_cost_bps(1), Some(2_928_000));
        assert_eq!(budget.level(), 0, "the sum of active rungs is what fits");
        assert!(budget.admit(0) == Admission::Granted { level: 0 });
        assert_eq!(
            budget.admit(1),
            Admission::Refused {
                requested: 1,
                admitted: 0,
                reason: RefusalReason::AboveAdmittedLevel
            }
        );
        let tiny = UplinkBudget::new(Some(500_000), vec![800_000], 128_000, config).unwrap();
        assert!(!tiny.fully_funded());
        assert_eq!(
            tiny.admit(0),
            Admission::Refused {
                requested: 0,
                admitted: 0,
                reason: RefusalReason::Unfunded
            }
        );
    }

    #[test]
    fn sender_counters_cannot_authorize_unknown_capacity_upshift() {
        let mut budget = UplinkBudget::new(
            None,
            vec![800_000, 2_000_000],
            128_000,
            UplinkConfig::default(),
        )
        .unwrap();
        for interval in 0..100 {
            let directive = budget.observe(UplinkReading {
                published_bytes: 10_000_000,
                elapsed: Duration::from_secs(1),
                counters: QuicSample {
                    bytes_sent: Some(interval * 10_000_000),
                    bytes_lost: Some(0),
                    estimated_send_rate_bps: Some(100_000_000),
                    rtt_micros: Some(1_000),
                    ..QuicSample::default()
                },
            });
            assert_eq!(directive, Directive::Hold);
            assert_eq!(budget.level(), 0);
            assert_eq!(budget.probing(), None);
        }
    }

    #[test]
    fn rejects_unordered_or_empty_levels() {
        assert_eq!(
            UplinkBudget::new(None, vec![], 0, UplinkConfig::default()).unwrap_err(),
            BudgetError::EmptyLadder
        );
        assert_eq!(
            UplinkBudget::new(None, vec![0], 0, UplinkConfig::default()).unwrap_err(),
            BudgetError::ZeroCost { at: 0 }
        );
        assert_eq!(
            UplinkBudget::new(None, vec![5, 5], 0, UplinkConfig::default()).unwrap_err(),
            BudgetError::LevelsNotAscending { at: 1 }
        );
    }

    #[test]
    fn evidence_is_explicit_about_missing_reset_and_stale_counters() {
        let stale_after = Duration::from_secs(3);
        let clean = QuicSample {
            bytes_sent: Some(1_000),
            bytes_lost: Some(10),
            ..QuicSample::default()
        };
        let later = QuicSample {
            bytes_sent: Some(11_000),
            bytes_lost: Some(110),
            ..QuicSample::default()
        };
        let reading = UplinkReading {
            published_bytes: 10_000,
            elapsed: Duration::from_secs(1),
            counters: later,
        };
        assert_eq!(
            classify(Some(clean), &reading, stale_after),
            UplinkEvidence::Reported {
                loss_ppm: 10_000,
                sent_bytes: 10_000,
                lost_bytes: 100
            }
        );
        // Counters move backwards: a reset, not a loss ratio.
        assert_eq!(
            classify(
                Some(later),
                &UplinkReading {
                    counters: clean,
                    ..reading
                },
                stale_after
            ),
            UplinkEvidence::Reset
        );
        // Unreported loss is never zero loss.
        assert_eq!(
            classify(
                Some(clean),
                &UplinkReading {
                    counters: QuicSample {
                        bytes_sent: Some(11_000),
                        ..QuicSample::default()
                    },
                    ..reading
                },
                stale_after
            ),
            UplinkEvidence::Unreported
        );
        // A long gap carries no continuous-window evidence.
        assert_eq!(
            classify(
                Some(clean),
                &UplinkReading {
                    elapsed: Duration::from_secs(30),
                    ..reading
                },
                stale_after
            ),
            UplinkEvidence::Stale
        );
    }

    #[test]
    fn lease_ownership_is_unique_and_stale_leases_cannot_mutate_the_registry() {
        let registry = PeerRegistry::new(3, 2);
        let mut first = registry.subscribe(1, 0).unwrap();
        assert_eq!(
            registry.subscribe(1, 2).unwrap_err(),
            PeerReject::Duplicate { level: 0 }
        );
        assert_eq!(registry.counts(), vec![1, 0, 0]);

        // The operator releases peer 1, the peer re-subscribes at a new level: the old lease
        // is stale and must not move or release the new owner's slot.
        assert!(registry.unsubscribe(1));
        let mut replacement = registry.subscribe(1, 2).unwrap();
        assert_eq!(registry.counts(), vec![0, 0, 1]);
        assert_eq!(first.switch(1).unwrap_err(), PeerReject::Stale);
        assert_eq!(replacement.switch(1).unwrap(), ());
        assert_eq!(registry.counts(), vec![0, 1, 0]);
        assert_eq!(registry.level_of(1), Some(1));

        // Dropping the stale lease must leave the live owner untouched, and counts never
        // underflow past zero.
        drop(first);
        assert_eq!(registry.counts(), vec![0, 1, 0]);
        assert_eq!(registry.level_of(1), Some(1));
        let _ = replacement.switch(1);
        drop(replacement);
        assert_eq!(registry.counts(), vec![0, 0, 0]);
        assert_eq!(registry.total(), 0);
    }
}
