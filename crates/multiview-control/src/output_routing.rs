//! The control-plane seam for **output feed routing** (ADR-0108 slice 1,
//! design `docs/architecture/output-routing.md` §6.2-§6.4).
//!
//! The engine side lives in `multiview-cli` (the bake consumer's router); this
//! crate never depends on it. [`OutputRouting`] is the narrow trait the binary
//! implements over its handle. Outputs are addressed here by their **config
//! id** ([`multiview_config::Output::id`]); the HTTP layer maps the API id the
//! caller addressed onto it from the accepted output resources.
//!
//! Isolation (invariant #10): every call on the trait is non-blocking
//! (`try_take` is a bounded `try_send`, `snapshot` a wait-free load). The
//! outcome relay runs on its own thread + task and never touches the clock.
//! All state here is bounded: the take store (one record per take
//! `Idempotency-Key`, capped in total and per principal; settled records are
//! evicted oldest-first under pressure, pending ones never) and both rate
//! limiters have fixed caps.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use multiview_config::VideoFeed;

/// The picture bus a take routes onto an output (the slice-1 subset of
/// [`VideoFeed`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RoutedFeed {
    /// The Program bus (follows the switcher; the startup encoder).
    Program,
    /// The M/E's Preview bus.
    Preview,
    /// The Program picture before the overlay bake (no labels, no tally).
    Clean,
    /// One source directly (camera, media player, generator).
    Source(String),
}

impl RoutedFeed {
    /// The routed feed of `feed`, or `None` for a layout.
    #[must_use]
    pub fn of(feed: &VideoFeed) -> Option<Self> {
        use multiview_config::OutputBus;
        match feed {
            VideoFeed::Bus(OutputBus::Program) => Some(Self::Program),
            VideoFeed::Bus(OutputBus::Preview) => Some(Self::Preview),
            VideoFeed::Bus(OutputBus::Clean) => Some(Self::Clean),
            VideoFeed::Source(id) => Some(Self::Source(id.clone())),
            _ => None,
        }
    }

    /// The picture-bus name (`program`, `preview`, `clean`, `src:<id>`).
    #[must_use]
    pub fn bus_name(&self) -> String {
        match self {
            Self::Program => "program".to_owned(),
            Self::Preview => "preview".to_owned(),
            Self::Clean => "clean".to_owned(),
            Self::Source(id) => format!("src:{id}"),
        }
    }

    /// Parse a picture-bus name back (`None` for an unknown name).
    #[must_use]
    pub fn from_bus_name(name: &str) -> Option<Self> {
        match name {
            "program" => Some(Self::Program),
            "preview" => Some(Self::Preview),
            "clean" => Some(Self::Clean),
            _ => name
                .strip_prefix("src:")
                .map(|id| Self::Source(id.to_owned())),
        }
    }

    /// The config [`VideoFeed`] this routed feed persists as.
    #[must_use]
    pub fn video_feed(&self) -> VideoFeed {
        match self {
            Self::Program => VideoFeed::PROGRAM,
            Self::Preview => VideoFeed::Bus(multiview_config::OutputBus::Preview),
            Self::Clean => VideoFeed::Bus(multiview_config::OutputBus::Clean),
            Self::Source(id) => VideoFeed::Source(id.clone()),
        }
    }
}

/// One take, already authorized by the HTTP layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputTakeRequest {
    /// The operation id minted for the take (echoed in its outcome).
    pub operation_id: String,
    /// The output's config id.
    pub output: String,
    /// The destination feed.
    pub feed: RoutedFeed,
    /// The authenticated key id the take is charged to (never client input).
    pub principal: String,
}

/// Why a take could not be enqueued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TakeSubmitError {
    /// The engine take mailbox is full or the run ended (503 `engine-busy`).
    Busy,
    /// The engine does not route this output (no run index, or a kind
    /// slice 1 cannot cut cleanly).
    Unroutable,
}

/// A refusal a plan can predict before the take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanRefusal {
    /// The output's kind cannot be routed in this release.
    OutputKindUnroutable,
    /// Preview or Clean requested on a run without an M/E.
    SwitcherRequired,
}

