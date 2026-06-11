//! The device runtime state machine (ADR-M008 §2.2).
//!
//! A managed device rides a typed lifecycle the future driver actors
//! (DEV-A4/A5) drive; this slice models the **state and transitions only** —
//! there is no real device I/O here. The states are the
//! [`multiview_events::DeviceState`] wire vocabulary so the control plane and
//! the realtime `device.status` event speak one enum:
//!
//! ```text
//! DISCOVERED ──adopt──▶ ADOPTING ──probe ok──▶ ONLINE ◀──recover── DEGRADED
//!                          │                     │  ▲                  ▲
//!                          │ bad creds           │  └─reconnect────────┘
//!                          ▼                     ▼
//!                     AUTH_FAILED           UNREACHABLE
//! ```
//!
//! `DISCOVERED` is the untrusted-inventory state (ADR-0041) and is never a
//! registry entry, so a registry device starts in `ADOPTING`. `AUTH_FAILED`
//! opens the supervised-reconnect breaker immediately: a bare reconnect attempt
//! does **nothing** while it is open; only a credential update re-arms a probe.

use multiview_events::DeviceState;

/// An input to the device lifecycle — what the driver actor (a future slice)
/// reports after a probe / poll / supervision step.
///
/// `#[non_exhaustive]` so a later driver can report a new condition without a
/// breaking change; matches over it carry a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LifecycleEvent {
    /// A probe succeeded: the device answered the management channel cleanly.
    ProbeOk,
    /// The device answers but reports a fault (decode stalled, over-temperature):
    /// the management channel is up, the media side is degraded.
    DeviceFault,
    /// A previously-reported fault cleared (DEGRADED → ONLINE).
    Recover,
    /// The management channel rejected our credentials (opens the breaker).
    AuthRejected,
    /// The device did not answer within the probe window (supervised reconnect
    /// drives the backoff).
    Unreachable,
    /// Supervised reconnect re-established the management channel; the driver
    /// re-converges desired state. Has no effect while the auth breaker is open.
    Reconnect,
    /// The operator updated the stored secret: re-arm a probe out of
    /// `AUTH_FAILED`.
    SecretUpdated,
}

/// A managed device's lifecycle state and its transition function.
///
/// Construct one with [`DeviceLifecycle::new`] (starts in `ADOPTING`, the
/// registry-entry start state) and feed it [`LifecycleEvent`]s with
/// [`DeviceLifecycle::apply`]. The transition function is total — every
/// `(state, event)` pair has a defined result — so the future driver code can
/// drive it without ever hitting an undefined edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceLifecycle {
    state: DeviceState,
}

