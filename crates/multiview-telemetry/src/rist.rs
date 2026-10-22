//! RIST link-statistics telemetry surface (ADR-0095 Tier-1 / RIST-5).
//!
//! `FFmpeg`'s `rist://` protocol registers **no** stats callback and exposes **no**
//! stats `AVOption` (verified against `libavformat/librist.c` and `ffmpeg -h
//! protocol=rist`), so a RIST link's health — retransmits, RTT, link quality,
//! bandwidth, lost/recovered packets — is invisible through the Tier-0 transport.
//! RIST-5 obtains those numbers via a direct-librist FFI leaf
//! (`multiview-rist-sys`, which owns the librist `rist_stats_callback_set`) and
//! feeds them here as a neutral [`RistLinkSample`].
//!
//! This module owns only the **model**: the Prometheus series for one RIST link
//! and a pure loss assessment. It is dependency-free (no FFI, no wire-event type)
//! exactly like [`crate::gpu`] — the producer (the RIST sink / cli poller) reads
//! the FFI sample, calls [`RistLinkGauges::update`], and maps the same sample to
//! the `multiview_events::RistLinkStats` wire event. The stats path is off the
//! data plane and best-effort: it never blocks the engine (inv #10).
//!
//! **Cumulative-as-gauge.** librist reports **cumulative totals** since the link
//! opened (not per-interval deltas), so the counter-style series (retransmitted /
//! lost / recovered / sent) are stored in **gauges** set to the absolute total.
//! Re-polling the same total is therefore idempotent — a Prometheus `counter`
//! that we `increment()`-ed per poll would double-count. Rates are derived
//! downstream by differencing successive scrapes (`rate()` works on a gauge that
//! only ever increases just as well as on a counter for this purpose, and a link
//! re-open that resets the total is a visible step the dashboard can detect).

use crate::metrics::{Gauge, Labels, MetricsRegistry};

/// Metric series names (RIST-5). Public so a Prometheus exporter or a test can
/// reference them without re-typing the strings.
pub mod names {
    /// Current round-trip time to the peer (milliseconds), as a gauge.
    pub const RIST_LINK_RTT_MS: &str = "multiview_rist_link_rtt_milliseconds";
    /// librist link-quality metric (`0..=100`; 100 = no loss needing recovery).
    pub const RIST_LINK_QUALITY: &str = "multiview_rist_link_quality_ratio";
    /// Average measured throughput (bits per second), as a gauge.
    pub const RIST_LINK_BANDWIDTH_BPS: &str = "multiview_rist_link_bandwidth_bits_per_second";
    /// Throughput devoted to ARQ retransmissions (bits per second), as a gauge.
    pub const RIST_LINK_RETRY_BANDWIDTH_BPS: &str =
        "multiview_rist_link_retry_bandwidth_bits_per_second";
    /// The active peer count on this link (`1` single-link; `>1` only bonded).
    pub const RIST_LINK_PEERS: &str = "multiview_rist_link_peers";
    /// Cumulative packets sent on this link.
    pub const RIST_LINK_SENT: &str = "multiview_rist_link_sent_packets_total";
    /// Cumulative packets received on this link (receiver role).
    pub const RIST_LINK_RECEIVED: &str = "multiview_rist_link_received_packets_total";
    /// Cumulative packets retransmitted (the ARQ recovery traffic).
    pub const RIST_LINK_RETRANSMITTED: &str = "multiview_rist_link_retransmitted_packets_total";
    /// Cumulative packets lost (unrecoverable within the buffer window).
    pub const RIST_LINK_LOST: &str = "multiview_rist_link_lost_packets_total";
    /// Cumulative packets recovered by the ARQ machinery.
    pub const RIST_LINK_RECOVERED: &str = "multiview_rist_link_recovered_packets_total";
}

/// The quality floor (`0..=100`) below which a link is considered to be losing
/// packets faster than ARQ comfortably recovers; sustained dips raise the warning.
const QUALITY_FLOOR: f64 = 80.0;

/// How many *consecutive* below-floor samples raise the loss warning. One dip is
/// expected on a real link (bad-inputs-are-the-purpose) and self-clears; a
/// sustained run is the operator-actionable signal. Hysteresis prevents flapping.
const SUSTAINED_SAMPLES: u32 = 2;

/// Which end of a RIST link a [`RistLinkSample`] describes.
///
/// Mirrors `multiview_events::RistLinkRole` but is defined here so telemetry
/// stays a leaf (no wire-event dependency); the producer maps between the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RistLinkRole {
    /// Egress: librist `rist_stats_sender_peer` (we push the flow to a peer).
    Sender,
    /// Ingress: librist `rist_stats_receiver_flow` (we receive the flow).
    Receiver,
}

impl RistLinkRole {
    /// The stable lower-case label used in the metric `role` label + warnings.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Sender => "sender",
            Self::Receiver => "receiver",
        }
    }
}