/// The predicted cost of a take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanCost {
    /// `true` when the take needs a new encoder (+1), `false` for +0.
    pub needs_encoder: bool,
    /// Live-opened encoder sessions in use (including opens in flight).
    pub sessions_in_use: usize,
    /// The live-opened encoder session limit.
    pub sessions_max: usize,
}

impl PlanCost {
    /// Whether the take fits the current encoder budget.
    #[must_use]
    pub fn fits(&self) -> bool {
        !self.needs_encoder || self.sessions_in_use < self.sessions_max
    }
}

/// One output's live routing readback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputLive {
    /// The current picture bus name.
    pub bus: String,
    /// The tick its current feed started.
    pub since_tick: u64,
    /// A take in progress: destination bus name and ticks held.
    pub pending: Option<(String, u64)>,
    /// The current bus is holding its last picture beyond the degraded bound.
    pub degraded: bool,
}

/// The engine's routing readback.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoutingLive {
    /// Per output config id.
    pub outputs: HashMap<String, OutputLive>,
    /// Video encoders running (base + live-opened).
    pub encoders: usize,
    /// Live-opened encoder sessions in use and their limit.
    pub sessions_in_use: usize,
    /// The live-opened encoder session limit.
    pub sessions_max: usize,
    /// Clock ticks the routed-picture thread could not take.
    pub dropped_ticks: u64,
}

/// One terminal take outcome from the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputOutcome {
    /// The take's operation id (empty for an unrequested fallback).
    pub operation_id: String,
    /// The output's config id.
    pub output: String,
    /// The tick the outcome was decided.
    pub tick: u64,
    /// `applied` | `superseded` | `refused:*` | `fallback:*`.
    pub outcome: String,
    /// The output's feed after this outcome, when the engine knows it
    /// (always set for `applied`: the feed to persist). Carried here so
    /// persistence never depends on control-plane bookkeeping.
    pub feed: Option<RoutedFeed>,
}

/// The engine seam the binary implements (all calls non-blocking).
pub trait OutputRouting: Send + Sync {
    /// Predict a take's cost and refusals.
    ///
    /// # Errors
    /// The refusal the take would certainly meet.
    fn plan(&self, output: &str, feed: &RoutedFeed) -> Result<PlanCost, PlanRefusal>;
    /// Enqueue an authorized take without waiting.
    ///
    /// # Errors
    /// [`TakeSubmitError`] when it cannot be enqueued.
    fn try_take(&self, take: OutputTakeRequest) -> Result<(), TakeSubmitError>;
    /// The latest readback (`None` before a run published one).
    fn snapshot(&self) -> Option<RoutingLive>;
    /// The outcome stream; `Some` exactly once.
    fn subscribe_outcomes(&self) -> Option<std::sync::mpsc::Receiver<OutputOutcome>>;
}

/// Shared handle to an installed [`OutputRouting`].
pub type SharedOutputRouting = Arc<dyn OutputRouting>;

/// Take records held by the take store (all principals together). At the cap
/// a new distinct key evicts the oldest **settled** record; only when every
/// record is still pending does it fail closed (`idempotency-capacity`). A
/// pending record is never evicted, so a key answered `accepted` always replays
/// its own operation while its outcome is unknown.
pub const MAX_TAKE_RECORDS: usize = crate::concurrency::MAX_IDEMPOTENCY_RESERVATIONS;
/// Take records one principal may hold. At this cap the principal's own oldest
/// settled record is evicted first; a principal whose records are all pending
/// gets `idempotency-capacity` while every other principal is unaffected.
pub const MAX_TAKE_RECORDS_PER_PRINCIPAL: usize = 256;
/// Takes per second per output (§6.4).
pub const TAKES_PER_OUTPUT_PER_SEC: usize = 2;
/// Takes per second per principal (§6.4).
pub const TAKES_PER_PRINCIPAL_PER_SEC: usize = 10;
/// Principals tracked by the limiter (least recently used evicted first).
pub const MAX_LIMITED_PRINCIPALS: usize = 1024;
/// Outputs tracked by the limiter (keys are existing outputs; a hard cap
/// still bounds the map if outputs churn).
pub const MAX_LIMITED_OUTPUTS: usize = 1024;
const RATE_WINDOW: Duration = Duration::from_secs(1);

