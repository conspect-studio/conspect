//! The **pending remote-actions queue** + strip backend (Conspect, the brief
//! §10/§11 #30–#34 + spec §6/§11 "remote actions / pending-action strip").
//!
//! A *remote action* is an operator-required action surfaced for local approval
//! or local cancellation: **restart** (control-plane restart), **reboot** (a
//! machine-level lifecycle action), or **salvo** (fire a named action set). The
//! queue is the backend for the SPA's pending-action strip (brief §13.7).
//!
//! # Who fills the queue
//!
//! Actions are enqueued **locally now** — the operator drives restart/reboot/
//! salvo from the machine UI — and **portal-fed later** (a portal-initiated
//! action arrives over the heartbeat/relay transport, which is O1-blocked, so
//! that producer is the later server-client item). Either way the *queue + the
//! local surfaces* are complete and locally drivable, exactly as the spec
//! requires for local actions and salvos.
//!
//! # Local always wins
//!
//! [`PendingActionStore::cancel`] is the operator's local override: an action the
//! operator cancels **before** it executes is cancelled, full stop — the cancel
//! takes precedence over any later (portal-fed) execution attempt. An action that
//! has **already executed** answers [`CancelOutcome::AlreadyExecuted`] (surfaced
//! as `410 Gone`) — the truthful "too late" — never a silent success. This
//! local-always-wins rule is pinned by test.
//!
//! Like every store in this crate it holds control-plane state only and is never
//! on the engine's data plane, so however it synchronises internally it cannot
//! back-pressure the engine (invariant #10). The queue is bounded (drop-oldest)
//! so a flood of requests cannot grow control-plane memory without bound.
use std::collections::VecDeque;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use multiview_core::time::MediaTime;

/// The resource collection name used in routes and problem documents.
pub const PENDING_ACTION_KIND: &str = "action";

/// The default cap on retained pending/terminal actions (oldest evicted).
///
/// The strip surfaces only a handful of pending actions at a time; the cap bounds
/// memory while keeping recent terminal (executed/cancelled) actions visible for
/// the audit/strip correlation (invariant #10 / brief §16).
pub const DEFAULT_PENDING_ACTION_CAPACITY: usize = 256;

/// The kind of remote action queued for local approval/execution.
///
/// Serialised `kebab-case` so the slug is stable across the machine and the
/// portal. `#[non_exhaustive]` so a future action kind adds a variant without
/// breaking the wire contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum PendingActionKind {
    /// Restart the control plane (routed through the command bus on execute).
    Restart,
    /// Reboot the machine (a machine-level lifecycle action; surfaced as data +
    /// audited — the actual reboot is the daemon's lifecycle path, never faked).
    Reboot,
    /// Fire a named salvo (routed through the command bus `TakeSalvo` on execute).
    Salvo,
}

/// The lifecycle state of a queued action.
///
/// An action is `Pending` until it is either `Executed` (it ran) or `Cancelled`
/// (the operator cancelled it locally first). Both terminal states are immutable
/// once reached — the local cancel cannot be undone by a later execution, and an
/// executed action cannot be retroactively cancelled (local-always-wins applies
/// only *before* execution).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum PendingActionState {
    /// Queued, awaiting execution or local cancellation.
    Pending,
    /// The action executed.
    Executed,
    /// The operator cancelled the action locally before it executed.
    Cancelled,
}

/// One queued remote action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct PendingAction {
    /// The opaque, unique action id (a UUID) the cancel/execute paths address.
    pub action_id: String,
    /// The kind of action.
    pub kind: PendingActionKind,
    /// The principal (or portal/system actor) that requested the action.
    pub requested_by: String,
    /// The media-timeline timestamp (nanoseconds) the action was queued.
    pub requested_at_nanos: i64,
    /// The action's lifecycle state.
    pub state: PendingActionState,
    /// An optional structured detail (e.g. the salvo name a `salvo` action
    /// fires). Never contains secrets.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

/// The outcome of a [`PendingActionStore::cancel`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// The action was pending and is now cancelled (local always wins).
    Cancelled,
    /// The action had already executed — too late to cancel (`410 Gone`).
    AlreadyExecuted,
    /// The action was already cancelled (idempotent re-cancel is a success).
    AlreadyCancelled,
    /// No action has that id.
    NotFound,
}

