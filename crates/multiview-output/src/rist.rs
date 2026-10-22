//! RIST link-statistics bridge (ADR-0095 Tier-1 / RIST-5).
//!
//! The glue that turns a direct-librist stats sample into the observable
//! surfaces: the [`multiview_telemetry`] metric series, the
//! [`multiview_events::Event::RistLinkStats`] wire event, and the
//! [`multiview_events::WarningCode::RistLinkLoss`] health warning. All raw
//! librist FFI lives in [`multiview_rist_sys`]; this module is pure safe Rust so
//! `multiview-output` stays `forbid(unsafe_code)`.
//!
//! ## What this delivers, and the honest boundary
//!
//! [`RistStatsBridge::ingest`] is the surfacing core: feed it a decoded
//! [`RistLinkSample`] (whatever its origin) and it updates the metrics + returns
//! the wire events to publish. [`RistSenderLink`] wires that to a **real**
//! direct-librist sender egress: `open()` a librist sender session, `write()` the
//! same encoded MPEG-TS packets every other push sink fans out (inv #7), and
//! `pump_stats()` to drain the librist stats callback into the bridge.
//!
//! `FFmpeg`'s Tier-0 `rist://` transport exposes **no** stats (it owns its
//! librist context privately), so this librist-direct sender is the *only* egress
//! path that observes link health. The direct-librist **receiver** (ingress) with
//! stats owns the receive+demux loop and is a larger, Tier-2-shaped change —
//! deliberately not built here; the sample model + the bridge already handle the
//! receiver role, so it is a clean follow-up. No stat is ever fabricated: the
//! bridge only surfaces samples a librist context we own produced.

use multiview_events::{
    Event, HealthWarning, RistLinkRole as WireRole, RistLinkStats, WarningCode, WarningSeverity,
};
use multiview_telemetry::metrics::MetricsRegistry;
use multiview_telemetry::rist::{RistLinkGauges, RistLinkRole, RistLinkSample};

#[cfg(feature = "rist-stats")]
use multiview_rist_sys::session::{RistSenderSession, SessionError};

/// Maps the telemetry-layer role to the wire-event role (the two are defined in
/// separate crates so each stays a leaf; this is the single conversion point).
fn wire_role(role: RistLinkRole) -> WireRole {
    match role {
        RistLinkRole::Sender => WireRole::Sender,
        RistLinkRole::Receiver => WireRole::Receiver,
    }
}

/// Lowers a decoded link sample into the conflated wire telemetry event.
fn to_wire_stats(sample: &RistLinkSample) -> RistLinkStats {
    RistLinkStats {
        link_id: sample.link_id.clone(),
        role: wire_role(sample.role),
        flow_id: sample.flow_id,
        cname: sample.cname.clone(),
        peer_count: sample.peer_count,
        rtt_ms: sample.rtt_ms,
        quality: sample.quality,
        bandwidth_bps: sample.bandwidth_bps,
        retry_bandwidth_bps: sample.retry_bandwidth_bps,
        sent: sample.sent,
        received: sample.received,
        retransmitted: sample.retransmitted,
        lost: sample.lost,
        recovered: sample.recovered,
        since: sample.since,
    }
}

/// Surfaces RIST link stats to telemetry + the realtime event stream.
///
/// One bridge per RIST link. [`ingest`](Self::ingest) folds a sample into the
/// metric series and returns the events to publish: always a `rist.link.stats`
/// sample, plus a `health.warning.raised`/`cleared` on the loss-warning **edge**
/// (raised once when loss becomes sustained, cleared once on recovery — never a
/// duplicate while already active, inv #10 conflation hygiene).
pub struct RistStatsBridge {
    gauges: RistLinkGauges,
    /// Whether the loss warning is currently raised (edge tracking).
    warning_active: bool,
    /// When the warning was first raised (engine ns), carried on the clear event.
    warning_since: i64,
}

impl RistStatsBridge {
    /// Create a bridge for `link_id`, registering the link's metric series
    /// against `registry`.
    #[must_use]
    pub fn new(registry: &MetricsRegistry, link_id: impl Into<String>, role: RistLinkRole) -> Self {
        Self {
            gauges: RistLinkGauges::register(registry, link_id, role),
            warning_active: false,
            warning_since: 0,
        }
    }

