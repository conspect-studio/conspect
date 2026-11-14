//! Atomic multi-destination patch sets on the control plane (ADR-0110
//! decision 8, output-routing §12.8, #308): the set-scoped idempotency store
//! and the `patch.take` outcome aggregator.
//!
//! **Idempotency.** A set's `Idempotency-Key` lives in its own namespace
//! (`patch-set:` for `POST /patch/take`, `salvo-take:` for a patch salvo),
//! bound to a digest of the canonical body (crosspoints sorted, `on_air`):
//! a replay with the same body answers its first 202, a different body is
//! 409 `conflict`. Bounded: [`MAX_SET_RECORDS`] in total and
//! [`MAX_SET_RECORDS_PER_PRINCIPAL`] per principal; under pressure the
//! oldest SETTLED record goes, a pending one never (fail closed with
//! `idempotency-capacity`).
//!
//! **Outcome.** The engine reports each output member on `route.output` and
//! the mixer half on `switcher.operation`, all carrying the set's operation
//! id. The aggregator (one task, off every clock, fed by the bounded
//! drop-oldest engine event stream) collects them and, once every member is
//! terminal, publishes one `patch.take` per destination scoped by that
//! destination, so realtime filtering redacts the others. At most
//! [`MAX_PENDING_SETS`] sets wait at once (admission answers 503 beyond);
//! a set with no final outcome after [`SET_OUTCOME_DEADLINE`] is published
//! `refused` / `deadline`. Nothing here persists: an applied output member
//! persists through the `route.output` relay, the mixer half through the
//! mixer persist relay.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use multiview_events::{Event, PatchTakeDestination, PatchTakeEvent};

use crate::command::OperationId;
use crate::state::AppState;

/// Set records the store holds (all principals together).
pub const MAX_SET_RECORDS: usize = 1024;
/// Set records one principal may hold.
pub const MAX_SET_RECORDS_PER_PRINCIPAL: usize = 256;
/// Sets awaiting their final outcome at once.
pub const MAX_PENDING_SETS: usize = 256;
/// A set with no final outcome this long after admission is published
/// `refused` / `deadline` (the engine refuses an unadopted set after 150
/// ticks; this is the control plane's own backstop).
pub const SET_OUTCOME_DEADLINE: Duration = Duration::from_secs(10);

/// The M/E and program of the one mixer half (first slice).
const ME: &str = "me1";
const PROGRAM: &str = "main";

/// The result of [`PatchSets::reserve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetReservation {
    /// Unseen key: enqueue under this new operation id.
    Fresh(OperationId),
    /// The key names this very body: answer its first operation.
    Replay(OperationId),
    /// The key names another body: 409 `conflict`, nothing enqueued.
    Conflict,
    /// No record can be made (every candidate is pending), or
    /// [`MAX_PENDING_SETS`] sets await their outcome: fail closed.
    Capacity,
}

#[derive(Debug)]
struct Record {
    operation_id: OperationId,
    digest: String,
    principal: String,
    seq: u64,
    settled: bool,
}

/// One member's terminal report.
#[derive(Debug, Clone)]
struct Report {
    tick: u64,
    applied: bool,
    /// The typed refusal kind when not applied.
    kind: Option<String>,
}

/// A set's mixer half, for the aggregate.
#[derive(Debug)]
enum MixerHalf {
    /// The set has no mixer member.
    Absent,
    /// Awaiting the `switcher.operation` outcome.
    Waiting,
    /// Reported.
    Reported(Report),
}

#[derive(Debug)]
struct Pending {
    key: String,
    /// Output API id → its report once terminal.
    outputs: BTreeMap<String, Option<Report>>,
    /// The mixer half's report state.
    mixer: MixerHalf,
    /// The first non-applied cause seen, in arrival order.
    first_cause: Option<String>,
    admitted: Instant,
}