/// The outcome of a [`PendingActionStore::mark_executed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecuteOutcome {
    /// The action was pending and is now marked executed.
    Executed,
    /// The operator had already cancelled it — execution is refused
    /// (local-always-wins: the cancel takes precedence).
    Cancelled,
    /// The action had already executed.
    AlreadyExecuted,
    /// No action has that id.
    NotFound,
}

/// Append + query + transition access to the pending-actions queue.
///
/// There is no "edit" of a pending action's fields — only the lifecycle
/// transitions [`PendingActionStore::cancel`] and
/// [`PendingActionStore::mark_executed`], both of which are one-way into a
/// terminal state. The queue is otherwise append + read.
pub trait PendingActionStore: Send + Sync + 'static {
    /// Enqueue a new pending action, returning it. The store assigns the
    /// `requested_at_nanos` from the supplied `at` and sets `state = Pending`.
    fn enqueue(
        &self,
        action_id: String,
        kind: PendingActionKind,
        requested_by: &str,
        at: MediaTime,
        detail: Option<serde_json::Value>,
    ) -> PendingAction;

    /// List the **pending** actions only, oldest-first (the strip surface).
    fn list_pending(&self) -> Vec<PendingAction>;

    /// Fetch one action by id (any state), or `None` if unknown.
    fn get(&self, action_id: &str) -> Option<PendingAction>;

    /// Cancel a pending action locally — **local always wins** (the cancel
    /// precedes any later execution). See [`CancelOutcome`].
    fn cancel(&self, action_id: &str) -> CancelOutcome;

    /// Mark a pending action executed — refused if the operator already
    /// cancelled it (local-always-wins). See [`ExecuteOutcome`].
    fn mark_executed(&self, action_id: &str) -> ExecuteOutcome;
}

/// Alias matching the crate's `*Repository` naming for trait objects in state.
pub use PendingActionStore as PendingActionRepository;

/// An in-memory, bounded pending-actions queue.
///
/// Backed by a `Mutex<VecDeque>` capped at a fixed capacity (oldest evicted).
/// The lock guards control-plane state only — never held by the engine — so it
/// cannot back-pressure the engine (invariant #10).
#[derive(Debug)]
pub struct InMemoryPendingActions {
    capacity: usize,
    actions: Mutex<VecDeque<PendingAction>>,
}

impl Default for InMemoryPendingActions {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_PENDING_ACTION_CAPACITY)
    }
}