/// A recorded terminal outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedOutcome {
    /// The outcome string.
    pub outcome: String,
    /// The tick it was decided.
    pub tick: u64,
}

/// What an accepted take needs when its outcome arrives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingTake {
    /// The output's API id (the event and the readback name this).
    pub api_id: String,
    /// The feed to persist on `applied`.
    pub feed: RoutedFeed,
}

/// A take's record for the life of its `Idempotency-Key` (or until it is
/// settled and evicted under pressure).
#[derive(Debug, Clone, PartialEq, Eq)]
struct TakeRecord {
    operation_id: crate::command::OperationId,
    take: PendingTake,
    outcome: Option<RecordedOutcome>,
    /// The principal the record is charged to.
    principal: String,
    /// Reservation order (the eviction age).
    seq: u64,
}

/// The result of [`OutputRoutingControl::reserve_take`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TakeReservation {
    /// The key was unseen: enqueue the take under this new operation id.
    Fresh(crate::command::OperationId),
    /// The key already names a take: answer it, never enqueue again.
    Replay(TakeReplay),
    /// No record can be made: the principal's records (or, globally, every
    /// record) are all pending. Fail closed (`idempotency-capacity`).
    Capacity,
}

/// What a replayed key answers: its own operation and, once known, outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TakeReplay {
    /// The operation the key's first request enqueued.
    pub operation_id: crate::command::OperationId,
    /// Its terminal outcome, once the engine reported it.
    pub outcome: Option<RecordedOutcome>,
}

/// The take-scoped idempotency store: scoped key → record, plus operation id
/// → scoped key for settling. Bounded by `cap` records in total and
/// `per_principal` records per principal; under pressure the oldest SETTLED
/// record is evicted (the principal's own first), never a pending one.
#[derive(Debug)]
struct TakeStore {
    cap: usize,
    per_principal: usize,
    next_seq: u64,
    by_key: HashMap<String, TakeRecord>,
    by_op: HashMap<String, String>,
    /// Records held per principal (an entry is removed at zero, so this map is
    /// bounded by the record count).
    held: HashMap<String, usize>,
    /// Settled records by reservation order: the eviction queue.
    settled: std::collections::BTreeMap<u64, String>,
}

impl TakeStore {
    fn new(cap: usize, per_principal: usize) -> Self {
        Self {
            cap,
            per_principal,
            next_seq: 0,
            by_key: HashMap::new(),
            by_op: HashMap::new(),
            held: HashMap::new(),
            settled: std::collections::BTreeMap::new(),
        }
    }

    fn replay(&self, key: &str) -> Option<TakeReplay> {
        self.by_key.get(key).map(|r| TakeReplay {
            operation_id: r.operation_id.clone(),
            outcome: r.outcome.clone(),
        })
    }