#[derive(Debug, Default)]
struct Inner {
    next_seq: u64,
    records: HashMap<String, Record>,
    held: HashMap<String, usize>,
    pending: HashMap<String, Pending>,
}

impl Inner {
    fn remove_record(&mut self, key: &str) {
        if let Some(record) = self.records.remove(key) {
            if let Some(count) = self.held.get_mut(&record.principal) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.held.remove(&record.principal);
                }
            }
        }
    }

    /// Evict the oldest settled record (of `principal` only, when given).
    fn evict_settled(&mut self, principal: Option<&str>) -> bool {
        let victim = self
            .records
            .iter()
            .filter(|(_, r)| r.settled && principal.is_none_or(|p| r.principal == p))
            .min_by_key(|(_, r)| r.seq)
            .map(|(k, _)| k.clone());
        victim.is_some_and(|key| {
            self.remove_record(&key);
            true
        })
    }
}

/// The set store and aggregator (one per process, in [`AppState`]).
#[derive(Debug, Default)]
pub struct PatchSets {
    inner: Mutex<Inner>,
}

/// What a set admission registers for its outcome.
#[derive(Debug, Clone)]
pub struct SetMembers {
    /// The output API ids of the output members.
    pub outputs: Vec<String>,
    /// Whether the set has a mixer half.
    pub mixer: bool,
}

impl PatchSets {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The operation a key already names, bound to `digest`: `Some(Ok(op))`
    /// for the same body, `Some(Err(()))` for another body, `None` unseen.
    #[must_use]
    pub fn peek(&self, key: &str, digest: &str) -> Option<Result<OperationId, ()>> {
        self.lock().records.get(key).map(|r| {
            if r.digest == digest {
                Ok(r.operation_id.clone())
            } else {
                Err(())
            }
        })
    }

    /// Reserve `key` for the body `digest`, charged to `principal`, and
    /// register the fresh set for its outcome (`members`, admitted `now`),
    /// all under ONE lock acquisition: the pending bound
    /// ([`MAX_PENDING_SETS`]) is checked and charged atomically with the key
    /// record, so concurrent admissions never exceed it. Registered before
    /// the enqueue, so an outcome never beats it. A failure after this point
    /// must [`Self::release`] both; settlement and the deadline free the
    /// pending slot.
    pub fn reserve(
        &self,
        key: &str,
        digest: &str,
        principal: &str,
        members: &SetMembers,
        now: Instant,
    ) -> SetReservation {
        let mut inner = self.lock();
        if let Some(record) = inner.records.get(key) {
            return if record.digest == digest {
                SetReservation::Replay(record.operation_id.clone())
            } else {
                SetReservation::Conflict
            };
        }
        if inner.pending.len() >= MAX_PENDING_SETS {
            return SetReservation::Capacity;
        }
        let Some(next) = inner.next_seq.checked_add(1) else {
            return SetReservation::Capacity;
        };
        if inner.held.get(principal).copied().unwrap_or(0) >= MAX_SET_RECORDS_PER_PRINCIPAL
            && !inner.evict_settled(Some(principal))
        {
            return SetReservation::Capacity;
        }
        if inner.records.len() >= MAX_SET_RECORDS && !inner.evict_settled(None) {
            return SetReservation::Capacity;
        }
        let seq = inner.next_seq;
        inner.next_seq = next;
        let op = OperationId::new();
        *inner.held.entry(principal.to_owned()).or_insert(0) += 1;
        inner.records.insert(
            key.to_owned(),
            Record {
                operation_id: op.clone(),
                digest: digest.to_owned(),
                principal: principal.to_owned(),
                seq,
                settled: false,
            },
        );
        inner.pending.insert(
            op.as_str().to_owned(),
            Pending {
                key: key.to_owned(),
                outputs: members.outputs.iter().map(|o| (o.clone(), None)).collect(),
                mixer: if members.mixer {
                    MixerHalf::Waiting
                } else {
                    MixerHalf::Absent
                },
                first_cause: None,
                admitted: now,
            },
        );
        SetReservation::Fresh(op)
    }

