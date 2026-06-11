//! Northbound alarm notifiers: webhook and email.
//!
//! When the engine's monitoring/alarm subsystem raises, updates, clears, or
//! acknowledges an alarm (broadcast-multiviewer brief §4: "northbound SNMP
//! traps, syslog, email, webhook"), the control plane fans the transition out to
//! configured northbound destinations. This module owns that fan-out.
//!
//! ## Pure builders, sends behind config
//!
//! Per the build plan the notifiers are **pure builders**: turning an
//! [`AlarmTransition`](multiview_events::AlarmTransition) plus a destination
//! configuration into a fully-formed request value (a [`webhook::WebhookRequest`]
//! or an [`email::EmailMessage`]) is a total, side-effect-free function that is
//! exhaustively unit-testable with no network. The *actual* send is a thin,
//! separately-driven step:
//!
//! * the **webhook** send rides the crate's existing HTTP stack and is always
//!   available;
//! * the **email** send uses `lettre` and lives behind the off-by-default
//!   `email` feature, so the default build pulls no SMTP/native dependency and
//!   stays cargo-deny-clean.
//!
//! ## Per-severity routing
//!
//! A [`SeverityRouter`] maps each X.733 [`PerceivedSeverity`] to the set of
//! destinations that should be notified, so an operator can (for example) page
//! on `Critical`/`Major` over email while logging everything to a webhook. The
//! router is a pure predicate over the alarm record; it never performs I/O.
use multiview_core::alarm::{AlarmRecord, PerceivedSeverity};

pub mod email;
pub mod webhook;

/// Which lifecycle transition of an alarm a notification reports.
///
/// Mirrors the four alarm [`Event`](multiview_events::Event) variants the engine
/// publishes (`alarm.raised` / `alarm.updated` / `alarm.cleared` /
/// `alarm.acked`) so a notifier can render a transition-appropriate subject line
/// without re-deriving it from the record alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AlarmTransitionKind {
    /// The alarm was first raised.
    Raised,
    /// An active alarm's value changed (e.g. severity escalated).
    Updated,
    /// The alarm's underlying condition cleared.
    Cleared,
    /// An operator acknowledged the alarm.
    Acked,
}

impl AlarmTransitionKind {
    /// A short, stable machine-readable label (`raised` / `updated` /
    /// `cleared` / `acked`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Raised => "raised",
            Self::Updated => "updated",
            Self::Cleared => "cleared",
            Self::Acked => "acked",
        }
    }
}

/// A configured northbound destination a notification can be routed to.
///
/// Serialised **tagged** (`#[serde(tag = "kind")]`) per repo conventions; never
/// `untagged`. `#[non_exhaustive]` so additional destination kinds (syslog,
/// SNMP) can be added without a breaking change.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Destination {
    /// An HTTP webhook endpoint.
    Webhook {
        /// The absolute URL the alarm JSON is posted to.
        url: String,
        /// An optional shared-secret bearer token sent as `Authorization`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
    },
    /// An email recipient.
    Email {
        /// The destination mailbox (e.g. `noc@example.com`).
        to: String,
    },
}

/// The minimum severity at which a routing rule fires.
///
/// A rule with `min_severity = Major` fires for `Major` and `Critical` alarm
/// transitions but not for `Minor`/`Warning`. Because [`PerceivedSeverity`] has
/// the X.733 total order, "at least this severe" is a simple `>=` comparison.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RoutingRule {
    /// The lowest severity that triggers this rule.
    pub min_severity: PerceivedSeverity,
    /// The destination notified when the rule fires.
    pub destination: Destination,
}

/// A pure per-severity router: which destinations an alarm record routes to.
///
/// The router holds an ordered list of [`RoutingRule`]s. [`SeverityRouter::route`]
/// returns every destination whose `min_severity` is satisfied by the record's
/// severity, in rule order. It performs **no I/O** — it is a pure predicate, so
/// the routing policy is exhaustively unit-testable.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SeverityRouter {
    /// The routing rules, evaluated in order.
    pub rules: Vec<RoutingRule>,
}

impl SeverityRouter {
    /// A router with no rules (routes nothing).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a rule routing alarms of at least `min_severity` to `destination`.
    #[must_use]
    pub fn with_rule(mut self, min_severity: PerceivedSeverity, destination: Destination) -> Self {
        self.rules.push(RoutingRule {
            min_severity,
            destination,
        });
        self
    }

    /// The destinations `record` routes to, in rule order.
    ///
    /// A destination is returned once per matching rule (duplicate rules yield
    /// duplicate destinations — the caller decides whether to dedupe). An alarm
    /// whose severity is below every rule's threshold routes nowhere.
    #[must_use]
    pub fn route(&self, record: &AlarmRecord) -> Vec<&Destination> {
        self.rules
            .iter()
            .filter(|rule| record.severity >= rule.min_severity)
            .map(|rule| &rule.destination)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use multiview_core::alarm::{AlarmId, AlarmKind, AlarmRecord, AlarmScope, PerceivedSeverity};
    use multiview_core::time::MediaTime;

    use super::{Destination, SeverityRouter};

    fn record(severity: PerceivedSeverity) -> AlarmRecord {
        AlarmRecord::new(
            AlarmId::new("a1"),
            AlarmKind::Black,
            severity,
            AlarmScope::Tile { index: 0 },
            MediaTime::from_nanos(100),
        )
    }

    fn webhook(url: &str) -> Destination {
        Destination::Webhook {
            url: url.to_owned(),
            token: None,
        }
    }

    #[test]
    fn router_includes_destinations_at_or_above_threshold() {
        let router = SeverityRouter::new()
            .with_rule(PerceivedSeverity::Warning, webhook("https://log"))
            .with_rule(PerceivedSeverity::Major, webhook("https://page"));

        // Critical is >= both thresholds: both destinations fire, in rule order.
        let matched = router.route(&record(PerceivedSeverity::Critical));
        assert_eq!(matched.len(), 2);
        assert_eq!(matched[0], &webhook("https://log"));
        assert_eq!(matched[1], &webhook("https://page"));
    }

    #[test]
    fn router_excludes_destinations_below_threshold() {
        let router = SeverityRouter::new()
            .with_rule(PerceivedSeverity::Warning, webhook("https://log"))
            .with_rule(PerceivedSeverity::Major, webhook("https://page"));

        // Minor is >= Warning but < Major: only the log destination fires.
        let matched = router.route(&record(PerceivedSeverity::Minor));
        assert_eq!(matched, vec![&webhook("https://log")]);
    }

    #[test]
    fn router_routes_nothing_below_every_threshold() {
        let router =
            SeverityRouter::new().with_rule(PerceivedSeverity::Major, webhook("https://page"));
        assert!(router.route(&record(PerceivedSeverity::Warning)).is_empty());
        // An empty router routes nothing regardless of severity.
        assert!(SeverityRouter::new()
            .route(&record(PerceivedSeverity::Critical))
            .is_empty());
    }
}