    /// Remove `key`'s record and every index of it.
    fn remove(&mut self, key: &str) {
        let Some(record) = self.by_key.remove(key) else {
            return;
        };
        self.by_op.remove(record.operation_id.as_str());
        self.settled.remove(&record.seq);
        if let Some(count) = self.held.get_mut(&record.principal) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.held.remove(&record.principal);
            }
        }
    }

    /// Evict the oldest settled record (of `principal` only, when given).
    /// `false` when there is none: every candidate is pending.
    fn evict_oldest_settled(&mut self, principal: Option<&str>) -> bool {
        // O(records) at worst, on the control plane only, bounded by `cap`.
        let victim = self
            .settled
            .values()
            .find(|key| {
                principal.is_none_or(|p| self.by_key.get(*key).is_some_and(|r| r.principal == p))
            })
            .cloned();
        match victim {
            Some(key) => {
                self.remove(&key);
                true
            }
            None => false,
        }
    }

    fn reserve(&mut self, key: &str, principal: &str, take: PendingTake) -> TakeReservation {
        if let Some(replay) = self.replay(key) {
            return TakeReservation::Replay(replay);
        }
        if self.held.get(principal).copied().unwrap_or(0) >= self.per_principal
            && !self.evict_oldest_settled(Some(principal))
        {
            return TakeReservation::Capacity;
        }
        if self.by_key.len() >= self.cap && !self.evict_oldest_settled(None) {
            return TakeReservation::Capacity;
        }
        let op = crate::command::OperationId::new();
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.by_op.insert(op.as_str().to_owned(), key.to_owned());
        *self.held.entry(principal.to_owned()).or_insert(0) += 1;
        self.by_key.insert(
            key.to_owned(),
            TakeRecord {
                operation_id: op.clone(),
                take,
                outcome: None,
                principal: principal.to_owned(),
                seq,
            },
        );
        TakeReservation::Fresh(op)
    }

    fn release(&mut self, key: &str, op: &crate::command::OperationId) {
        if self.by_key.get(key).is_some_and(|r| r.operation_id == *op) {
            self.remove(key);
        }
    }

    /// Record `outcome` on `operation_id`'s take; the take it settles.
    fn settle(&mut self, operation_id: &str, outcome: RecordedOutcome) -> Option<PendingTake> {
        let key = self.by_op.get(operation_id)?.clone();
        let record = self.by_key.get_mut(&key)?;
        if record.outcome.is_none() {
            self.settled.insert(record.seq, key);
        }
        record.outcome = Some(outcome);
        Some(record.take.clone())
    }

    fn record(&self, operation_id: &str) -> Option<&TakeRecord> {
        let key = self.by_op.get(operation_id)?;
        self.by_key.get(key)
    }
}

/// A sliding one-second window limiter over a bounded key map.
#[derive(Debug)]
struct WindowLimiter {
    limit: usize,
    cap: usize,
    /// Key → (last use, events in the window).
    keys: HashMap<String, (Instant, VecDeque<Instant>)>,
}

impl WindowLimiter {
    fn new(limit: usize, cap: usize) -> Self {
        Self {
            limit,
            cap,
            keys: HashMap::new(),
        }
    }

    /// `Err(retry_after)` if `key` is over its limit at `now`.
    fn check(&mut self, key: &str, now: Instant) -> Result<(), Duration> {
        if let Some((_, events)) = self.keys.get_mut(key) {
            while events
                .front()
                .is_some_and(|t| now.saturating_duration_since(*t) >= RATE_WINDOW)
            {
                events.pop_front();
            }
            if events.len() >= self.limit {
                let oldest = events.front().copied().unwrap_or(now);
                return Err(RATE_WINDOW.saturating_sub(now.saturating_duration_since(oldest)));
            }
        }
        Ok(())
    }

    fn record(&mut self, key: &str, now: Instant) {
        if !self.keys.contains_key(key) && self.keys.len() >= self.cap {
            // Evict the least recently used key (O(n) over a bounded map,
            // control plane only).
            if let Some(lru) = self
                .keys
                .iter()
                .min_by_key(|(_, (used, _))| *used)
                .map(|(k, _)| k.clone())
            {
                self.keys.remove(&lru);
            }
        }
        let entry = self
            .keys
            .entry(key.to_owned())
            .or_insert_with(|| (now, VecDeque::new()));
        entry.0 = now;
        entry.1.push_back(now);
        while entry.1.len() > self.limit {
            entry.1.pop_front();
        }
    }

    fn len(&self) -> usize {
        self.keys.len()
    }
}

/// Per-output and per-principal take limits (§6.4), both bounded.
#[derive(Debug)]
pub struct TakeLimiter {
    inner: Mutex<(WindowLimiter, WindowLimiter)>,
}

impl Default for TakeLimiter {
    fn default() -> Self {
        Self::new(MAX_LIMITED_OUTPUTS, MAX_LIMITED_PRINCIPALS)
    }
}

impl TakeLimiter {
    /// A limiter tracking at most `outputs` outputs and `principals` principals.
    #[must_use]
    pub fn new(outputs: usize, principals: usize) -> Self {
        Self {
            inner: Mutex::new((
                WindowLimiter::new(TAKES_PER_OUTPUT_PER_SEC, outputs.max(1)),
                WindowLimiter::new(TAKES_PER_PRINCIPAL_PER_SEC, principals.max(1)),
            )),
        }
    }

