//! Placement / migration / split counters for the GPU work-placement loop
//! (ADR-0018 §consequences — "every adaptation logged").
//!
//! The off-hot-path placement controller in `multiview-engine` proposes a
//! [`PlacementProposal`](../../multiview_engine/placement/enum.PlacementProposal.html)
//! each control tick; this module owns the **counter model** that records the
//! distribution of those proposals so an operator can see how often the loop
//! holds, sheds, migrates, or splits — and how often the anti-storm damps
//! suppress a migration.
//!
//! Like the rest of this crate it owns only the *model* (the series names + the
//! bounded label scheme) and hands back lock-free [`Counter`] handles; the
//! caller (the controller) does the `increment()`. It keeps **no** dependency on
//! the engine's `PlacementProposal` type (telemetry stays a leaf), so the
//! controller maps its proposal to the typed `record_*` helpers here.
//!
//! Counters are monotonic and lock-free on the update path, so recording an
//! adaptation never back-pressures the controller (which itself never
//! back-pressures the engine — invariant #10).

use crate::metrics::{Counter, Labels, MetricsRegistry};

/// Metric series names. Public so a Prometheus exporter / test can reference
/// them without re-typing the strings.
pub mod names {
    /// Total placement decisions, labelled by `outcome`
    /// (`hold`/`shed`/`migrate`/`split`).
    pub const DECISIONS: &str = "multiview_placement_decisions_total";
    /// Total make-before-break migrations the controller proposed.
    pub const MIGRATIONS: &str = "multiview_placement_migrations_total";
    /// Total migrations suppressed by the anti-storm damps, labelled by `reason`
    /// (`pinned`/`no_better_home`/`anti_storm`).
    pub const MIGRATIONS_SUPPRESSED: &str = "multiview_placement_migrations_suppressed_total";
    /// Total deliberate last-resort multi-GPU splits the controller proposed.
    pub const SPLITS: &str = "multiview_placement_splits_total";
}

/// The kind of relief a shed/suppression carried (the bounded `reason`/`outcome`
/// label values), mirroring the engine's `ShedReason` without depending on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SuppressReason {
    /// The pipeline is pinned to its (overloaded) device, so it cannot migrate.
    Pinned,
    /// No materially-better home exists (the imbalance can't be cured by moving).
    NoBetterHome,
    /// A better home exists but cooldown / per-GPU budget forbid moving now.
    AntiStorm,
}

impl SuppressReason {
    /// The bounded, stable lower-case label for this reason.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            SuppressReason::Pinned => "pinned",
            SuppressReason::NoBetterHome => "no_better_home",
            SuppressReason::AntiStorm => "anti_storm",
        }
    }
}

/// The registered placement counter handles (ADR-0018).
///
/// One set per process (placement is whole-system); the `outcome`/`reason`
/// labels are bounded to a fixed small vocabulary, so cardinality is bounded.
#[derive(Debug, Clone)]
pub struct PlacementCounters {
    decision_hold: Counter,
    decision_shed: Counter,
    decision_migrate: Counter,
    decision_split: Counter,
    migrations: Counter,
    suppressed_pinned: Counter,
    suppressed_no_better_home: Counter,
    suppressed_anti_storm: Counter,
    splits: Counter,
}

impl PlacementCounters {
    /// Register the placement counter series against `registry`.
    ///
    /// Re-registering the same `(name, labels)` returns the existing handle, so
    /// this is idempotent.
    #[must_use]
    pub fn register(registry: &MetricsRegistry) -> Self {
        let outcome = |value: &str| Labels::new().with("outcome", value);
        let reason = |value: &str| Labels::new().with("reason", value);
        Self {
            decision_hold: registry.counter(names::DECISIONS, outcome("hold")),
            decision_shed: registry.counter(names::DECISIONS, outcome("shed")),
            decision_migrate: registry.counter(names::DECISIONS, outcome("migrate")),
            decision_split: registry.counter(names::DECISIONS, outcome("split")),
            migrations: registry.counter(names::MIGRATIONS, Labels::empty()),
            suppressed_pinned: registry.counter(names::MIGRATIONS_SUPPRESSED, reason("pinned")),
            suppressed_no_better_home: registry
                .counter(names::MIGRATIONS_SUPPRESSED, reason("no_better_home")),
            suppressed_anti_storm: registry
                .counter(names::MIGRATIONS_SUPPRESSED, reason("anti_storm")),
            splits: registry.counter(names::SPLITS, Labels::empty()),
        }
    }