/// A neutral RIST link-stats sample (one librist `rist_stats` callback delivery),
/// decoded by `multiview-rist-sys` and fed to [`RistLinkGauges::update`].
///
/// All counters are **cumulative since the link opened**. Fields the reporting
/// role does not populate are `0` (e.g. `received` is `0` for a sender link).
#[derive(Debug, Clone, PartialEq)]
pub struct RistLinkSample {
    /// The configured RIST source/output id this link belongs to.
    pub link_id: String,
    /// Which end of the link reported these stats.
    pub role: RistLinkRole,
    /// The RIST flow id (set by the sender).
    pub flow_id: u32,
    /// The peer canonical name(s) librist reported.
    pub cname: String,
    /// Number of active peers on this link.
    pub peer_count: u32,
    /// Current round-trip time to the peer (milliseconds).
    pub rtt_ms: u32,
    /// librist link-quality metric (`0..=100`).
    pub quality: f64,
    /// Average measured throughput (bits per second).
    pub bandwidth_bps: u64,
    /// Throughput devoted to ARQ retransmissions (bits per second).
    pub retry_bandwidth_bps: u64,
    /// Cumulative packets sent.
    pub sent: u64,
    /// Cumulative packets received (receiver role; `0` for sender).
    pub received: u64,
    /// Cumulative packets retransmitted.
    pub retransmitted: u64,
    /// Cumulative packets lost (unrecoverable).
    pub lost: u64,
    /// Cumulative packets recovered by ARQ.
    pub recovered: u64,
    /// When this link first reported stats (engine monotonic nanoseconds).
    pub since: i64,
}

/// The outcome of folding one sample into a link's running health assessment.
///
/// Carries whether the sustained-loss warning is currently active for the link
/// and, when active, the operator-facing message + remediation the producer
/// lifts into a `multiview_events::HealthWarning` (`WarningCode::RistLinkLoss`).
/// The producer raises/clears on the [`loss_warning_active`](Self::loss_warning_active)
/// edge, coalescing on the link id.
#[derive(Debug, Clone, PartialEq)]
pub struct RistLinkAssessment {
    active: bool,
    message: String,
    remediation: String,
}

impl RistLinkAssessment {
    /// Whether the sustained-loss warning is active for this link right now.
    #[must_use]
    pub fn loss_warning_active(&self) -> bool {
        self.active
    }

    /// The human-readable condition message (names the link + the quality).
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The concrete remediation an operator can act on.
    #[must_use]
    pub fn remediation(&self) -> &str {
        &self.remediation
    }
}

/// The registered gauge handles + running health state for one RIST link.
///
/// Cheap to clone where needed (the handles are `Arc`); however the hysteresis
/// counter is per-instance, so a single owner (the link's poller) should hold one
/// `RistLinkGauges` and call [`update`](Self::update) on each librist callback.
#[derive(Debug, Clone)]
pub struct RistLinkGauges {
    rtt_ms: Gauge,
    quality: Gauge,
    bandwidth_bps: Gauge,
    retry_bandwidth_bps: Gauge,
    peers: Gauge,
    sent: Gauge,
    received: Gauge,
    retransmitted: Gauge,
    lost: Gauge,
    recovered: Gauge,
    /// Consecutive below-floor samples seen (hysteresis state).
    below_floor_streak: u32,
}

impl RistLinkGauges {
    /// Register the RIST link series against `registry`, keyed by a bounded
    /// `{link, role}` label set. Re-registering the same `(name, labels)` returns
    /// the existing handle, so calling this twice for a link is idempotent.
    #[must_use]
    pub fn register(
        registry: &MetricsRegistry,
        link_id: impl Into<String>,
        role: RistLinkRole,
    ) -> Self {
        let l = Labels::new()
            .with("link", link_id.into())
            .with("role", role.label());
        Self {
            rtt_ms: registry.gauge(names::RIST_LINK_RTT_MS, l.clone()),
            quality: registry.gauge(names::RIST_LINK_QUALITY, l.clone()),
            bandwidth_bps: registry.gauge(names::RIST_LINK_BANDWIDTH_BPS, l.clone()),
            retry_bandwidth_bps: registry.gauge(names::RIST_LINK_RETRY_BANDWIDTH_BPS, l.clone()),
            peers: registry.gauge(names::RIST_LINK_PEERS, l.clone()),
            sent: registry.gauge(names::RIST_LINK_SENT, l.clone()),
            received: registry.gauge(names::RIST_LINK_RECEIVED, l.clone()),
            retransmitted: registry.gauge(names::RIST_LINK_RETRANSMITTED, l.clone()),
            lost: registry.gauge(names::RIST_LINK_LOST, l.clone()),
            recovered: registry.gauge(names::RIST_LINK_RECOVERED, l),
            below_floor_streak: 0,
        }
    }