    /// Admit one take of `principal` on `output` at `now`, or the
    /// `Retry-After` delay. Both limits are checked before either records.
    ///
    /// # Errors
    /// The delay until a slot frees.
    pub fn admit(&self, output: &str, principal: &str, now: Instant) -> Result<(), Duration> {
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let (outputs, principals) = &mut *guard;
        outputs.check(output, now)?;
        principals.check(principal, now)?;
        outputs.record(output, now);
        principals.record(principal, now);
        Ok(())
    }

    /// Tracked (outputs, principals) — the bound is the cap.
    #[must_use]
    pub fn tracked(&self) -> (usize, usize) {
        let guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        (guard.0.len(), guard.1.len())
    }
}

/// The control-plane state of output routing (installed via
/// [`crate::AppState::with_output_routing`]).
pub struct OutputRoutingControl {
    /// The engine seam.
    pub routing: SharedOutputRouting,
    /// The take rate limits.
    pub limiter: TakeLimiter,
    /// The take-scoped idempotency store. It alone owns the replay decision
    /// of `POST /routing/output/take`: the route does not use the
    /// process-global [`crate::concurrency::IdempotencyStore`], so a foreign
    /// `release`/`rebind` of the same raw string there (as switcher,
    /// discovery and command submission do) can never change a take's replay.
    /// Keys arrive already scoped by principal, method and path (the
    /// [`crate::concurrency::IdempotencyKey`] extractor), so two principals'
    /// equal raw keys are two takes.
    records: Mutex<TakeStore>,
}

impl std::fmt::Debug for OutputRoutingControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputRoutingControl")
            .finish_non_exhaustive()
    }
}

impl OutputRoutingControl {
    /// Wrap an engine seam.
    #[must_use]
    pub fn new(routing: SharedOutputRouting) -> Self {
        Self {
            routing,
            limiter: TakeLimiter::default(),
            records: Mutex::new(TakeStore::new(
                MAX_TAKE_RECORDS,
                MAX_TAKE_RECORDS_PER_PRINCIPAL,
            )),
        }
    }