    /// Undo [`Self::reserve`] (the key record AND the pending slot) for a
    /// set that never reached the engine (only if `key` still names `op`).
    pub fn release(&self, key: &str, op: &OperationId) {
        let mut inner = self.lock();
        inner.pending.remove(op.as_str());
        if inner
            .records
            .get(key)
            .is_some_and(|r| r.operation_id == *op)
        {
            inner.remove_record(key);
        }
    }

    /// Sets awaiting their final outcome (bounded by [`MAX_PENDING_SETS`]).
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.lock().pending.len()
    }

    /// Set records held (bounded by [`MAX_SET_RECORDS`]).
    #[must_use]
    pub fn records_len(&self) -> usize {
        self.lock().records.len()
    }

    /// Feed one engine event; the `patch.take` deliveries it completes.
    #[must_use]
    pub fn observe(&self, event: &Event) -> Vec<Event> {
        let mut inner = self.lock();
        let op = match event {
            Event::RouteOutput(e) => {
                let Some(pending) = inner.pending.get_mut(&e.operation_id) else {
                    return Vec::new();
                };
                let Some(slot) = pending.outputs.get_mut(&e.output) else {
                    return Vec::new();
                };
                if slot.is_some() {
                    return Vec::new();
                }
                let applied = e.outcome == "applied";
                let kind = (!applied).then(|| output_refusal_kind(&e.outcome).to_owned());
                if let (None, Some(kind)) = (&pending.first_cause, &kind) {
                    pending.first_cause = Some(kind.clone());
                }
                *slot = Some(Report {
                    tick: e.tick,
                    applied,
                    kind,
                });
                e.operation_id.clone()
            }
            Event::SwitcherOperation(e) => {
                let Some(pending) = inner.pending.get_mut(&e.operation_id) else {
                    return Vec::new();
                };
                if !matches!(pending.mixer, MixerHalf::Waiting) {
                    return Vec::new();
                }
                let kind = (!e.completed).then(|| {
                    mixer_refusal_kind(e.error.as_deref().unwrap_or("refused")).to_owned()
                });
                pending.mixer = MixerHalf::Reported(Report {
                    tick: e.tick,
                    applied: e.completed,
                    kind: kind.clone(),
                });
                if let (None, Some(kind)) = (&pending.first_cause, kind) {
                    pending.first_cause = Some(kind);
                }
                e.operation_id.clone()
            }
            _ => return Vec::new(),
        };
        let complete = inner.pending.get(&op).is_some_and(|p| {
            p.outputs.values().all(Option::is_some) && !matches!(p.mixer, MixerHalf::Waiting)
        });
        if !complete {
            return Vec::new();
        }
        settle(&mut inner, &op, None)
    }

    /// Publish `refused` / `deadline` for every set admitted at least
    /// [`SET_OUTCOME_DEADLINE`] before `now`; the deliveries.
    #[must_use]
    pub fn expire(&self, now: Instant) -> Vec<Event> {
        let mut inner = self.lock();
        let late: Vec<String> = inner
            .pending
            .iter()
            .filter(|(_, p)| now.saturating_duration_since(p.admitted) >= SET_OUTCOME_DEADLINE)
            .map(|(op, _)| op.clone())
            .collect();
        let mut out = Vec::new();
        for op in late {
            out.extend(settle(&mut inner, &op, Some("deadline")));
        }
        out
    }
}