    /// Fold one librist stats sample into the registered series and the running
    /// health assessment.
    ///
    /// The cumulative counters are **set** to the sample's absolute total (not
    /// incremented), so a re-poll of an unchanged total is idempotent. Returns
    /// the [`RistLinkAssessment`] the producer raises/clears the loss warning on.
    pub fn update(&mut self, sample: &RistLinkSample) -> RistLinkAssessment {
        self.rtt_ms.set(f64::from(sample.rtt_ms));
        self.quality.set(sample.quality);
        self.bandwidth_bps.set(bits_to_f64(sample.bandwidth_bps));
        self.retry_bandwidth_bps
            .set(bits_to_f64(sample.retry_bandwidth_bps));
        self.peers.set(f64::from(sample.peer_count));
        self.sent.set(bits_to_f64(sample.sent));
        self.received.set(bits_to_f64(sample.received));
        self.retransmitted.set(bits_to_f64(sample.retransmitted));
        self.lost.set(bits_to_f64(sample.lost));
        self.recovered.set(bits_to_f64(sample.recovered));

        if sample.quality < QUALITY_FLOOR {
            self.below_floor_streak = self.below_floor_streak.saturating_add(1);
        } else {
            self.below_floor_streak = 0;
        }
        let active = self.below_floor_streak >= SUSTAINED_SAMPLES;
        let message = if active {
            format!(
                "RIST link `{}` ({}) is recovering below clean-link quality \
                 (quality {:.1}%, RTT {} ms, {} lost / {} recovered) — sustained \
                 packet loss is straining the ARQ recovery window.",
                sample.link_id,
                sample.role.label(),
                sample.quality,
                sample.rtt_ms,
                sample.lost,
                sample.recovered,
            )
        } else {
            String::new()
        };
        RistLinkAssessment {
            active,
            message,
            remediation: if active {
                "Investigate the network path (packet loss / RTT) between the \
                 RIST peers, or increase the link `buffer_ms` so the ARQ window \
                 covers the loss burst."
                    .to_owned()
            } else {
                String::new()
            },
        }
    }

    /// The RTT gauge handle.
    #[must_use]
    pub fn rtt_ms(&self) -> &Gauge {
        &self.rtt_ms
    }
    /// The link-quality gauge handle.
    #[must_use]
    pub fn quality(&self) -> &Gauge {
        &self.quality
    }
    /// The throughput gauge handle.
    #[must_use]
    pub fn bandwidth_bps(&self) -> &Gauge {
        &self.bandwidth_bps
    }
    /// The cumulative retransmitted-packets gauge handle.
    #[must_use]
    pub fn retransmitted(&self) -> &Gauge {
        &self.retransmitted
    }
    /// The cumulative lost-packets gauge handle.
    #[must_use]
    pub fn lost(&self) -> &Gauge {
        &self.lost
    }
    /// The cumulative recovered-packets gauge handle.
    #[must_use]
    pub fn recovered(&self) -> &Gauge {
        &self.recovered
    }
    /// The cumulative sent-packets gauge handle.
    #[must_use]
    pub fn sent(&self) -> &Gauge {
        &self.sent
    }
}

/// Convert a `u64` count/bitrate to the `f64` a gauge stores without a lossy
/// `as` cast (the workspace denies `as_conversions`). Reconstructed from two
/// 32-bit halves via the infallible [`f64::from`] `u32` impl, so the result is
/// **exact** for every value up to 2^53 — far above any realistic RIST count or
/// bitrate. Above 2^53 the low half loses precision (the `f64` mantissa limit),
/// which is irrelevant here and never panics.
fn bits_to_f64(value: u64) -> f64 {
    const SHIFT_32: f64 = 4_294_967_296.0; // 2^32, exact in f64
    let high = u32::try_from(value >> 32).unwrap_or(u32::MAX);
    let low = u32::try_from(value & 0xFFFF_FFFF).unwrap_or(0);
    f64::from(high).mul_add(SHIFT_32, f64::from(low))
}

#[cfg(test)]
mod tests {
    // Exact float comparison is intentional here: `bits_to_f64` is an *integer*
    // reconstruction, so its outputs are exact for the tested values.
    #![allow(clippy::float_cmp)]
    use super::*;

    #[test]
    fn bits_to_f64_is_exact_for_realistic_values() {
        assert_eq!(bits_to_f64(0), 0.0);
        assert_eq!(bits_to_f64(1_000), 1_000.0);
        assert_eq!(bits_to_f64(12_000_000), 12_000_000.0);
        assert_eq!(bits_to_f64(5_000_000_000), 5_000_000_000.0);
    }

    #[test]
    fn role_label_is_stable() {
        assert_eq!(RistLinkRole::Sender.label(), "sender");
        assert_eq!(RistLinkRole::Receiver.label(), "receiver");
    }
}