    fn records(&self) -> std::sync::MutexGuard<'_, TakeStore> {
        self.records.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The replay of a take key already reserved, without reserving (lets the
    /// route answer a retry before its rate limits and existence checks).
    #[must_use]
    pub fn replay(&self, key: &str) -> Option<TakeReplay> {
        self.records().replay(key)
    }

    /// Reserve `key` for a take of `pending` charged to `principal` (the
    /// authenticated key id): fresh, replay of the key's own operation, or
    /// capacity. Under pressure the oldest settled record is evicted (the
    /// principal's own first, at its per-principal cap); a pending record never
    /// is. The record exists before the enqueue, so an outcome never beats it.
    pub fn reserve_take(
        &self,
        key: &str,
        principal: &str,
        pending: PendingTake,
    ) -> TakeReservation {
        self.records().reserve(key, principal, pending)
    }

    /// Free `key` after its own take failed to enqueue (busy/unroutable), but
    /// only if it still names `op`.
    pub fn release_take(&self, key: &str, op: &crate::command::OperationId) {
        self.records().release(key, op);
    }

    /// Record a terminal outcome on its take; returns the take it settles,
    /// if it was taken through this control plane.
    pub fn settle(&self, outcome: &OutputOutcome) -> Option<PendingTake> {
        if outcome.operation_id.is_empty() {
            return None;
        }
        self.records().settle(
            &outcome.operation_id,
            RecordedOutcome {
                outcome: outcome.outcome.clone(),
                tick: outcome.tick,
            },
        )
    }

    /// The recorded terminal outcome of `operation_id`, once known.
    #[must_use]
    pub fn outcome_of(&self, operation_id: &str) -> Option<RecordedOutcome> {
        self.records()
            .record(operation_id)
            .and_then(|r| r.outcome.clone())
    }

    /// The API id of a tracked take that has no outcome yet.
    #[must_use]
    pub fn pending_api_id(&self, operation_id: &str) -> Option<String> {
        self.records()
            .record(operation_id)
            .filter(|r| r.outcome.is_none())
            .map(|r| r.take.api_id.clone())
    }

    /// Take records held (bounded by [`MAX_TAKE_RECORDS`]).
    #[must_use]
    pub fn records_len(&self) -> usize {
        self.records().by_key.len()
    }
}

/// Start the outcome relay: one OS thread blocks on the engine's outcome
/// receiver and hands each outcome to a bounded async channel; one task
/// records it, publishes `route.output` and persists an `applied` take. A full
/// async channel blocks only the relay thread, never the engine (whose own
/// outcome channel drops when full).
pub(crate) fn spawn_outcome_relay(state: &crate::AppState, control: &Arc<OutputRoutingControl>) {
    let Some(rx) = control.routing.subscribe_outcomes() else {
        return;
    };
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        tracing::warn!("output routing: no async runtime; take outcomes are not relayed");
        return;
    };
    let (tx, mut async_rx) = tokio::sync::mpsc::channel::<OutputOutcome>(64);
    let spawned = std::thread::Builder::new()
        .name("route-outcomes".to_owned())
        .spawn(move || {
            while let Ok(outcome) = rx.recv() {
                if tx.blocking_send(outcome).is_err() {
                    return;
                }
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "output routing: outcome relay thread did not start");
        return;
    }
    let state = state.clone();
    let control = Arc::clone(control);
    runtime.spawn(async move {
        while let Some(outcome) = async_rx.recv().await {
            relay_one(&state, &control, outcome).await;
        }
    });
}

async fn relay_one(
    state: &crate::AppState,
    control: &OutputRoutingControl,
    outcome: OutputOutcome,
) {
    let settled = control.settle(&outcome);
    let api_id = match &settled {
        Some(pending) => Some(pending.api_id.clone()),
        None => crate::routes::routing::api_id_of(state, &outcome.output),
    };
    let Some(api_id) = api_id else {
        return;
    };
    // The feed to persist rides the outcome itself, so an `applied` persists
    // whether or not this process still holds the take's record.
    if outcome.outcome == "applied" {
        let feed = outcome
            .feed
            .clone()
            .or_else(|| settled.as_ref().map(|p| p.feed.clone()));
        if let Some(feed) = feed {
            if let Err(error) = persist_feed(state, &outcome.output, &feed).await {
                tracing::warn!(%error, output = %api_id, "output routing: applied take not persisted");
            }
        }
    }
    state
        .engine
        .publish_event(multiview_events::Event::RouteOutput(
            multiview_events::RouteOutputEvent {
                output: api_id,
                operation_id: outcome.operation_id,
                tick: outcome.tick,
                outcome: outcome.outcome,
            },
        ));
}

/// Persist `feed` as `output`'s `[[routing.output]]` entry (Program removes
/// it, unless the boot file routes that output elsewhere: then an explicit
/// Program row is kept so a restart does not re-seed the boot row), under the
/// config-mutation lock `apply-layout` uses. Called only after the engine
/// reported `applied`. Start-up carries these rows (store-backed, #329).
///
/// # Errors
/// The accepted document cannot be read, validated or committed.
pub async fn persist_feed(
    state: &crate::AppState,
    output: &str,
    feed: &RoutedFeed,
) -> crate::ControlResult<()> {
    if state.boot_model.is_none() {
        return Ok(());
    }
    let _mutation = state.lock_config_mutation().await;
    let mut config: multiview_config::MultiviewConfig =
        serde_json::from_value(crate::boot_model::current_desired_config(state)?)
            .map_err(|e| crate::ControlError::Repository(format!("desired configuration: {e}")))?;
    let program = config
        .programs
        .first()
        .map_or(multiview_config::MAIN_PROGRAM, |p| p.id.as_str())
        .to_owned();
    // Only the output rows change: an absent table stays free of desugared
    // cell/audio rows (a later layout edit must not meet stale crosspoints).
    let mut table = config.routing.take().unwrap_or_default();
    table.output.retain(|xp| xp.output != output);
    // A start-up seeds an output with no accepted row from the BOOT file's
    // row (#329), so a take back to Program must stay explicit when the boot
    // file routes that output elsewhere — or a restart would undo it.
    let boot_seeds_other = state.boot_model.as_ref().is_some_and(|model| {
        model.loaded().routing.as_ref().is_some_and(|t| {
            t.output.iter().any(|xp| {
                xp.output == output && xp.effective_video() != multiview_config::VideoFeed::PROGRAM
            })
        })
    });
    if *feed != RoutedFeed::Program || boot_seeds_other {
        let mut xp = multiview_config::OutputCrosspoint::program(output, program);
        xp.video = Some(feed.video_feed());
        table.output.push(xp);
    }
    // Otherwise Program removes the entry (absent = Program, today's
    // behaviour).
    let empty = table.output.is_empty()
        && table.video.is_empty()
        && table.audio.is_empty()
        && table.subtitle.is_empty();
    config.routing = (!empty).then_some(table);
    config
        .validate()
        .map_err(|e| crate::ControlError::Validation(e.to_string()))?;
    let layouts = crate::boot_model::layout_library(state)?;
    crate::boot_model::commit_desired(state, config, layouts).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_principal_state_is_bounded_by_the_lru_cap() {
        let limiter = TakeLimiter::new(4, 16);
        let now = Instant::now();
        for i in 0..1000 {
            let _ = limiter.admit("out", &format!("p{i}"), now + Duration::from_secs(i));
        }
        let (outputs, principals) = limiter.tracked();
        assert_eq!(principals, 16, "the principal map never exceeds its cap");
        assert_eq!(outputs, 1);
        for i in 0..1000 {
            let _ = limiter.admit(&format!("o{i}"), "p", now + Duration::from_secs(2000 + i));
        }
        assert_eq!(
            limiter.tracked().0,
            4,
            "the output map never exceeds its cap"
        );
    }

    #[test]
    fn two_per_output_and_ten_per_principal_per_second() {
        let limiter = TakeLimiter::default();
        let t0 = Instant::now();
        assert!(limiter.admit("a", "p", t0).is_ok());
        assert!(limiter.admit("a", "p", t0).is_ok());
        let retry = limiter.admit("a", "p", t0).unwrap_err();
        assert!(retry <= RATE_WINDOW && retry > Duration::ZERO);
        assert!(limiter.admit("a", "p", t0 + RATE_WINDOW).is_ok());
        let t1 = t0 + Duration::from_secs(10);
        for i in 0..10 {
            assert!(limiter.admit(&format!("x{i}"), "q", t1).is_ok());
        }
        assert!(limiter.admit("y", "q", t1).is_err());
    }

    #[test]
    fn take_records_are_bounded_and_fail_closed_without_eviction() {
        let mut store = TakeStore::new(3, 3);
        let pending = || PendingTake {
            api_id: "o".to_owned(),
            feed: RoutedFeed::Program,
        };
        let mut ops = Vec::new();
        for i in 0..3 {
            let TakeReservation::Fresh(op) = store.reserve(&format!("k{i}"), "p", pending()) else {
                panic!("fresh");
            };
            ops.push(op);
        }
        assert_eq!(
            store.reserve("k9", "p", pending()),
            TakeReservation::Capacity
        );
        assert_eq!(store.by_key.len(), 3);
        for (i, op) in ops.iter().enumerate() {
            let Some(replay) = store.replay(&format!("k{i}")) else {
                panic!("k{i} evicted");
            };
            assert_eq!(&replay.operation_id, op);
        }
        // A release with a stale op is a no-op; the owner's frees the key.
        store.release("k0", &crate::command::OperationId::new());
        assert!(store.replay("k0").is_some());
        store.release("k0", &ops[0]);
        assert!(store.replay("k0").is_none() && store.record(ops[0].as_str()).is_none());
        assert!(matches!(
            store.reserve("k9", "p", pending()),
            TakeReservation::Fresh(_)
        ));
    }

    fn pending_take() -> PendingTake {
        PendingTake {
            api_id: "o".to_owned(),
            feed: RoutedFeed::Program,
        }
    }

    fn fresh(store: &mut TakeStore, key: &str, principal: &str) -> crate::command::OperationId {
        match store.reserve(key, principal, pending_take()) {
            TakeReservation::Fresh(op) => op,
            other => panic!("{key}: expected fresh, got {other:?}"),
        }
    }

    fn settle(store: &mut TakeStore, op: &crate::command::OperationId) {
        assert!(store
            .settle(
                op.as_str(),
                RecordedOutcome {
                    outcome: "applied".to_owned(),
                    tick: 1,
                },
            )
            .is_some());
    }

    #[test]
    fn one_principal_filling_its_cap_cannot_block_another() {
        let mut store = TakeStore::new(100, 4);
        for i in 0..4 {
            fresh(&mut store, &format!("a{i}"), "alice");
        }
        // All of alice's records are pending: alice fails closed …
        assert_eq!(
            store.reserve("a-more", "alice", pending_take()),
            TakeReservation::Capacity
        );
        // … and bob is unaffected.
        fresh(&mut store, "b0", "bob");
        // Every alice key still replays its own operation while pending.
        for i in 0..4 {
            assert!(store.replay(&format!("a{i}")).is_some());
        }
    }

    #[test]
    fn a_principal_at_its_cap_evicts_its_own_oldest_settled_record_first() {
        let mut store = TakeStore::new(100, 3);
        let bob = fresh(&mut store, "b0", "bob");
        settle(&mut store, &bob);
        let a0 = fresh(&mut store, "a0", "alice");
        let a1 = fresh(&mut store, "a1", "alice");
        fresh(&mut store, "a2", "alice");
        settle(&mut store, &a1);
        settle(&mut store, &a0);
        // Alice is at her cap: her OWN oldest settled (a0, reserved first) is
        // evicted, not bob's older settled record and not her pending a2.
        fresh(&mut store, "a3", "alice");
        assert!(
            store.replay("a0").is_none(),
            "alice's oldest settled evicted"
        );
        assert!(store.replay("a1").is_some());
        assert!(store.replay("a2").is_some(), "pending never evicted");
        assert!(store.replay("b0").is_some(), "another principal untouched");
        assert_eq!(store.by_key.len(), 4);
    }

    #[test]
    fn settled_records_are_evicted_oldest_first_at_the_global_cap_and_pending_never() {
        let mut store = TakeStore::new(4, 4);
        let k0 = fresh(&mut store, "k0", "p0");
        fresh(&mut store, "k1", "p1");
        let k2 = fresh(&mut store, "k2", "p2");
        fresh(&mut store, "k3", "p3");
        settle(&mut store, &k2);
        settle(&mut store, &k0);
        // Full: the oldest SETTLED by reservation order (k0) goes first.
        fresh(&mut store, "k4", "p4");
        assert!(store.replay("k0").is_none());
        assert!(store.record(k0.as_str()).is_none(), "both indexes cleared");
        fresh(&mut store, "k5", "p5");
        assert!(store.replay("k2").is_none());
        // Only pending records remain: fail closed, nothing evicted.
        assert_eq!(
            store.reserve("k6", "p6", pending_take()),
            TakeReservation::Capacity
        );
        for key in ["k1", "k3", "k4", "k5"] {
            assert!(store.replay(key).is_some(), "{key} pending, kept");
        }
        // A replay of an evicted key is a new take (the documented trade-off).
        assert_eq!(store.by_key.len(), 4);
        assert!(store.held.len() <= 4, "per-principal counts are bounded");
    }

    #[test]
    fn bus_names_round_trip() {
        for feed in [
            RoutedFeed::Program,
            RoutedFeed::Preview,
            RoutedFeed::Clean,
            RoutedFeed::Source("bars".into()),
        ] {
            assert_eq!(RoutedFeed::from_bus_name(&feed.bus_name()), Some(feed));
        }
    }

    #[test]
    fn clean_is_a_routed_picture_bus() {
        let clean = VideoFeed::Bus(multiview_config::OutputBus::Clean);
        assert_eq!(RoutedFeed::of(&clean), Some(RoutedFeed::Clean));
        assert_eq!(RoutedFeed::Clean.bus_name(), "clean");
        assert_eq!(RoutedFeed::Clean.video_feed(), clean);
    }
}