impl Default for DeviceLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl DeviceLifecycle {
    /// A freshly-adopted device: the registry entry exists and the first probe
    /// is in flight (`ADOPTING`).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: DeviceState::Adopting,
        }
    }

    /// Build a lifecycle pinned to an explicit `state` (e.g. rehydrating a
    /// known runtime status).
    #[must_use]
    pub const fn in_state(state: DeviceState) -> Self {
        Self { state }
    }

    /// The current lifecycle state.
    #[must_use]
    pub const fn state(self) -> DeviceState {
        self.state
    }

    /// Compute the state `event` leads to from `state`, total over every pair.
    ///
    /// Kept pure (no `&self`) so the transition table is unit-testable in
    /// isolation and the driver code can reason about an edge without a live
    /// instance.
    // The table is intentionally written one explicit `(state, event)` arm per
    // edge (ADR-M008 §2.2), grouped by source state, so each transition reads as
    // a row of the documented diagram. Several edges share a destination
    // (`AuthFailed` / `Unreachable` / `Online`); merging those arms by
    // destination would scramble the per-state grouping and obscure the table —
    // the explicit edges are the design, so `match_same_arms` is suppressed here.
    #[must_use]
    #[allow(clippy::match_same_arms)]
    pub fn transition(state: DeviceState, event: LifecycleEvent) -> DeviceState {
        match (state, event) {
            // ADOPTING: the first probe resolves it.
            (DeviceState::Adopting, LifecycleEvent::ProbeOk) => DeviceState::Online,
            (DeviceState::Adopting, LifecycleEvent::AuthRejected) => DeviceState::AuthFailed,
            (DeviceState::Adopting, LifecycleEvent::Unreachable) => DeviceState::Unreachable,

            // ONLINE: device-reported faults degrade it; loss of the channel or
            // a credential rejection moves it out.
            (DeviceState::Online, LifecycleEvent::DeviceFault) => DeviceState::Degraded,
            (DeviceState::Online, LifecycleEvent::Unreachable) => DeviceState::Unreachable,
            (DeviceState::Online, LifecycleEvent::AuthRejected) => DeviceState::AuthFailed,

            // DEGRADED: recovers to ONLINE, or drops to UNREACHABLE / AUTH_FAILED.
            (DeviceState::Degraded, LifecycleEvent::Recover | LifecycleEvent::ProbeOk) => {
                DeviceState::Online
            }
            (DeviceState::Degraded, LifecycleEvent::Unreachable) => DeviceState::Unreachable,
            (DeviceState::Degraded, LifecycleEvent::AuthRejected) => DeviceState::AuthFailed,

            // UNREACHABLE: supervised reconnect (or a fresh probe) brings it
            // back; an auth rejection during reconnect opens the breaker.
            (DeviceState::Unreachable, LifecycleEvent::Reconnect | LifecycleEvent::ProbeOk) => {
                DeviceState::Online
            }
            (DeviceState::Unreachable, LifecycleEvent::AuthRejected) => DeviceState::AuthFailed,

            // AUTH_FAILED: the breaker is open. Only a credential update re-arms
            // a probe (back to ADOPTING); every other event is ignored — no
            // reconnect storm against a device that rejected our secret.
            (DeviceState::AuthFailed, LifecycleEvent::SecretUpdated) => DeviceState::Adopting,

            // DISCOVERED is the untrusted-inventory state and is never a
            // registry entry; a confirm-adopt creates an ADOPTING record. Any
            // event here other than a successful adopt-probe is a no-op.
            (DeviceState::Discovered, LifecycleEvent::ProbeOk) => DeviceState::Adopting,

            // Every other pair is a no-op: the state is unchanged.
            (other, _) => other,
        }
    }

    /// Apply `event`, mutating the state in place. Returns `true` when the state
    /// actually changed (so the caller knows whether to emit a status delta).
    pub fn apply(&mut self, event: LifecycleEvent) -> bool {
        let next = Self::transition(self.state, event);
        let changed = next != self.state;
        self.state = next;
        changed
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{DeviceLifecycle, LifecycleEvent};
    use multiview_events::DeviceState;

    #[test]
    fn registry_devices_start_in_adopting() {
        assert_eq!(DeviceLifecycle::new().state(), DeviceState::Adopting);
    }

    #[test]
    fn transition_is_total_and_pure() {
        // Every (state, event) pair has a defined result and is idempotent under
        // re-application of the same event from the resulting state's class.
        let states = [
            DeviceState::Discovered,
            DeviceState::Adopting,
            DeviceState::Online,
            DeviceState::Degraded,
            DeviceState::AuthFailed,
            DeviceState::Unreachable,
        ];
        let events = [
            LifecycleEvent::ProbeOk,
            LifecycleEvent::DeviceFault,
            LifecycleEvent::Recover,
            LifecycleEvent::AuthRejected,
            LifecycleEvent::Unreachable,
            LifecycleEvent::Reconnect,
            LifecycleEvent::SecretUpdated,
        ];
        for s in states {
            for e in events {
                // Just exercising every edge proves totality (no panic / no
                // missing arm) and lets the pure function be reasoned about.
                let _ = DeviceLifecycle::transition(s, e);
            }
        }
    }

    #[test]
    fn auth_failed_breaker_ignores_reconnect() {
        let mut lc = DeviceLifecycle::new();
        lc.apply(LifecycleEvent::AuthRejected);
        assert_eq!(lc.state(), DeviceState::AuthFailed);
        assert!(!lc.apply(LifecycleEvent::Reconnect));
        assert_eq!(lc.state(), DeviceState::AuthFailed);
        assert!(lc.apply(LifecycleEvent::SecretUpdated));
        assert_eq!(lc.state(), DeviceState::Adopting);
    }
}