/// Remove `op` from the pending map, mark its record settled and build one
/// `patch.take` per destination. `forced` is a refusal kind that overrides
/// the members' reports (the deadline).
fn settle(inner: &mut Inner, op: &str, forced: Option<&str>) -> Vec<Event> {
    let Some(pending) = inner.pending.remove(op) else {
        return Vec::new();
    };
    if let Some(record) = inner.records.get_mut(&pending.key) {
        record.settled = true;
    }
    let mixer_report = match &pending.mixer {
        MixerHalf::Reported(report) => Some(report),
        MixerHalf::Absent | MixerHalf::Waiting => None,
    };
    let reports: Vec<&Report> = pending
        .outputs
        .values()
        .flatten()
        .chain(mixer_report)
        .collect();
    let ticks: Vec<u64> = reports.iter().map(|r| r.tick).collect();
    let same_tick = ticks.windows(2).all(|w| w.first() == w.get(1));
    let all_applied = !reports.is_empty() && reports.iter().all(|r| r.applied);
    let (outcome, refusal) = match forced {
        Some(kind) => ("refused", Some(kind.to_owned())),
        None if all_applied && same_tick => ("applied", None),
        // Every member applied but not at one tick: a residual fault the
        // engine counts as `set-partial`; never reported as applied.
        None if all_applied => ("refused", Some("set-partial".to_owned())),
        None => (
            "refused",
            Some(
                pending
                    .first_cause
                    .clone()
                    .or_else(|| reports.iter().find_map(|r| r.kind.clone()))
                    .unwrap_or_else(|| "refused".to_owned()),
            ),
        ),
    };
    // The adoption tick when applied; otherwise the latest decision tick.
    let tick = ticks.iter().copied().max().unwrap_or(0);
    let event = |destination: PatchTakeDestination| {
        Event::PatchTake(PatchTakeEvent {
            operation_id: op.to_owned(),
            tick,
            outcome: outcome.to_owned(),
            refusal: refusal.clone(),
            destination,
        })
    };
    let mut out: Vec<Event> = pending
        .outputs
        .keys()
        .map(|output| {
            event(PatchTakeDestination {
                output: Some(output.clone()),
                mixer: None,
                program: None,
            })
        })
        .collect();
    if !matches!(pending.mixer, MixerHalf::Absent) {
        out.push(event(PatchTakeDestination {
            output: None,
            mixer: Some(ME.to_owned()),
            program: Some(PROGRAM.to_owned()),
        }));
    }
    out
}

/// The §12.8 typed kind of an engine refusal code (the part after
/// `refused:`), shared by output members and the mixer half: the engine's
/// `unroutable` is `slot-medium-mismatch`, its `format-mismatch`
/// (`SetRefusal::FormatMismatch`) is `slot-format-unsupported`; every other
/// code is already a §12.8 kind.
#[must_use]
fn typed_kind(code: &str) -> &str {
    match code {
        "unroutable" => "slot-medium-mismatch",
        "format-mismatch" => "slot-format-unsupported",
        other => other,
    }
}

/// The §12.8 refusal kind of an output member's `route.output` outcome.
#[must_use]
pub fn output_refusal_kind(outcome: &str) -> &str {
    match outcome {
        "superseded" => "superseded",
        other => typed_kind(
            other
                .strip_prefix("refused:")
                .or_else(|| other.strip_prefix("fallback:"))
                .unwrap_or(other),
        ),
    }
}

/// The §12.8 refusal kind of the mixer half's `switcher.operation` error,
/// normalized through the same typed mapping as an output member's.
#[must_use]
pub fn mixer_refusal_kind(error: &str) -> &str {
    if let Some(code) = error.strip_prefix("refused:") {
        return typed_kind(code.split(':').next().unwrap_or(code));
    }
    if error.starts_with("engine-busy") {
        return "engine-busy";
    }
    crate::patch::refusal_kind(error)
}