    /// Fold one decoded link sample into the metrics and return the wire events
    /// to publish (the stats sample, plus a warning raise/clear on the edge).
    #[must_use]
    pub fn ingest(&mut self, sample: &RistLinkSample) -> Vec<Event> {
        let assessment = self.gauges.update(sample);
        let mut events = Vec::with_capacity(2);
        events.push(Event::RistLinkStats(to_wire_stats(sample)));

        match (self.warning_active, assessment.loss_warning_active()) {
            (false, true) => {
                // Rising edge: raise the warning once.
                self.warning_active = true;
                self.warning_since = sample.since;
                events.push(Event::HealthWarningRaised(HealthWarning {
                    code: WarningCode::RistLinkLoss,
                    severity: WarningSeverity::Warning,
                    subsystem: format!("rist:{}", sample.link_id),
                    message: assessment.message().to_owned(),
                    remediation: assessment.remediation().to_owned(),
                    since: self.warning_since,
                    active: true,
                }));
            }
            (true, false) => {
                // Falling edge: clear the warning once.
                self.warning_active = false;
                events.push(Event::HealthWarningCleared(HealthWarning {
                    code: WarningCode::RistLinkLoss,
                    severity: WarningSeverity::Warning,
                    subsystem: format!("rist:{}", sample.link_id),
                    message: format!("RIST link `{}` recovered", sample.link_id),
                    remediation: String::new(),
                    since: self.warning_since,
                    active: false,
                }));
            }
            // No edge: just the stats sample.
            (false, false) | (true, true) => {}
        }
        events
    }
}

/// A live direct-librist **sender** egress link with statistics (the `rist-stats`
/// data path). Owns the librist session and the surfacing [`RistStatsBridge`].
///
/// `write()` fans the same encoded MPEG-TS packets as every other push sink
/// (inv #7); `pump_stats()` drains the librist stats callback into the bridge and
/// returns the events to publish — call it off the data plane on the stats
/// cadence. The librist context is runtime-loaded and never linked at build time.
#[cfg(feature = "rist-stats")]
pub struct RistSenderLink {
    session: RistSenderSession,
    bridge: RistStatsBridge,
}

#[cfg(feature = "rist-stats")]
impl RistSenderLink {
    /// Open a librist sender link to the already-lowered `rist://…` `url`,
    /// reporting stats every `interval_ms`, and register its metric series.
    ///
    /// `profile` is the librist `rist_profile` value (simple=0/main=1/advanced=2),
    /// matching the typed config's lowering.
    ///
    /// # Errors
    /// [`SessionError`] if the librist runtime cannot be loaded or the session
    /// cannot be opened (no peer, bad URL, missing symbol).
    pub fn open(
        registry: &MetricsRegistry,
        link_id: impl Into<String>,
        url: &str,
        profile: i32,
        interval_ms: i32,
    ) -> Result<Self, SessionError> {
        let link_id = link_id.into();
        let session = RistSenderSession::open(link_id.clone(), url, profile, interval_ms)?;
        let bridge = RistStatsBridge::new(registry, link_id, RistLinkRole::Sender);
        Ok(Self { session, bridge })
    }

    /// Write one encoded MPEG-TS payload to the RIST flow.
    ///
    /// # Errors
    /// [`SessionError`] if the librist write fails.
    pub fn write(&self, payload: &[u8]) -> Result<(), SessionError> {
        self.session.write(payload)
    }

    /// Drain any link-stats samples librist has reported and surface them,
    /// returning the wire events to publish (off the data plane, drop-oldest).
    #[must_use]
    pub fn pump_stats(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        for sample in self.session.drain_stats() {
            events.extend(self.bridge.ingest(&sample));
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_role_maps_both_directions() {
        assert_eq!(wire_role(RistLinkRole::Sender), WireRole::Sender);
        assert_eq!(wire_role(RistLinkRole::Receiver), WireRole::Receiver);
    }
}
