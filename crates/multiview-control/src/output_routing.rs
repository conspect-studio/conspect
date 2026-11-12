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
//! All state here is bounded: the take-record map (one record per reserved
//! `Idempotency-Key`, same cap and same lifetime as the key) and both rate
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
    /// One source directly (camera, media player, generator).
    Source(String),
}

impl RoutedFeed {
    /// The slice-1 routed feed of `feed`, or `None` for Clean / a layout.
    #[must_use]
    pub fn of(feed: &VideoFeed) -> Option<Self> {
        use multiview_config::OutputBus;
        match feed {
            VideoFeed::Bus(OutputBus::Program) => Some(Self::Program),
            VideoFeed::Bus(OutputBus::Preview) => Some(Self::Preview),
            VideoFeed::Source(id) => Some(Self::Source(id.clone())),
            _ => None,
        }
    }

    /// The picture-bus name (`program`, `preview`, `src:<id>`).
    #[must_use]
    pub fn bus_name(&self) -> String {
        match self {
            Self::Program => "program".to_owned(),
            Self::Preview => "preview".to_owned(),
            Self::Source(id) => format!("src:{id}"),
        }
    }

    /// Parse a picture-bus name back (`None` for an unknown name).
    #[must_use]
    pub fn from_bus_name(name: &str) -> Option<Self> {
        match name {
            "program" => Some(Self::Program),
            "preview" => Some(Self::Preview),
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
    /// Preview requested on a run without an M/E.
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

/// Take records kept beside the idempotency store: the same cap, and a record
/// is created only with a fresh key reservation and removed only with that
/// key's release, so a record can never be evicted while its key replays.
pub const MAX_TAKE_RECORDS: usize = crate::concurrency::MAX_IDEMPOTENCY_RESERVATIONS;
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

/// A take's record for the life of its `Idempotency-Key`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TakeRecord {
    take: PendingTake,
    outcome: Option<RecordedOutcome>,
}

/// A bounded, insertion-ordered map (oldest evicted first).
#[derive(Debug)]
struct BoundedMap<V> {
    cap: usize,
    map: HashMap<String, V>,
    order: VecDeque<String>,
}

impl<V> BoundedMap<V> {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn insert(&mut self, key: String, value: V) {
        if self.map.insert(key.clone(), value).is_none() {
            self.order.push_back(key);
        }
        while self.map.len() > self.cap {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.map.remove(&oldest);
        }
    }

    fn remove(&mut self, key: &str) -> Option<V> {
        let value = self.map.remove(key)?;
        self.order.retain(|k| k != key);
        Some(value)
    }

    fn get(&self, key: &str) -> Option<&V> {
        self.map.get(key)
    }

    fn get_mut(&mut self, key: &str) -> Option<&mut V> {
        self.map.get_mut(key)
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
    /// Operation id → take record. Inserted only after a fresh key
    /// reservation and removed only with that key's release; the idempotency
    /// store never holds more than [`MAX_TAKE_RECORDS`] keys, so the bound
    /// below is never reached by eviction (it stays as a backstop).
    records: Mutex<BoundedMap<TakeRecord>>,
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
            records: Mutex::new(BoundedMap::new(MAX_TAKE_RECORDS)),
        }
    }

    fn records(&self) -> std::sync::MutexGuard<'_, BoundedMap<TakeRecord>> {
        self.records.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Record an accepted take for the life of its reserved key. Call only
    /// after a fresh reservation of that key.
    pub fn track(&self, operation_id: &str, pending: PendingTake) {
        self.records().insert(
            operation_id.to_owned(),
            TakeRecord {
                take: pending,
                outcome: None,
            },
        );
    }

    /// Forget a take that never reached the engine (with its key's release).
    pub fn untrack(&self, operation_id: &str) {
        self.records().remove(operation_id);
    }

    /// Record a terminal outcome on its take; returns the take it settles,
    /// if it was taken through this control plane.
    pub fn settle(&self, outcome: &OutputOutcome) -> Option<PendingTake> {
        if outcome.operation_id.is_empty() {
            return None;
        }
        let mut records = self.records();
        let record = records.get_mut(&outcome.operation_id)?;
        record.outcome = Some(RecordedOutcome {
            outcome: outcome.outcome.clone(),
            tick: outcome.tick,
        });
        Some(record.take.clone())
    }

    /// The recorded terminal outcome of `operation_id`, once known.
    #[must_use]
    pub fn outcome_of(&self, operation_id: &str) -> Option<RecordedOutcome> {
        self.records()
            .get(operation_id)
            .and_then(|r| r.outcome.clone())
    }

    /// The API id of a tracked take that has no outcome yet.
    #[must_use]
    pub fn pending_api_id(&self, operation_id: &str) -> Option<String> {
        self.records()
            .get(operation_id)
            .filter(|r| r.outcome.is_none())
            .map(|r| r.take.api_id.clone())
    }

    /// Take records held (bounded by [`MAX_TAKE_RECORDS`]).
    #[must_use]
    pub fn records_len(&self) -> usize {
        self.records().map.len()
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
/// it), under the config-mutation lock `apply-layout` uses. Called only after
/// the engine reported `applied`.
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
    if *feed != RoutedFeed::Program {
        let mut xp = multiview_config::OutputCrosspoint::program(output, program);
        xp.video = Some(feed.video_feed());
        table.output.push(xp);
    }
    // Program: the entry is removed (absent = Program, today's behaviour).
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
    fn recorded_outcomes_are_bounded() {
        let mut map = BoundedMap::new(3);
        for i in 0..10 {
            map.insert(format!("k{i}"), i);
        }
        assert_eq!(map.map.len(), 3);
        assert!(map.get("k9").is_some() && map.get("k0").is_none());
    }

    #[test]
    fn bus_names_round_trip() {
        for feed in [
            RoutedFeed::Program,
            RoutedFeed::Preview,
            RoutedFeed::Source("bars".into()),
        ] {
            assert_eq!(RoutedFeed::from_bus_name(&feed.bus_name()), Some(feed));
        }
    }
}