impl InMemoryPendingActions {
    /// A fresh, empty queue at the default capacity.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A fresh, empty queue retaining at most `capacity` newest actions.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            actions: Mutex::new(VecDeque::new()),
        }
    }

    /// Lock the inner queue, recovering from a poisoned lock (a panic in another
    /// request must not wedge the whole control plane).
    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<PendingAction>> {
        match self.actions.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl PendingActionStore for InMemoryPendingActions {
    fn enqueue(
        &self,
        action_id: String,
        kind: PendingActionKind,
        requested_by: &str,
        at: MediaTime,
        detail: Option<serde_json::Value>,
    ) -> PendingAction {
        let action = PendingAction {
            action_id,
            kind,
            requested_by: requested_by.to_owned(),
            requested_at_nanos: at.as_nanos(),
            state: PendingActionState::Pending,
            detail,
        };
        let mut guard = self.lock();
        guard.push_back(action.clone());
        while guard.len() > self.capacity {
            guard.pop_front();
        }
        action
    }

    fn list_pending(&self) -> Vec<PendingAction> {
        self.lock()
            .iter()
            .filter(|a| a.state == PendingActionState::Pending)
            .cloned()
            .collect()
    }

    fn get(&self, action_id: &str) -> Option<PendingAction> {
        self.lock()
            .iter()
            .find(|a| a.action_id == action_id)
            .cloned()
    }

    fn cancel(&self, action_id: &str) -> CancelOutcome {
        let mut guard = self.lock();
        let Some(action) = guard.iter_mut().find(|a| a.action_id == action_id) else {
            return CancelOutcome::NotFound;
        };
        match action.state {
            PendingActionState::Pending => {
                action.state = PendingActionState::Cancelled;
                CancelOutcome::Cancelled
            }
            // Local always wins applies *before* execution: once executed, the
            // truthful answer is "too late", never a fake success.
            PendingActionState::Executed => CancelOutcome::AlreadyExecuted,
            PendingActionState::Cancelled => CancelOutcome::AlreadyCancelled,
        }
    }

    fn mark_executed(&self, action_id: &str) -> ExecuteOutcome {
        let mut guard = self.lock();
        let Some(action) = guard.iter_mut().find(|a| a.action_id == action_id) else {
            return ExecuteOutcome::NotFound;
        };
        match action.state {
            PendingActionState::Pending => {
                action.state = PendingActionState::Executed;
                ExecuteOutcome::Executed
            }
            // The operator's local cancel takes precedence: a cancelled action
            // never executes, even if a (portal-fed) execution arrives later.
            PendingActionState::Cancelled => ExecuteOutcome::Cancelled,
            PendingActionState::Executed => ExecuteOutcome::AlreadyExecuted,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{
        CancelOutcome, ExecuteOutcome, InMemoryPendingActions, PendingActionKind,
        PendingActionState, PendingActionStore,
    };
    use multiview_core::time::MediaTime;

    fn enqueue(store: &InMemoryPendingActions, id: &str, kind: PendingActionKind) {
        store.enqueue(
            id.to_owned(),
            kind,
            "operator",
            MediaTime::from_nanos(1),
            None,
        );
    }

    #[test]
    fn enqueue_then_list_pending_shows_only_pending_oldest_first() {
        let store = InMemoryPendingActions::new();
        enqueue(&store, "a", PendingActionKind::Restart);
        enqueue(&store, "b", PendingActionKind::Reboot);
        let pending = store.list_pending();
        let ids: Vec<&str> = pending.iter().map(|a| a.action_id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"], "oldest-first, both pending");
        assert!(pending
            .iter()
            .all(|a| a.state == PendingActionState::Pending));
    }

    #[test]
    fn local_cancel_of_a_pending_action_always_wins() {
        let store = InMemoryPendingActions::new();
        enqueue(&store, "a", PendingActionKind::Reboot);
        assert_eq!(store.cancel("a"), CancelOutcome::Cancelled);
        // A cancelled action drops out of the strip.
        assert!(store.list_pending().is_empty());
        // And a later execution attempt is refused — the local cancel precedes it.
        assert_eq!(store.mark_executed("a"), ExecuteOutcome::Cancelled);
        assert_eq!(
            store.get("a").unwrap().state,
            PendingActionState::Cancelled,
            "the action stays cancelled; execution never overrides a local cancel"
        );
    }

    #[test]
    fn cancel_after_execution_is_already_executed_not_a_fake_success() {
        let store = InMemoryPendingActions::new();
        enqueue(&store, "a", PendingActionKind::Salvo);
        assert_eq!(store.mark_executed("a"), ExecuteOutcome::Executed);
        // Too late: the truthful answer, surfaced as 410 Gone by the route.
        assert_eq!(store.cancel("a"), CancelOutcome::AlreadyExecuted);
        assert_eq!(store.get("a").unwrap().state, PendingActionState::Executed);
    }

    #[test]
    fn re_cancel_is_idempotent_and_unknown_is_not_found() {
        let store = InMemoryPendingActions::new();
        enqueue(&store, "a", PendingActionKind::Restart);
        assert_eq!(store.cancel("a"), CancelOutcome::Cancelled);
        assert_eq!(store.cancel("a"), CancelOutcome::AlreadyCancelled);
        assert_eq!(store.cancel("missing"), CancelOutcome::NotFound);
        assert_eq!(store.mark_executed("missing"), ExecuteOutcome::NotFound);
    }

    #[test]
    fn bounded_eviction_drops_oldest() {
        let store = InMemoryPendingActions::with_capacity(2);
        enqueue(&store, "a", PendingActionKind::Restart);
        enqueue(&store, "b", PendingActionKind::Restart);
        enqueue(&store, "c", PendingActionKind::Restart);
        assert!(store.get("a").is_none(), "oldest evicted");
        assert!(store.get("b").is_some());
        assert!(store.get("c").is_some());
    }
}