    /// Record a `Hold` decision (no sustained overload this tick).
    pub fn record_hold(&self) {
        self.decision_hold.increment(1);
    }

    /// Record a `Shed` decision with the reason it was chosen over a migration.
    ///
    /// Increments both the `outcome="shed"` decision counter and the matching
    /// `migrations_suppressed{reason}` counter (a shed always means a migration
    /// was *not* taken, for one of the three damped reasons).
    pub fn record_shed(&self, reason: SuppressReason) {
        self.decision_shed.increment(1);
        match reason {
            SuppressReason::Pinned => self.suppressed_pinned.increment(1),
            SuppressReason::NoBetterHome => self.suppressed_no_better_home.increment(1),
            SuppressReason::AntiStorm => self.suppressed_anti_storm.increment(1),
        }
    }

    /// Record a make-before-break `Migrate` decision.
    pub fn record_migrate(&self) {
        self.decision_migrate.increment(1);
        self.migrations.increment(1);
    }

    /// Record a deliberate last-resort `Split` decision.
    pub fn record_split(&self) {
        self.decision_split.increment(1);
        self.splits.increment(1);
    }

    /// The total migrations recorded (test/telemetry convenience).
    #[must_use]
    pub fn migrations_total(&self) -> u64 {
        self.migrations.get()
    }

    /// The total splits recorded (test/telemetry convenience).
    #[must_use]
    pub fn splits_total(&self) -> u64 {
        self.splits.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> MetricsRegistry {
        MetricsRegistry::new()
    }

    #[test]
    fn registers_the_placement_series_set() {
        let reg = registry();
        let _ = PlacementCounters::register(&reg);
        let names: Vec<String> = reg.series().into_iter().map(|s| s.name).collect();
        for expected in [
            names::DECISIONS,
            names::MIGRATIONS,
            names::MIGRATIONS_SUPPRESSED,
            names::SPLITS,
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "must register {expected}"
            );
        }
    }

    #[test]
    fn migrate_increments_both_decision_and_migration_totals() {
        let reg = registry();
        let counters = PlacementCounters::register(&reg);
        counters.record_migrate();
        counters.record_migrate();
        assert_eq!(counters.migrations_total(), 2);
        let decisions = reg.counter(names::DECISIONS, Labels::new().with("outcome", "migrate"));
        assert_eq!(decisions.get(), 2);
    }

    #[test]
    fn shed_increments_the_matching_suppress_reason() {
        let reg = registry();
        let counters = PlacementCounters::register(&reg);
        counters.record_shed(SuppressReason::Pinned);
        counters.record_shed(SuppressReason::AntiStorm);
        counters.record_shed(SuppressReason::AntiStorm);

        let pinned = reg.counter(
            names::MIGRATIONS_SUPPRESSED,
            Labels::new().with("reason", "pinned"),
        );
        let anti = reg.counter(
            names::MIGRATIONS_SUPPRESSED,
            Labels::new().with("reason", "anti_storm"),
        );
        let no_home = reg.counter(
            names::MIGRATIONS_SUPPRESSED,
            Labels::new().with("reason", "no_better_home"),
        );
        assert_eq!(pinned.get(), 1);
        assert_eq!(anti.get(), 2);
        assert_eq!(no_home.get(), 0, "an untouched reason stays at zero");
        // The shed decision counter counts every shed regardless of reason.
        let shed = reg.counter(names::DECISIONS, Labels::new().with("outcome", "shed"));
        assert_eq!(shed.get(), 3);
    }

    #[test]
    fn split_increments_both_decision_and_split_totals() {
        let reg = registry();
        let counters = PlacementCounters::register(&reg);
        counters.record_split();
        assert_eq!(counters.splits_total(), 1);
        let decisions = reg.counter(names::DECISIONS, Labels::new().with("outcome", "split"));
        assert_eq!(decisions.get(), 1);
    }

    #[test]
    fn suppress_reason_labels_are_stable_and_bounded() {
        assert_eq!(SuppressReason::Pinned.label(), "pinned");
        assert_eq!(SuppressReason::NoBetterHome.label(), "no_better_home");
        assert_eq!(SuppressReason::AntiStorm.label(), "anti_storm");
    }
}
