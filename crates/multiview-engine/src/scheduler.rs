//! Time- and event-triggered **automation scheduler**: a cron-like / interval
//! timeline plus on-alarm / on-cue triggers that produce salvo (and other
//! command) actions over an injected [`MediaTime`] (broadcast-multiviewer brief
//! §8, ADR-MV001).
//!
//! The scheduler answers one question per control tick: *given the current media
//! time and any events that occurred, which automation actions should fire now?*
//! It is the pure decision layer above the [`salvo`](crate::salvo) engine — it
//! decides *when* a named salvo (or other action) should be triggered; the salvo
//! engine decides *what* changes that produces.
//!
//! Two trigger families:
//!
//! * **Time triggers** — fire on a fixed **interval** measured from a base
//!   instant (a robust, drift-free stand-in for cron on a media timeline: "every
//!   N nanoseconds"). Edge-detected against the injected clock so each due
//!   instant fires exactly once even when control ticks are coarse or bursty.
//! * **Event triggers** — fire when a named event is reported this tick
//!   (on-alarm, on-cue, on-GPI).
//!
//! ## Isolation (invariant #1 + #10)
//!
//! [`Scheduler::tick`] is a pure function of `(now, events)` that **returns** the
//! [`ScheduledAction`]s to apply; it never reaches into the engine, blocks, or
//! `.await`s. The engine polls it on its slow control tick and applies any
//! returned actions at a frame boundary — a stalled scheduler tick simply defers
//! the decision, never a frame.
use multiview_core::time::MediaTime;

/// What event class an event trigger (registered via
/// [`Scheduler::on_event`]) listens for.
///
/// `#[non_exhaustive]` so future event sources can be added without a breaking
/// change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EventKind {
    /// An alarm raised/cleared transition.
    Alarm,
    /// An SCTE-35 / programme cue.
    Cue,
    /// A GPI edge.
    Gpi,
}

/// An event reported to the scheduler this tick.
///
/// Carries the [`kind`](TriggerEvent::kind) and a `name` so a trigger can match a
/// *specific* alarm / cue / GPI rather than any of its class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerEvent {
    /// The event class.
    pub kind: EventKind,
    /// The specific event name (alarm id, cue label, GPI point name).
    pub name: String,
}

impl TriggerEvent {
    /// Construct an event of `kind` named `name`.
    #[must_use]
    pub fn new(kind: EventKind, name: impl Into<String>) -> Self {
        Self {
            kind,
            name: name.into(),
        }
    }
}

/// The action an automation rule produces when it fires.
///
/// Today this is a named salvo to take (the salvo engine resolves it into a
/// [`SalvoBatch`](crate::salvo::SalvoBatch)); the enum is `#[non_exhaustive]` so
/// other command actions can be added.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScheduledAction {
    /// Take (commit) the named salvo.
    TakeSalvo {
        /// The salvo name to take.
        salvo: String,
    },
}

impl ScheduledAction {
    /// Construct a "take salvo" action for `salvo`.
    #[must_use]
    pub fn take_salvo(salvo: impl Into<String>) -> Self {
        Self::TakeSalvo {
            salvo: salvo.into(),
        }
    }
}

/// What makes a rule fire.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Trigger {
    /// Fire every `interval` from `base`; `next` is the next due instant.
    Interval {
        base: MediaTime,
        interval: MediaTime,
        next: MediaTime,
    },
    /// Fire when a matching event is reported.
    Event { kind: EventKind, name: String },
}

/// One automation rule: a trigger plus the action it produces.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    trigger: Trigger,
    action: ScheduledAction,
}

/// A pure automation scheduler over an injected [`MediaTime`].
///
/// Build it with [`Scheduler::new`], register rules with
/// [`Scheduler::every`] / [`Scheduler::on_event`], then drive it once per control
/// tick with [`Scheduler::tick`]. The scheduler carries only its interval-rule
/// cursors between ticks; it is `Clone` for lock-free snapshotting.
#[derive(Debug, Clone, Default)]
pub struct Scheduler {
    rules: Vec<Rule>,
}

impl Scheduler {
    /// An empty scheduler with no rules.
    #[must_use]
    pub fn new() -> Self {
        Self { rules: Vec::new() }
    }

    /// Register an **interval** rule: fire `action` every `interval` starting at
    /// `base`. A non-positive `interval` is rejected (the rule is not added) and
    /// returns `false`, since a zero/negative period would fire unboundedly.
    /// Returns `true` when the rule was added.
    pub fn every(&mut self, base: MediaTime, interval: MediaTime, action: ScheduledAction) -> bool {
        if interval.as_nanos() <= 0 {
            return false;
        }
        self.rules.push(Rule {
            trigger: Trigger::Interval {
                base,
                interval,
                next: base,
            },
            action,
        });
        true
    }

    /// Register an **event** rule: fire `action` whenever an event of `kind`
    /// named `name` is reported to [`tick`](Scheduler::tick).
    pub fn on_event(&mut self, kind: EventKind, name: impl Into<String>, action: ScheduledAction) {
        self.rules.push(Rule {
            trigger: Trigger::Event {
                kind,
                name: name.into(),
            },
            action,
        });
    }

    /// The number of registered rules.
    #[must_use]
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// Advance the scheduler to media time `now`, reporting any `events` that
    /// occurred this tick, and **return** the actions to fire (in rule order).
    ///
    /// Interval rules fire once for each due instant at or before `now` — if a
    /// coarse tick skips several periods the rule still fires only **once** per
    /// tick (catch-up is coalesced, never a burst) and its cursor advances past
    /// `now` so it cannot re-fire for the same window. Event rules fire once for
    /// each matching event reported this tick.
    ///
    /// Pure and total: a non-monotonic `now` never fires an interval rule early
    /// (a due instant strictly in the future is not yet reached) and never
    /// rewinds a cursor.
    #[must_use]
    pub fn tick(&mut self, now: MediaTime, events: &[TriggerEvent]) -> Vec<ScheduledAction> {
        let mut fired = Vec::new();
        for rule in &mut self.rules {
            match &mut rule.trigger {
                Trigger::Interval {
                    base,
                    interval,
                    next,
                } => {
                    if now >= *next {
                        fired.push(rule.action.clone());
                        *next = advance_past(*base, *interval, now);
                    }
                }
                Trigger::Event { kind, name } => {
                    if events.iter().any(|e| e.kind == *kind && e.name == *name) {
                        fired.push(rule.action.clone());
                    }
                }
            }
        }
        fired
    }
}

/// Compute the first interval instant strictly greater than `now`, given the
/// rule's `base` and `interval`. Coalesces missed periods: the returned cursor is
/// always `> now`, so a single tick fires at most once and never replays a window.
fn advance_past(base: MediaTime, interval: MediaTime, now: MediaTime) -> MediaTime {
    let interval_ns = interval.as_nanos().max(1);
    let base_ns = base.as_nanos();
    let now_ns = now.as_nanos();
    if now_ns < base_ns {
        // `now` precedes the base; the next due instant is the base itself.
        return base;
    }
    // Number of whole intervals elapsed since base, then step one past `now`.
    let elapsed = now_ns.saturating_sub(base_ns);
    let steps = elapsed / interval_ns + 1;
    let offset = steps.saturating_mul(interval_ns);
    MediaTime::from_nanos(base_ns.saturating_add(offset))
}