/// Start the aggregator: one task fed by the engine event stream (bounded,
/// drop-oldest; a lagged stream loses reports, and the deadline settles the
/// set) plus a one-second deadline sweep. Off every clock.
pub(crate) fn spawn_aggregator(state: &AppState) {
    if state.switcher.is_none() && state.output_routing.is_none() {
        return;
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        tracing::warn!("patch sets: no async runtime; patch.take is not published");
        return;
    };
    // Subscribed before the task starts, so no outcome published after the
    // router is built is missed.
    let mut events = state.engine.subscribe();
    let state = state.clone();
    runtime.spawn(Box::pin(async move {
        let mut sweep = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                received = events.recv() => match received {
                    Ok(item) => publish(&state, state.patch_sets.observe(&item.event)),
                    Err(multiview_engine::isolation::RecvError::Lagged(_)) => {}
                    Err(multiview_engine::isolation::RecvError::Closed) => break,
                },
                _ = sweep.tick() => publish(&state, state.patch_sets.expire(Instant::now())),
            }
        }
    }));
}

/// Publish `patch.take` deliveries on the engine event stream.
pub fn publish(state: &AppState, events: Vec<Event>) {
    for event in events {
        state.engine.publish_event(event);
    }
}

/// Settle every set past its deadline at `now` and publish (the sweep's
/// body; public so a test drives it without waiting).
#[must_use]
pub fn expire_sets(state: &AppState, now: Instant) -> usize {
    let events = state.patch_sets.expire(now);
    let n = events.len();
    publish(state, events);
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use multiview_events::{RouteOutputEvent, SwitcherOperation};

    fn route(op: &str, output: &str, tick: u64, outcome: &str) -> Event {
        Event::RouteOutput(RouteOutputEvent {
            output: output.into(),
            operation_id: op.into(),
            tick,
            outcome: outcome.into(),
        })
    }

    fn mixer(op: &str, tick: u64, error: Option<&str>) -> Event {
        Event::SwitcherOperation(SwitcherOperation {
            me: "me1".into(),
            program: "main".into(),
            tick,
            operation_id: op.into(),
            completed: error.is_none(),
            error: error.map(str::to_owned),
        })
    }

    fn members(outputs: &[&str], with_mixer: bool) -> SetMembers {
        SetMembers {
            outputs: outputs.iter().map(|s| (*s).to_owned()).collect(),
            mixer: with_mixer,
        }
    }

    fn reserve(sets: &PatchSets, key: &str, digest: &str, principal: &str) -> SetReservation {
        sets.reserve(
            key,
            digest,
            principal,
            &members(&["o"], false),
            Instant::now(),
        )
    }

    fn admitted(sets: &PatchSets, key: &str, outputs: &[&str], with_mixer: bool) -> String {
        let SetReservation::Fresh(op) =
            sets.reserve(key, "d", "p", &members(outputs, with_mixer), Instant::now())
        else {
            panic!("fresh");
        };
        op.as_str().to_owned()
    }

    fn take(events: &[Event]) -> Vec<&PatchTakeEvent> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::PatchTake(t) => Some(t),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn applied_once_every_member_applied_at_one_tick() {
        let sets = PatchSets::default();
        let op = admitted(&sets, "k", &["output-0", "output-1"], true);
        assert!(
            sets.observe(&route(&op, "output-0", 9, "applied"))
                .is_empty(),
            "output-1 outstanding"
        );
        assert!(
            sets.observe(&mixer(&op, 9, None)).is_empty(),
            "output-1 outstanding"
        );
        let out = sets.observe(&route(&op, "output-1", 9, "applied"));
        let out = take(&out);
        assert_eq!(out.len(), 3, "one delivery per destination");
        assert!(out.iter().all(|t| t.outcome == "applied" && t.tick == 9));
        assert_eq!(sets.pending_len(), 0);
        // A late duplicate settles nothing twice.
        assert!(
            sets.observe(&route(&op, "output-0", 9, "applied"))
                .is_empty(),
            "settled once"
        );
    }

    #[test]
    fn refused_with_the_first_cause() {
        let sets = PatchSets::default();
        let op = admitted(&sets, "k", &["output-0", "output-1"], true);
        let _ = sets.observe(&route(&op, "output-1", 4, "refused:feed-not-fresh"));
        let _ = sets.observe(&route(&op, "output-0", 4, "refused:capacity"));
        let out = sets.observe(&mixer(&op, 4, Some("refused:feed-not-fresh")));
        let out = take(&out);
        assert_eq!(out.len(), 3);
        assert!(out
            .iter()
            .all(|t| t.outcome == "refused" && t.refusal.as_deref() == Some("feed-not-fresh")));
    }

    #[test]
    fn engine_reasons_map_to_typed_kinds() {
        assert_eq!(
            output_refusal_kind("refused:unroutable"),
            "slot-medium-mismatch"
        );
        assert_eq!(
            output_refusal_kind("refused:format-mismatch"),
            "slot-format-unsupported"
        );
        assert_eq!(
            output_refusal_kind("refused:encoder-not-ready"),
            "encoder-not-ready"
        );
        assert_eq!(
            output_refusal_kind("refused:on-air-protected"),
            "on-air-protected"
        );
        assert_eq!(output_refusal_kind("superseded"), "superseded");
        assert_eq!(mixer_refusal_kind("refused:deadline: late"), "deadline");
        assert_eq!(
            mixer_refusal_kind("on-air-protected: input on Program"),
            "on-air-protected"
        );
    }

    #[test]
    fn mixed_ticks_are_never_applied_and_the_deadline_settles_the_rest() {
        let sets = PatchSets::default();
        let op = admitted(&sets, "k", &["output-0"], true);
        let _ = sets.observe(&route(&op, "output-0", 5, "applied"));
        let out = sets.observe(&mixer(&op, 6, None));
        assert_eq!(take(&out)[0].refusal.as_deref(), Some("set-partial"));
        let late = admitted(&sets, "k2", &["output-0"], false);
        assert!(sets.expire(Instant::now()).is_empty(), "not yet");
        let out = sets.expire(Instant::now() + SET_OUTCOME_DEADLINE);
        let out = take(&out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].operation_id, late);
        assert_eq!(out[0].refusal.as_deref(), Some("deadline"));
    }

    #[test]
    fn keys_bind_the_body_and_the_store_is_bounded() {
        let sets = PatchSets::default();
        let SetReservation::Fresh(op) = reserve(&sets, "k", "a", "p") else {
            panic!()
        };
        assert_eq!(
            reserve(&sets, "k", "a", "p"),
            SetReservation::Replay(op.clone())
        );
        assert_eq!(reserve(&sets, "k", "b", "p"), SetReservation::Conflict);
        sets.release("k", &op);
        assert!(matches!(
            reserve(&sets, "k", "b", "p"),
            SetReservation::Fresh(_)
        ));
        // Pending sets are bounded; a full pending map fails closed.
        let sets = PatchSets::default();
        for i in 0..MAX_PENDING_SETS {
            let _ = admitted(&sets, &format!("k{i}"), &["o"], false);
        }
        assert_eq!(reserve(&sets, "over", "d", "q"), SetReservation::Capacity);
        assert_eq!(sets.pending_len(), MAX_PENDING_SETS);
        // A principal's records are capped; settled ones are evicted.
        let sets = PatchSets::default();
        for i in 0..MAX_SET_RECORDS_PER_PRINCIPAL {
            let SetReservation::Fresh(_) = reserve(&sets, &format!("k{i}"), "d", "p") else {
                panic!()
            };
        }
        assert_eq!(
            reserve(&sets, "more", "d", "p"),
            SetReservation::Capacity,
            "all pending"
        );
        // Settled (here by the deadline): the pending slots free and the
        // settled records are evicted for new ones.
        let settled = sets.expire(Instant::now() + SET_OUTCOME_DEADLINE);
        assert_eq!(take(&settled).len(), MAX_SET_RECORDS_PER_PRINCIPAL);
        assert_eq!(sets.pending_len(), 0);
        assert!(matches!(
            reserve(&sets, "more", "d", "p"),
            SetReservation::Fresh(_)
        ));
        assert_eq!(sets.records_len(), MAX_SET_RECORDS_PER_PRINCIPAL);
    }

    /// The pending bound is charged atomically with the key record: at 255
    /// pending, two racing admissions get exactly one slot; a rollback
    /// (`release`) and a settlement each free one again.
    #[test]
    fn the_pending_bound_holds_under_concurrent_admission() {
        use std::sync::{Arc, Barrier};
        let sets = Arc::new(PatchSets::default());
        for i in 0..MAX_PENDING_SETS - 1 {
            let _ = admitted(&sets, &format!("k{i}"), &["o"], false);
        }
        assert_eq!(sets.pending_len(), MAX_PENDING_SETS - 1);
        for round in 0..64 {
            let barrier = Arc::new(Barrier::new(2));
            let racers: Vec<_> = (0..2)
                .map(|n| {
                    let (sets, barrier) = (Arc::clone(&sets), Arc::clone(&barrier));
                    std::thread::spawn(move || {
                        let key = format!("race{round}-{n}");
                        barrier.wait();
                        (key.clone(), reserve(&sets, &key, "d", &format!("q{n}")))
                    })
                })
                .collect();
            let results: Vec<(String, SetReservation)> = racers
                .into_iter()
                .map(|h| h.join().expect("racer"))
                .collect();
            let fresh: Vec<(&String, &OperationId)> = results
                .iter()
                .filter_map(|(k, r)| match r {
                    SetReservation::Fresh(op) => Some((k, op)),
                    _ => None,
                })
                .collect();
            assert_eq!(fresh.len(), 1, "exactly one admission wins: {results:?}");
            assert!(
                results.iter().any(|(_, r)| *r == SetReservation::Capacity),
                "the other fails closed"
            );
            assert_eq!(
                sets.pending_len(),
                MAX_PENDING_SETS,
                "never beyond the bound"
            );
            // The rollback of the winner frees its slot (and its key).
            let (key, op) = fresh[0];
            sets.release(key, op);
            assert_eq!(sets.pending_len(), MAX_PENDING_SETS - 1);
            assert_eq!(sets.peek(key, "d"), None, "key released");
        }
        // A settled set frees its slot too.
        let SetReservation::Fresh(op) = reserve(&sets, "last", "d", "q") else {
            panic!("the freed slot admits");
        };
        assert_eq!(reserve(&sets, "over", "d", "q"), SetReservation::Capacity);
        let out = sets.observe(&route(op.as_str(), "o", 3, "applied"));
        assert_eq!(take(&out).len(), 1);
        assert_eq!(sets.pending_len(), MAX_PENDING_SETS - 1);
        assert!(matches!(
            reserve(&sets, "over", "d", "q"),
            SetReservation::Fresh(_)
        ));
    }

    /// The mixer half's refusal codes use the same typed kinds as an
    /// output member's (§12.8): `format-mismatch` is never published raw.
    #[test]
    fn mixer_refusals_map_to_the_same_typed_kinds() {
        assert_eq!(
            mixer_refusal_kind("refused:format-mismatch"),
            "slot-format-unsupported"
        );
        assert_eq!(
            mixer_refusal_kind("refused:unroutable"),
            "slot-medium-mismatch"
        );
        assert_eq!(
            mixer_refusal_kind("refused:feed-not-fresh"),
            "feed-not-fresh"
        );
        let sets = PatchSets::default();
        let op = admitted(&sets, "k", &["output-0"], true);
        let _ = sets.observe(&mixer(&op, 7, Some("refused:format-mismatch")));
        let out = sets.observe(&route(&op, "output-0", 7, "refused:superseded"));
        let out = take(&out);
        assert_eq!(out.len(), 2);
        assert!(out
            .iter()
            .all(|t| t.refusal.as_deref() == Some("slot-format-unsupported")));
    }
}
