//! Realtime fan-out: WebSocket (primary) and SSE (one-way fallback).
//!
//! The realtime layer turns the engine's drop-oldest event broadcast
//! (`EnginePublisher::events`) into a per-client stream of
//! [`Envelope<Event>`](multiview_events::Envelope) frames, following the
//! snapshot-then-delta + resume-by-seq model (ADR-RT002/RT003):
//!
//! 1. On connect the client receives a **snapshot** frame establishing the
//!    `$control` baseline (and resume cursor).
//! 2. Then a stream of **delta** frames, one per engine event.
//! 3. A reconnecting client may present a `since_seq` resume cursor; the stream
//!    replays only events strictly after it (best-effort, bounded by the
//!    broadcast ring) and otherwise issues a fresh snapshot.
//!
//! **Isolation (invariant #10) is the load-bearing property.** The reader pulls
//! from the broadcast with [`EventSubscription::recv`]; a slow client that falls
//! behind observes [`RecvError::Lagged`] and the reader **skips/resubscribes**
//! (lagged-skip) rather than ever applying back-pressure. The engine's publish
//! path is a non-blocking `broadcast::send`, so no client — slow, stalled, or
//! malicious — can stall the engine. The client-facing socket write is the only
//! thing that can block, and it blocks only this client's task, never the
//! engine.
use std::collections::HashMap;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Query, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use multiview_config::Origin;
use multiview_core::time::MediaTime;
use multiview_engine::{EventSubscription, RecvError, SeqEvent, TryRecvError};
use multiview_events::{
    AuthzScope, DeviceStatus, Envelope, Event, FrameKind, Hello, MediaPlayerState, OutputRunState,
    Resync, ResyncReason, SalvoPhase, SchemaVersion, Seq, TileSnapshotEntry, TilesSnapshot, Topic,
};

use crate::auth::{scope_permits, Action, ApiKeyStore, AuthzScopes, Principal, Role};
use crate::command::{Command, MediaTransportVerb, OperationId};
use crate::devices::DeviceStatusRegistry;
use crate::state::{AppState, EngineStateSnapshot};

/// How often an otherwise-idle realtime session re-samples the authorization
/// generation (ADR-RT010). On an active stream re-authorization is effectively
/// immediate (checked before projecting every delta); this timer bounds the
/// worst-case revocation latency on a stream with no traffic. Tunable against the
/// per-session idle-wakeup cost.
const REAUTH_TICK: Duration = Duration::from_secs(5);

/// The WebSocket close code for a forbidden/revoked authorization (RFC 6455
/// private-use range; reserved as "forbidden scope" by ADR-RT005 §12). Sent when a
/// live session's principal loses read access mid-session (ADR-RT010).
const WS_CLOSE_FORBIDDEN: u16 = 4403;

/// The session heartbeat interval advertised in `$hello`.
const HEARTBEAT_MS: u32 = 15_000;
/// The minimum clamped wire cadence advertised in `$hello`.
const MIN_RATE_HZ: u32 = 1;
/// The maximum clamped wire cadence advertised in `$hello`.
const MAX_RATE_HZ: u32 = 60;
/// The default wire cadence advertised in `$hello`.
const DEFAULT_RATE_HZ: u32 = 30;

/// The correlation key that links an accepted command to its eventual outcome
/// event on the realtime stream (ADR-W008).
///
/// A command is 202'd with an [`OperationId`]; the engine later publishes an
/// outcome [`Event`] that does **not** itself carry the op id. This key is the
/// stable bridge: it is derived identically from the *command* (at 202 time,
/// [`CorrKey::for_command`]) and from the matching *outcome event* (at
/// projection time, [`CorrKey::for_event`]), so the realtime layer can recover
/// the op id and stamp it onto the outcome envelope's `corr` — **without**
/// adding an op id to the [`Event`] enum or touching the engine hot loop.
///
/// Only commands with a single, unambiguous outcome event are keyed here
/// (start/stop and named-salvo arm/take/cancel). A command with no realtime
/// outcome event (e.g. `SwapSource`, whose outcome is the layout change) or an
/// ambiguous one (`TakeSalvo`/`CancelSalvo` of the *armed* salvo, whose name is
/// not known until the engine resolves it) yields [`None`] and is simply not
/// correlated on the wire — the [`Envelope::corr`] stays `None`, which is the
/// honest "uncorrelated" state, never a wrong id.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CorrKey {
    /// An output run-state transition (the outcome of `Start`/`Stop`).
    OutputState(OutputRunState),
    /// A named salvo entering a lifecycle phase (the outcome of a named
    /// `ArmSalvo`/`TakeSalvo`/`CancelSalvo`).
    Salvo {
        /// The salvo name (must match the outcome `SalvoEvent::salvo`).
        salvo: String,
        /// The lifecycle phase the salvo transitions into.
        phase: SalvoPhase,
    },
    /// A media player transitioning into a discrete transport state (the
    /// outcome of a `MediaTransport`/`ArmMediaExit`/`TakeMediaExit`/
    /// `CancelMediaExit` whose target state is unambiguous; ADR-0097 §6,
    /// ADR-RT008). Mirrors [`CorrKey::Salvo`]: the key is derived identically
    /// from the command (at 202 time) and the matching `media.player_state`
    /// outcome event, so the op id never enters the `Event` enum.
    /// `MediaPlayerState` is `Eq + Hash`, so the `CorrKey` derive holds.
    MediaPlayer {
        /// The media-player id (must match the outcome `MediaPlayerEvent::player`).
        player: String,
        /// The transport state the player transitions into.
        state: MediaPlayerState,
    },
    /// The running discovery scan's `device.discovered` rows (ADR-RT007):
    /// a **windowed**, multi-event correlation — every row one scan publishes
    /// echoes that scan's operation id. Recorded via
    /// [`CorrRegistry::record_window`] (never the consume-once
    /// [`record`](CorrRegistry::record)); the scan single-flight gate
    /// guarantees at most one running scan, so the unit key is unambiguous,
    /// and the window's seq fence keeps an earlier scan's stragglers from ever
    /// being stamped with a newer scan's id.
    Discovery,
}

impl CorrKey {
    /// The correlation key a command's eventual outcome event will carry, or
    /// [`None`] when the command has no single unambiguous realtime outcome to
    /// correlate (so it is left uncorrelated rather than mis-correlated).
    ///
    /// * `Start` → `OutputState(Running)`, `Stop` → `OutputState(Idle)` — the
    ///   `OutputStatus` echo the drain emits.
    /// * `ArmSalvo`/`TakeSalvo`/`CancelSalvo` of a **named** salvo →
    ///   `Salvo { salvo, phase }`. A salvo `None` (take/cancel the *armed*
    ///   salvo) is not keyed: the outcome's name is resolved engine-side and is
    ///   not known here.
    /// * `SwapSource`/`ApplyLayout`/`SetTallyOverride` → [`None`]: a swap has
    ///   no dedicated outcome event; a stored-layout apply emits a
    ///   `job.progress` outcome (ADR-W019) but that phase string is not a
    ///   per-operation key (two applies of the same layout are
    ///   indistinguishable), so it rides uncorrelated rather than
    ///   mis-correlated; and the tally echo is not a command-acknowledgement
    ///   outcome.
    #[must_use]
    pub fn for_command(command: &Command) -> Option<Self> {
        match command {
            Command::Start { .. } => Some(Self::OutputState(OutputRunState::Running)),
            Command::Stop { .. } => Some(Self::OutputState(OutputRunState::Idle)),
            Command::ArmSalvo { salvo, .. } => Some(Self::Salvo {
                salvo: salvo.clone(),
                phase: SalvoPhase::Armed,
            }),
            Command::TakeSalvo {
                salvo: Some(salvo), ..
            } => Some(Self::Salvo {
                salvo: salvo.clone(),
                phase: SalvoPhase::Taken,
            }),
            Command::CancelSalvo {
                salvo: Some(salvo), ..
            } => Some(Self::Salvo {
                salvo: salvo.clone(),
                phase: SalvoPhase::Cancelled,
            }),
            // Media vamp-exit triad (ADR-0097 §3/§6), mirroring the salvo triad:
            // arm/take stage the transition to `Vamping { exit_armed: true }`
            // (take is functionally arm-then-soonest-boundary), cancel unsets it.
            // Both project to the same `media.player_state` outcome.
            Command::ArmMediaExit { player, .. } | Command::TakeMediaExit { player, .. } => {
                Some(Self::MediaPlayer {
                    player: player.clone(),
                    state: MediaPlayerState::Vamping { exit_armed: true },
                })
            }
            Command::CancelMediaExit { player, .. } => Some(Self::MediaPlayer {
                player: player.clone(),
                state: MediaPlayerState::Vamping { exit_armed: false },
            }),
            // Media transport: only verbs with a single unambiguous outcome state
            // are keyed. play→Playing, pause→Paused, stop→Stopped, cue→Cued.
            // `load` may resolve to Loading then Cued (ambiguous) and `seek`
            // leaves the player in its current state — both fall through to None
            // rather than mis-correlate (ADR-0097 §6 "single unambiguous outcome").
            Command::MediaTransport { player, verb, .. } => match verb {
                MediaTransportVerb::Play => Some(Self::MediaPlayer {
                    player: player.clone(),
                    state: MediaPlayerState::Playing,
                }),
                MediaTransportVerb::Pause => Some(Self::MediaPlayer {
                    player: player.clone(),
                    state: MediaPlayerState::Paused,
                }),
                MediaTransportVerb::Stop => Some(Self::MediaPlayer {
                    player: player.clone(),
                    state: MediaPlayerState::Stopped,
                }),
                MediaTransportVerb::Cue { .. } => Some(Self::MediaPlayer {
                    player: player.clone(),
                    state: MediaPlayerState::Cued,
                }),
                MediaTransportVerb::Load { .. } | MediaTransportVerb::Seek { .. } => None,
            },
            // Take/cancel of the *armed* salvo: the name is resolved engine-side
            // and unknown here, so it is left uncorrelated rather than guessed.
            Command::TakeSalvo { salvo: None, .. }
            | Command::CancelSalvo { salvo: None, .. }
            // No dedicated realtime outcome event to correlate. The per-stream
            // route commands (RT-11) apply at the frame boundary with no single
            // unambiguous outcome event yet (the change rides the conflated
            // snapshot), so they are left uncorrelated rather than mis-correlated.
            | Command::SwapSource { .. }
            | Command::RouteVideo { .. }
            | Command::RouteAudio { .. }
            | Command::RouteSubtitle { .. }
            | Command::ApplyLayout { .. }
            // Live source upsert/remove (ADR-W018): the observable outcome is
            // the tile state machine itself (`tile.state` events + the conflated
            // snapshot), not a dedicated ack event — left uncorrelated rather
            // than mis-correlated.
            | Command::UpsertSource { .. }
            | Command::RemoveSource { .. }
            // Live overlay upsert/remove (ADR-W022): the drain emits a
            // `job.progress` outcome whose phase string is not a per-operation
            // key (two applies of the same overlay are indistinguishable) —
            // left uncorrelated rather than mis-correlated, like ApplyLayout.
            | Command::UpsertOverlay { .. }
            | Command::RemoveOverlay { .. }
            // Overlay reorder (task #130): like the overlay upsert/remove above,
            // the drain emits a `job.progress` outcome with no per-operation key,
            // so it is left uncorrelated rather than mis-correlated.
            | Command::ReorderOverlays { .. }
            | Command::SetTallyOverride { .. } => None,
        }
    }

    /// The correlation key an outcome event carries, or [`None`] when the event
    /// is not a command outcome (so it is never stamped with a stale `corr`).
    ///
    /// Mirrors [`CorrKey::for_command`]: only `OutputStatus`, the named salvo
    /// arm/take/cancel events, and the windowed `device.discovered` rows map
    /// to a key; every other event — tile state, alerts, audio meters, the
    /// control frames — yields [`None`].
    #[must_use]
    pub fn for_event(event: &Event) -> Option<Self> {
        match event {
            Event::OutputStatus(status) => Some(Self::OutputState(status.state)),
            Event::SalvoArmed(e) => Some(Self::Salvo {
                salvo: e.salvo.clone(),
                phase: SalvoPhase::Armed,
            }),
            Event::SalvoTaken(e) => Some(Self::Salvo {
                salvo: e.salvo.clone(),
                phase: SalvoPhase::Taken,
            }),
            Event::SalvoCancelled(e) => Some(Self::Salvo {
                salvo: e.salvo.clone(),
                phase: SalvoPhase::Cancelled,
            }),
            // The running scan's rows correlate via the windowed Discovery key
            // (recorded at scan start, seq-fenced).
            Event::DeviceDiscovered(_) => Some(Self::Discovery),
            // A media-player lifecycle transition projects to the same key its
            // originating command produced (ADR-0097 §6, ADR-RT008): the player
            // id + the discrete transport state it entered.
            Event::MediaPlayerState(e) => Some(Self::MediaPlayer {
                player: e.player.clone(),
                state: e.state,
            }),
            _ => None,
        }
    }
}

/// A bounded, control-plane-only registry pairing an accepted command with the
/// [`OperationId`] its outcome event must echo as `corr` (ADR-W008).
///
/// The command surface records `(key, op)` at 202 time ([`CorrRegistry::record`]);
/// the realtime projection resolves it when the matching outcome event is
/// delivered ([`CorrRegistry::resolve`]), memoized **once per engine sequence
/// number** so every fanned-out subscriber stamps the same corr while a
/// re-emitted outcome (a different seq) carries no stale corr. Multi-event
/// operations (the discovery scan's `device.discovered` rows) use the
/// **windowed** lane instead ([`CorrRegistry::record_window`]): every matching
/// event after the window's seq fence echoes the op, non-consuming.
///
/// **Isolation (invariant #10).** This is ordinary control-plane state behind a
/// short-held `Mutex` that the engine never touches: `record` runs on the HTTP
/// 202 path and `take` on the per-client realtime projection — neither is on the
/// engine hot loop, and the engine's publish path (`broadcast::send`) never
/// locks this. It is **bounded**: at most `capacity` pending correlations are
/// retained; recording over capacity drops the oldest pending entry
/// (drop-oldest), so a flood of un-consumed correlations can never grow memory
/// without bound. A dropped correlation simply leaves its outcome uncorrelated
/// (`corr: None`) — acceptable, never a wrong id.
#[derive(Debug)]
pub struct CorrRegistry {
    inner: Mutex<CorrInner>,
    /// The maximum number of pending correlations retained at once.
    capacity: usize,
}

/// The mutex-guarded interior of [`CorrRegistry`].
#[derive(Debug, Default)]
struct CorrInner {
    /// FIFO queues of pending op ids keyed by their outcome key.
    pending: HashMap<CorrKey, VecDeque<OperationId>>,
    /// Drop-oldest insertion order across all keys, so the global bound evicts
    /// the oldest pending correlation regardless of key.
    order: VecDeque<CorrKey>,
    /// **Windowed** (multi-event) correlations: every outcome event matching
    /// the key whose engine seq is **after** the window's fence echoes the
    /// window's op — until a newer window for the same key replaces it.
    /// Bounded by construction: windows are keyed by code-defined [`CorrKey`]
    /// variants (today only [`CorrKey::Discovery`]) and re-recording a key
    /// overwrites its entry, so this map can never grow with traffic.
    windows: HashMap<CorrKey, CorrWindow>,
    /// Op ids already resolved for a specific outcome **engine seq**. The first
    /// client to project an outcome pops the pending op (or reads the window)
    /// and memoizes it here, so every other client projecting the SAME engine
    /// event (the realtime stream fans one engine event out to all subscribers)
    /// stamps the SAME `corr` rather than only the first reader seeing it.
    /// Bounded the same way as `order` (one resolved entry per consumed
    /// correlation).
    resolved: HashMap<u64, OperationId>,
    /// Insertion order of `resolved` engine seqs, so the global bound evicts the
    /// oldest resolved correlation alongside the pending ones.
    resolved_order: VecDeque<u64>,
}

/// One windowed (multi-event) correlation: the op id stamped on every matching
/// outcome event published **after** `from_seq`.
#[derive(Debug)]
struct CorrWindow {
    /// The operation id the window's events echo as `corr`.
    op: OperationId,
    /// The engine sequence number at the moment the window opened: only events
    /// with `seq > from_seq` belong to it. An earlier window's stragglers
    /// therefore resolve to nothing (honest "uncorrelated"), never to a newer
    /// operation's id.
    from_seq: u64,
}

impl CorrInner {
    /// Memoize a resolved `(engine_seq → op)` pair so every subscriber stamps
    /// the same `corr` for one outcome event, evicting the oldest memo beyond
    /// `capacity` (drop-oldest, invariant #10).
    fn memoize_resolved(&mut self, engine_seq: u64, op: OperationId, capacity: usize) {
        self.resolved.insert(engine_seq, op);
        self.resolved_order.push_back(engine_seq);
        while self.resolved_order.len() > capacity {
            if let Some(evicted) = self.resolved_order.pop_front() {
                self.resolved.remove(&evicted);
            }
        }
    }
}

impl CorrRegistry {
    /// Build a registry retaining at most `capacity` pending **and** `capacity`
    /// resolved correlations (drop-oldest beyond that). A `capacity` of `0` is
    /// promoted to `1`.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(CorrInner::default()),
            capacity: capacity.max(1),
        }
    }

    /// Record that the outcome event matching `key` should echo `op` as `corr`.
    ///
    /// Non-blocking and bounded: holds the lock only to push, and evicts the
    /// oldest pending correlation when over `capacity` (drop-oldest, invariant
    /// #10). A poisoned lock is treated as "registry unavailable" and the record
    /// is skipped — the outcome stays uncorrelated rather than the call panicking
    /// on the 202 path.
    pub fn record(&self, key: CorrKey, op: OperationId) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        inner.pending.entry(key.clone()).or_default().push_back(op);
        inner.order.push_back(key);
        // Enforce the global bound: evict the oldest pending correlation.
        while inner.order.len() > self.capacity {
            if let Some(evicted) = inner.order.pop_front() {
                if let Some(queue) = inner.pending.get_mut(&evicted) {
                    queue.pop_front();
                    if queue.is_empty() {
                        inner.pending.remove(&evicted);
                    }
                }
            }
        }
    }

    /// Open (or replace) a **windowed** correlation: every outcome event
    /// matching `key` published with an engine sequence **after** `from_seq`
    /// echoes `op` as `corr`, until a newer window for the same key replaces
    /// this one. Multi-event-per-operation surfaces use this — the discovery
    /// scan's `device.discovered` rows ([`CorrKey::Discovery`]) — where
    /// [`record`](Self::record)'s consume-once pairing cannot.
    ///
    /// `from_seq` is the publisher's sequence at the moment the operation
    /// started, so an *earlier* operation's stragglers (seq ≤ `from_seq`)
    /// resolve to [`None`] — honest "uncorrelated", never a wrong id. Bounded
    /// by construction (one window per code-defined key, overwritten on
    /// re-record; invariant #10). A poisoned lock skips the record — the
    /// operation's outcomes ride uncorrelated rather than the 202 path
    /// panicking.
    pub fn record_window(&self, key: CorrKey, op: OperationId, from_seq: u64) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        inner.windows.insert(key, CorrWindow { op, from_seq });
    }

    /// Resolve the op id to stamp as `corr` on the outcome event with engine
    /// sequence `engine_seq` and correlation `key`, or [`None`] when nothing
    /// correlates (then the outcome rides uncorrelated).
    ///
    /// **Resolve-once-per-engine-seq.** The first caller for a given `engine_seq`
    /// pops the oldest pending op under `key` (FIFO) and memoizes it against that
    /// seq; every subsequent caller for the same `engine_seq` (the other realtime
    /// clients, which each project the same fanned-out engine event) returns the
    /// memoized op — so all clients stamp the same `corr`. A later **re-emission**
    /// of the same outcome carries a different `engine_seq`, so it does not reuse
    /// a consumed correlation: it pops the next pending op (or [`None`]).
    ///
    /// A **windowed** key ([`record_window`](Self::record_window)) resolves
    /// without consuming: every event inside the window (seq after its fence)
    /// stamps the window's op; an event from before the window opened stays
    /// uncorrelated.
    ///
    /// A poisoned lock yields [`None`] (uncorrelated) rather than panicking on the
    /// realtime projection path.
    #[must_use]
    pub fn resolve(&self, key: &CorrKey, engine_seq: u64) -> Option<OperationId> {
        let Ok(mut inner) = self.inner.lock() else {
            return None;
        };
        // A client already resolved this exact outcome event: reuse its op so
        // every subscriber stamps a consistent `corr`.
        if let Some(op) = inner.resolved.get(&engine_seq) {
            return Some(op.clone());
        }
        // A windowed (multi-event) key: stamp every matching outcome inside
        // the window without consuming it. An event published before the
        // window opened (an earlier operation's straggler) stays uncorrelated
        // — never a wrong id. Windowed keys never use the pending FIFO.
        if let Some(window) = inner.windows.get(key) {
            if engine_seq <= window.from_seq {
                return None;
            }
            let op = window.op.clone();
            inner.memoize_resolved(engine_seq, op.clone(), self.capacity);
            return Some(op);
        }
        // First resolver for this engine seq: pop the oldest pending op for the
        // key and memoize it against the seq.
        let op = {
            let queue = inner.pending.get_mut(key)?;
            let popped = queue.pop_front();
            if queue.is_empty() {
                inner.pending.remove(key);
            }
            popped
        }?;
        // Drop the first matching key from the pending order so the bound stays
        // exact and a consumed correlation cannot be evicted twice.
        if let Some(pos) = inner.order.iter().position(|k| k == key) {
            inner.order.remove(pos);
        }
        inner.memoize_resolved(engine_seq, op.clone(), self.capacity);
        Some(op)
    }
}

/// One realtime frame plus the metadata the transport needs to emit it.
#[derive(Debug, Clone, PartialEq)]
pub struct RealtimeFrame {
    /// Whether this is a snapshot baseline or a delta.
    pub kind: FrameKind,
    /// The envelope to serialize.
    pub envelope: Envelope<Event>,
}

impl RealtimeFrame {
    /// Serialize the envelope to a JSON string for the wire.
    ///
    /// # Errors
    ///
    /// [`serde_json::Error`] only if the envelope is not serializable, which is
    /// not possible for the fixed [`Event`] union — callers may treat an error
    /// as "drop this frame".
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self.envelope)
    }
}

/// The outcome of a live re-authorization check ([`SessionStream::reauthorize`],
/// ADR-RT010) — what the transport must do about a mid-session authorization change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReauthOutcome {
    /// Authorization is current (the generation is unchanged, the session is not
    /// store-managed, or the re-resolved role+scope still match): keep streaming.
    Unchanged,
    /// The principal's object scope changed (narrowed or widened) while it can
    /// still read: the session has adopted the new scope. The transport emits a
    /// `$resync` rebuild directive and re-sends the connect snapshot set under the
    /// new scope, so the client drops now-hidden objects and gains newly-visible
    /// ones.
    ScopeChanged,
    /// The principal lost read access (its key was revoked, or its role can no
    /// longer [`Action::Read`]): the transport tears the session down (WS close
    /// [`WS_CLOSE_FORBIDDEN`], SSE stream end).
    Disconnect,
}

/// The live re-authorization handle a store-managed API-key session carries
/// (ADR-RT010): it re-resolves the *current* principal for `key_id` from the
/// shared [`ApiKeyStore`] whenever the wait-free `generation` advances.
///
/// Installed only for principals the store tracks; local-admin (auth disabled) and
/// JWT principals have no store entry and keep their connect-time authorization, so
/// they carry no `LiveAuthz`. The `store` handle is control-plane only — the engine
/// never touches it (invariant #10).
#[derive(Debug)]
struct LiveAuthz {
    /// The shared API-key store (the authorization source of truth).
    store: Arc<ApiKeyStore>,
    /// The authenticated key id whose current authorization is re-resolved.
    key_id: String,
    /// The role last re-resolved for this session (adopted on each change).
    role: Role,
    /// The authorization generation last observed; a higher store generation means
    /// re-resolve.
    generation: u64,
}

/// One step pulled from the engine broadcast by [`SessionStream::recv_event`]: an
/// event to project, a skip (a resumed event already seen, or a lag the reader
/// recovered from), or the channel closed.
#[derive(Debug)]
enum RecvStep {
    /// An event to project into a delta frame (via `frame_for`).
    Event(SeqEvent<Event>),
    /// Nothing to emit this step (resume/lag skip); poll again.
    Skipped,
    /// Every engine publish handle was dropped — end the session.
    Closed,
}

/// Drives one realtime session: emits the snapshot then streams deltas with
/// lagged-skip semantics, issuing strictly-increasing per-connection `seq`s.
///
/// This is transport-agnostic so both the WS and SSE handlers (and tests) share
/// one tested core. It owns the engine subscription and the per-connection
/// sequence counter.
#[derive(Debug)]
pub struct SessionStream {
    sub: EventSubscription<Event>,
    session_id: String,
    next_seq: u64,
    snapshot_sent: bool,
    /// A resume cursor: deltas with an engine seq `<= resume_after` are skipped
    /// (already observed by the client before its disconnect).
    resume_after: Option<u64>,
    /// The command-outcome correlation registry, when wired. When present,
    /// [`SessionStream::next_delta`] stamps an outcome event's envelope with the
    /// `corr` (op id) the accepted command recorded (ADR-W008). When [`None`]
    /// (e.g. the existing transport-only tests) no `corr` is stamped.
    corr: Option<Arc<CorrRegistry>>,
    /// The connecting principal's object-id allowlist (BOLA, ADR-W005/ADR-W025),
    /// or [`None`] for an unscoped principal that sees every object. Part of the
    /// per-session [`AuthzScopes`] view the read filter routes every event's
    /// [`Event::authz_scope`] through via [`scope_permits`] — identical to REST.
    scoped_object_ids: Option<Vec<String>>,
    /// The connecting principal's output-id allowlist (per-output BOLA + the
    /// `program:`-namespaced timing grant, ADR-W026), or [`None`] for unrestricted.
    scoped_output_ids: Option<Vec<String>>,
    /// The connecting principal's discovery-domain allowlist (ADR-W026), or
    /// [`None`] for unrestricted (sees all rows including unlabelled). A scoped
    /// principal is denied unlabelled rows (fail-closed) — enforced by
    /// [`scope_permits`], not here.
    ///
    /// Together these three axes are the read-side projection: a delta whose
    /// [`Event::authz_scope`] the principal's [`AuthzScopes`] does not permit is
    /// dropped, and the connect snapshot skips out-of-scope ids — by parity with
    /// `GET /{id}` returning `403`, so a scoped client cannot enumerate what it
    /// could not read. Pure per-client read filtering: never blocks, never
    /// touches the engine publish path (invariant #10).
    scoped_discovery_domains: Option<Vec<String>>,
    /// The connect-time **broadcast watermark** (ADR-RT009): the engine event
    /// sequence (`EnginePublisher::events.sequence()`) captured together with the
    /// connect snapshot, or [`None`] for a session with no snapshot pairing (the
    /// resume path and transport-only tests). When set, a delta whose engine
    /// `seq <= watermark` is already reflected in the snapshot the client
    /// received, so it is dropped rather than re-delivered — killing the duplicate
    /// and the transient backward-roll a queued pre-snapshot transition would
    /// cause. The drop is a read-side, per-connection decision on events already
    /// pulled from the bounded broadcast: no lock, no await, no back-pressure on
    /// the engine (invariant #10).
    snapshot_watermark: Option<u64>,
    /// The live re-authorization handle (ADR-RT010), or [`None`] for a session
    /// whose authorization is fixed for its lifetime (an unscoped local-admin, a
    /// JWT principal, or a transport-only test). When set,
    /// [`SessionStream::reauthorize`] re-resolves the current principal on an
    /// authorization-generation change and updates all three scope axes in place
    /// — so the read filter honors a mid-session scope change without a
    /// reconnect. Sampling the generation is a
    /// wait-free atomic load; the re-resolve takes only a control-plane read lock
    /// the engine never holds (invariant #10).
    live_authz: Option<LiveAuthz>,
}

impl SessionStream {
    /// Build a session over an engine event subscription.
    ///
    /// `resume_after` is the client's last observed engine sequence (`since_seq`
    /// / `Last-Event-ID`), or [`None`] for a fresh connection.
    #[must_use]
    pub fn new(
        sub: EventSubscription<Event>,
        session_id: impl Into<String>,
        resume_after: Option<u64>,
    ) -> Self {
        Self {
            sub,
            session_id: session_id.into(),
            next_seq: 0,
            snapshot_sent: false,
            resume_after,
            corr: None,
            scoped_object_ids: None,
            scoped_output_ids: None,
            scoped_discovery_domains: None,
            snapshot_watermark: None,
            live_authz: None,
        }
    }

    /// Wire the command-outcome correlation registry onto this session.
    ///
    /// With it installed, [`SessionStream::next_delta`] stamps each outcome
    /// event's envelope with the `corr` (op id) the originating command recorded
    /// at 202 time (ADR-W008), consuming the correlation once. Without it the
    /// stream is unchanged (every `corr` stays `None`).
    #[must_use]
    pub fn with_corr_registry(mut self, corr: Arc<CorrRegistry>) -> Self {
        self.corr = Some(corr);
        self
    }

    /// Confine this session to the connecting principal's object-id allowlist
    /// (BOLA visibility, ADR-W005/ADR-W025).
    ///
    /// `scope` is the principal's [`scoped_object_ids`](crate::auth::Principal):
    /// `Some(allowlist)` filters the stream to a read-side projection (a
    /// Devices-domain delta or connect snapshot for an object outside the
    /// allowlist is dropped), `None` (an unscoped admin/operator/viewer) leaves
    /// the stream unfiltered on the object axis. A pure per-client read decision —
    /// never blocks, never touches the engine publish path (invariant #10).
    ///
    /// This sets only the object axis (the output/discovery axes stay unset); use
    /// [`with_scopes`](Self::with_scopes) to confine all three at once.
    #[must_use]
    pub fn with_object_scope(mut self, scope: Option<Vec<String>>) -> Self {
        self.scoped_object_ids = scope;
        self
    }

    /// Confine this session to the connecting principal's full three-axis scope
    /// (object + output + discovery-domain, ADR-W026).
    ///
    /// Each axis mirrors the same-named [`Principal`] field: `Some(allowlist)`
    /// filters, `None` is unrestricted on that axis. The read filter routes every
    /// event's [`Event::authz_scope`] through [`scope_permits`] against these
    /// three, so realtime and REST cannot fork authorization semantics. A pure
    /// per-client read decision — never blocks, never touches the engine publish
    /// path (invariant #10).
    #[must_use]
    pub fn with_scopes(
        mut self,
        objects: Option<Vec<String>>,
        outputs: Option<Vec<String>>,
        discovery_domains: Option<Vec<String>>,
    ) -> Self {
        self.scoped_object_ids = objects;
        self.scoped_output_ids = outputs;
        self.scoped_discovery_domains = discovery_domains;
        self
    }

    /// Borrow this session's three authorization axes as the unified view the
    /// shared [`scope_permits`] predicate consumes. Allocates nothing.
    fn scopes(&self) -> AuthzScopes<'_> {
        AuthzScopes::new(
            self.scoped_object_ids.as_deref(),
            self.scoped_output_ids.as_deref(),
            self.scoped_discovery_domains.as_deref(),
        )
    }

    /// Pair this session with the connect-time **broadcast watermark** (ADR-RT009).
    ///
    /// `watermark` is the engine event sequence (`EnginePublisher::events.sequence()`)
    /// captured together with the connect snapshot. With it set,
    /// [`SessionStream::next_delta`] drops every subscribed event whose engine
    /// `seq <= watermark` — those are already reflected in the snapshot the client
    /// received, so re-delivering them would be a duplicate (and, for a
    /// multi-transition object, a transient backward roll). The drop happens
    /// **before** `issue_seq`, so it leaves no gap in the per-connection sequence
    /// (resume-by-seq stays intact) and composes with the object-scope filter.
    ///
    /// A pure per-client read decision on events already pulled from the bounded
    /// broadcast: it never blocks, never locks, and never touches the engine
    /// publish path (invariant #10).
    #[must_use]
    pub fn with_snapshot_watermark(mut self, watermark: u64) -> Self {
        self.snapshot_watermark = Some(watermark);
        self
    }

    /// Wire **live re-authorization** onto this session (ADR-RT010).
    ///
    /// `store` is the shared [`ApiKeyStore`], `key_id` the authenticated key, and
    /// `role` its connect-time role. With it installed, [`reauthorize`](Self::reauthorize)
    /// re-resolves the current principal for `key_id` whenever the store's
    /// authorization generation advances, honoring a mid-session scope narrow/widen
    /// (adopt the new scope + signal a rebuild), role downgrade below read, or key
    /// revocation (disconnect). Install it **only** for store-managed API-key
    /// principals — local-admin and JWT principals are not in the store and keep
    /// their connect-time authorization.
    ///
    /// The generation is sampled at build time, so a change strictly after connect
    /// is honored; a change racing the connect handshake is picked up at the next
    /// generation bump (self-healing — connect itself resolves current authz).
    #[must_use]
    pub fn with_live_reauth(
        self,
        store: Arc<ApiKeyStore>,
        key_id: impl Into<String>,
        role: Role,
    ) -> Self {
        // Convenience: baseline the handle at the CURRENT generation (the caller has
        // resolved current authz at build time). The transports use
        // [`with_live_reauth_at`] with the generation captured at connect-auth, so a
        // change racing the connect handshake is caught (ADR-RT010).
        let generation = store.generation();
        self.with_live_reauth_at(store, key_id, role, generation)
    }

    /// Wire live re-authorization with an explicit **baseline generation**
    /// captured at connect-authentication (ADR-RT010).
    ///
    /// `baseline_generation` is the store's authorization generation sampled at the
    /// moment the connecting principal was resolved (`resolve_principal`), *before*
    /// the handle is installed. Because it is captured at-or-before the principal
    /// read, a revoke or re-scope landing in the connect window — between auth and
    /// install — leaves `store.generation() > baseline`, so the first
    /// [`reauthorize`](Self::reauthorize) re-resolves and honors it (disconnect on
    /// revoke, adopt on re-scope). This closes the connect-race a fresh build-time
    /// sample would miss (it would capture the post-mutation generation with the
    /// pre-mutation principal, and never re-resolve).
    #[must_use]
    pub fn with_live_reauth_at(
        mut self,
        store: Arc<ApiKeyStore>,
        key_id: impl Into<String>,
        role: Role,
        baseline_generation: u64,
    ) -> Self {
        self.live_authz = Some(LiveAuthz {
            store,
            key_id: key_id.into(),
            role,
            generation: baseline_generation,
        });
        self
    }

    /// The role currently adopted by live re-authorization (ADR-RT010), or [`None`]
    /// for a session without a live-authz handle. Reflects the latest role
    /// [`reauthorize`](Self::reauthorize) resolved, so a mid-session role change is
    /// observable.
    #[must_use]
    pub fn live_role(&self) -> Option<Role> {
        self.live_authz.as_ref().map(|live| live.role)
    }

    /// Re-resolve the session's authorization if it changed since the last check
    /// (ADR-RT010), returning what the transport must do about it.
    ///
    /// The fast path is a single wait-free atomic load of the store's generation;
    /// only when it has advanced does this take the store's short control-plane read
    /// lock to re-resolve the current principal. A session with no live-authz handle
    /// (local-admin / JWT / transport-only tests) always returns
    /// [`ReauthOutcome::Unchanged`]. No engine lock, no await, no channel into the
    /// engine, no back-pressure on the publish path (invariant #10).
    ///
    /// * key revoked / role can no longer [`Action::Read`] → [`ReauthOutcome::Disconnect`].
    /// * any scope axis changed — object, output, or discovery-domain (narrow,
    ///   widen, or `None`↔`Some`) → the new scope is adopted in place and
    ///   [`ReauthOutcome::ScopeChanged`] is returned (ADR-W026).
    /// * otherwise (role adopted, scope unchanged) → [`ReauthOutcome::Unchanged`].
    pub fn reauthorize(&mut self) -> ReauthOutcome {
        // Fast path: sample the generation and, only on a change, re-resolve —
        // ending the immutable borrow before mutating session state below.
        let (current_gen, resolved) = match self.live_authz.as_ref() {
            None => return ReauthOutcome::Unchanged,
            Some(live) => {
                let current_gen = live.store.generation();
                if current_gen == live.generation {
                    return ReauthOutcome::Unchanged;
                }
                (current_gen, live.store.principal_for_key(&live.key_id))
            }
        };
        // Record the observed generation so we re-resolve once per change (whatever
        // the outcome). `live_authz` is `Some` in this branch.
        if let Some(live) = self.live_authz.as_mut() {
            live.generation = current_gen;
        }
        match resolved {
            // Still authenticated and still able to read the realtime firehose:
            // adopt the re-resolved role, and adopt the scope if it changed.
            Some(principal) if principal.role.can(Action::Read) => {
                if let Some(live) = self.live_authz.as_mut() {
                    live.role = principal.role;
                }
                // Compare ALL THREE authorization axes (ADR-W026): an output- or
                // discovery-only re-scope must propagate to the live stream too —
                // the pre-W026 object-only comparison left those axes dead.
                let changed = self.scoped_object_ids != principal.scoped_object_ids
                    || self.scoped_output_ids != principal.scoped_output_ids
                    || self.scoped_discovery_domains != principal.scoped_discovery_domains;
                if changed {
                    // Adopt the new scope in place; the read filter and the
                    // re-snapshot the transport now emits both honor it.
                    self.scoped_object_ids = principal.scoped_object_ids;
                    self.scoped_output_ids = principal.scoped_output_ids;
                    self.scoped_discovery_domains = principal.scoped_discovery_domains;
                    ReauthOutcome::ScopeChanged
                } else {
                    ReauthOutcome::Unchanged
                }
            }
            // The key is gone (revoked), or the re-resolved role can no longer read
            // the realtime firehose. Since EVERY current role permits `Action::Read`,
            // the role-can't-read arm is currently UNREACHABLE (untested by
            // construction) — a forward-compatible guard for a future non-reading
            // role; in practice this is exclusively the key-revocation path.
            _ => ReauthOutcome::Disconnect,
        }
    }

    /// Build the server-initiated `$resync` control frame that tells the client to
    /// **rebuild** (not merge) the object-bearing topics after a mid-session scope
    /// change (ADR-RT010, [`ReauthOutcome::ScopeChanged`]).
    ///
    /// It names exactly the topics `build_resync_frames` re-snapshots
    /// ([`Topic::Tiles`], [`Topic::Devices`]). `Switcher` is deliberately EXCLUDED:
    /// it is neither object-authz-scoped (the scope filter is Devices-domain) nor
    /// re-snapshotted at connect or on resync, so listing it would tell a
    /// rebuild-not-merge client to clear switcher state it never receives back —
    /// stranding it. The client drops now-hidden cached objects on the listed
    /// topics; the transport then re-sends the connect snapshot set under the new
    /// scope, so the client rebuilds to exactly what a fresh connect under the new
    /// scope would show. Reason
    /// [`ResyncReason::AuthzChanged`] distinguishes it from a replay-ring miss. Like
    /// the other snapshot frames it goes through `issue_seq`, so the per-connection
    /// seq stays gapless (resume-by-seq intact).
    #[must_use]
    pub fn resync_frame(&mut self, snapshot_seq: u64) -> RealtimeFrame {
        let seq = self.issue_seq();
        let envelope = Envelope::new(
            Topic::Control,
            seq,
            MediaTime::from_nanos(i64::try_from(snapshot_seq).unwrap_or(i64::MAX)),
            Event::Resync(Resync {
                reason: ResyncReason::AuthzChanged,
                resubscribe: vec![Topic::Tiles, Topic::Devices],
            }),
        );
        RealtimeFrame {
            kind: FrameKind::Snapshot,
            envelope,
        }
    }

    /// Allocate the next per-connection sequence number.
    fn issue_seq(&mut self) -> Seq {
        let seq = Seq::new(self.next_seq);
        self.next_seq = self.next_seq.saturating_add(1);
        seq
    }

    /// Build the connect-time `$hello` snapshot frame on the `$control` topic.
    ///
    /// `snapshot_seq` reports the engine state sequence the snapshot is current
    /// as of, so a later resume can reason about staleness.
    #[must_use]
    pub fn snapshot_frame(&mut self, snapshot_seq: u64) -> RealtimeFrame {
        self.snapshot_sent = true;
        let seq = self.issue_seq();
        let hello = Hello {
            session_id: self.session_id.clone(),
            server_v: vec![SchemaVersion::V1],
            heartbeat_ms: HEARTBEAT_MS,
            min_rate_hz: MIN_RATE_HZ,
            max_rate_hz: MAX_RATE_HZ,
            default_rate_hz: DEFAULT_RATE_HZ,
            replay_ring: u32::try_from(self.sub.len()).unwrap_or(u32::MAX),
        };
        let envelope = Envelope::new(
            Topic::Control,
            seq,
            MediaTime::from_nanos(i64::try_from(snapshot_seq).unwrap_or(i64::MAX)),
            Event::Hello(hello),
        );
        RealtimeFrame {
            kind: FrameKind::Snapshot,
            envelope,
        }
    }

    /// Build the connect-time `tiles` `$snapshot` frame from the engine's
    /// latest-state blob, or [`None`] when the blob carries no usable `tiles`
    /// array (nothing published yet, or an older engine that does not fold
    /// tile states in) — then nothing extra is sent and the client falls back
    /// to the sparse `tile.state` deltas, exactly as before.
    ///
    /// Emitted right after [`SessionStream::snapshot_frame`] on both
    /// transports so a fresh client REBUILDS its tile cache to the current
    /// truth (realtime-api §5; `snapshot ⊕ ordered deltas = current truth`,
    /// ADR-RT003). `snapshot_seq` is the engine state sequence the baseline is
    /// current as of (`as_of_seq` + the envelope `ts`, mirroring `$hello`).
    /// Reading the blob is a wait-free latest-state load — never a request the
    /// engine services (invariant #10).
    #[must_use]
    pub fn tiles_snapshot_frame(
        &mut self,
        snapshot: &EngineStateSnapshot,
        snapshot_seq: u64,
    ) -> Option<RealtimeFrame> {
        let mut tiles = tiles_from_engine_snapshot(snapshot)?;
        // BOLA visibility (ADR-W005/ADR-W025): drop snapshot tiles bound to an
        // out-of-scope input — the same object axis the `tile.state` delta is
        // gated on (its `authz_scope()` is `Object(input)`) and `get_input_streams`
        // authorizes. A placeholder tile (`input: None`) carries no object id, so
        // it is retained; a no-op for an unscoped principal. Mirrors
        // `devices_snapshot_frames`.
        tiles.retain(|tile| match tile.input.as_deref() {
            Some(input) => self.id_in_object_scope(input),
            None => true,
        });
        let seq = self.issue_seq();
        let envelope = Envelope::new(
            Topic::Tiles,
            seq,
            MediaTime::from_nanos(i64::try_from(snapshot_seq).unwrap_or(i64::MAX)),
            Event::TilesSnapshot(TilesSnapshot {
                as_of_seq: snapshot_seq,
                tiles,
            }),
        );
        Some(RealtimeFrame {
            kind: FrameKind::Snapshot,
            envelope,
        })
    }

    /// Build the connect-time device-status `$snapshot` frame for the **first**
    /// device the registry tracks (id-sorted), or [`None`] when no device has a
    /// status yet — then nothing extra is sent.
    ///
    /// The `devices` topic carries a conflated, latest-wins `device.status` lane
    /// that is **excluded from the lossless replay ring** (ADR-RT007): a
    /// resuming client never replays stale gap samples, it re-snapshots from the
    /// registry instead. This is that re-snapshot frame: a single
    /// [`Event::DeviceStatus`] carrying the registry's current latest-wins value
    /// for the first device. The N-device connect path uses
    /// [`SessionStream::devices_snapshot_frames`]; this single-frame form is the
    /// minimal building block (and what the broadcaster test drives directly).
    /// Reading the registry is a wait-free control-plane map load — never a
    /// request the engine services (invariant #10).
    #[must_use]
    pub fn devices_snapshot_frame(
        &mut self,
        registry: &DeviceStatusRegistry,
        snapshot_seq: u64,
    ) -> Option<RealtimeFrame> {
        // The first IN-SCOPE device (BOLA visibility, ADR-W005/ADR-W025): a
        // scoped principal must not learn an out-of-scope device exists from the
        // connect snapshot, exactly as the deltas filter it.
        let status = registry
            .snapshot_all()
            .into_iter()
            .find(|status| self.id_in_object_scope(&status.device_id))?;
        Some(self.device_status_frame(status, snapshot_seq))
    }

    /// Build the connect-time device-status `$snapshot` frames for **every**
    /// device the registry tracks (id-sorted), one frame per device — the full
    /// re-snapshot a freshly-connecting client rebuilds its device cache from
    /// (ADR-RT003 / ADR-RT007). Empty when the registry tracks no device.
    ///
    /// Each frame carries that device's latest-wins [`Event::DeviceStatus`]; the
    /// conflated lane never replays from the ring, so this snapshot is the sole
    /// way a connecting/resuming client learns current device status. Reading the
    /// registry is a wait-free control-plane map load (invariant #10).
    #[must_use]
    pub fn devices_snapshot_frames(
        &mut self,
        registry: &DeviceStatusRegistry,
        snapshot_seq: u64,
    ) -> Vec<RealtimeFrame> {
        // Filter to the principal's in-scope devices (BOLA visibility,
        // ADR-W005/ADR-W025): the connect snapshot must not leak an out-of-scope
        // device that the deltas then hide. An unscoped principal keeps every
        // device (the filter is a no-op for an unscoped principal). The in-scope
        // statuses are collected first (ending the immutable `self` borrow the
        // filter needs) before mapping through `device_status_frame`, which takes
        // `&mut self` to issue per-connection seqs.
        let in_scope: Vec<DeviceStatus> = registry
            .snapshot_all()
            .into_iter()
            .filter(|status| self.id_in_object_scope(&status.device_id))
            .collect();
        in_scope
            .into_iter()
            .map(|status| self.device_status_frame(status, snapshot_seq))
            .collect()
    }

    /// Wrap one latest-wins [`DeviceStatus`] in a `Snapshot`-kind realtime frame
    /// on the `devices` topic, scoped (envelope `id`) by its device id so an
    /// `ids` filter narrows the coarse topic to a detail view (ADR-RT007).
    fn device_status_frame(&mut self, status: DeviceStatus, snapshot_seq: u64) -> RealtimeFrame {
        let seq = self.issue_seq();
        let device_id = status.device_id.clone();
        let envelope = Envelope::new(
            Topic::Devices,
            seq,
            MediaTime::from_nanos(i64::try_from(snapshot_seq).unwrap_or(i64::MAX)),
            Event::DeviceStatus(status),
        )
        .with_id(device_id);
        RealtimeFrame {
            kind: FrameKind::Snapshot,
            envelope,
        }
    }

    /// Receive the next delta frame, applying **lagged-skip** isolation.
    ///
    /// Returns:
    /// * `Ok(Some(frame))` — a delta to emit.
    /// * `Ok(None)` — a resumed event that the client already saw (skipped) or a
    ///   lag the reader recovered from; the caller should poll again.
    /// * `Err(RecvError::Closed)` — the engine is gone; end the session.
    ///
    /// On [`RecvError::Lagged`] this **does not** propagate back-pressure: it
    /// re-subscribes from the channel head and returns `Ok(None)`. The client
    /// will re-baseline (a `$lag`/`$resync` is the next layer's concern); the
    /// engine is never blocked.
    ///
    /// # Errors
    ///
    /// [`RecvError::Closed`] when every engine publish handle has been dropped.
    pub async fn next_delta(&mut self) -> Result<Option<RealtimeFrame>, RecvError> {
        match self.recv_event().await {
            RecvStep::Event(seq_event) => Ok(self.frame_for(&seq_event)),
            RecvStep::Skipped => Ok(None),
            RecvStep::Closed => Err(RecvError::Closed),
        }
    }

    /// Pull the next raw event from the engine broadcast, applying **lagged-skip**
    /// isolation, without projecting it into a frame.
    ///
    /// Split out from [`next_delta`](Self::next_delta) so the transport can
    /// [`reauthorize`](Self::reauthorize) **between** receiving an event and
    /// projecting it (`frame_for`) — a mid-`await` scope change is then applied
    /// before the event is filtered, so no out-of-scope delta slips through and the
    /// per-connection seq stays gapless (ADR-RT010). Two read modes:
    /// * **Resume replay** (`resume_after` set): drain the bounded broadcast ring
    ///   **non-blocking** via `try_recv`; `Empty` yields [`RecvStep::Skipped`]
    ///   promptly (awaiting here would wedge a caller polling past the gap). No
    ///   current transport sets `resume_after`, so this branch runs solely in the
    ///   broadcaster resume tests until a `since_seq` cursor lands.
    /// * **Live tail** (`resume_after == None`, the connect path): `await` the next
    ///   event cooperatively (cancel-safe, so the transport's `select!` may drop it
    ///   for a re-auth tick without losing a message). A slow client lags and is
    ///   skipped; the engine is never back-pressured (invariant #10).
    async fn recv_event(&mut self) -> RecvStep {
        if self.resume_after.is_some() {
            return match self.sub.try_recv() {
                Ok(seq_event) => RecvStep::Event(seq_event),
                Err(TryRecvError::Empty) => RecvStep::Skipped,
                Err(TryRecvError::Lagged(_)) => {
                    self.sub = self.sub.resubscribe();
                    RecvStep::Skipped
                }
                Err(TryRecvError::Closed) => RecvStep::Closed,
            };
        }
        match self.sub.recv().await {
            Ok(seq_event) => RecvStep::Event(seq_event),
            Err(RecvError::Lagged(_)) => {
                // Drop-oldest overflow for THIS slow client only: resubscribe at
                // the head and let the client re-baseline. The engine never saw
                // any back-pressure (invariant #10).
                self.sub = self.sub.resubscribe();
                RecvStep::Skipped
            }
            Err(RecvError::Closed) => RecvStep::Closed,
        }
    }

    /// Turn one received engine event into a delta frame to emit, or [`None`]
    /// when this resuming client must skip it (already observed, or a conflated
    /// latest-wins sample excluded from the lossless replay ring).
    ///
    /// The ADR-RT007 replay-ring rule, per event:
    /// `topic.is_high_rate() || event.is_conflated()`. A resuming client replays
    /// the gap LOSSLESSLY only for the lossless lanes — the conflated latest-wins
    /// samples (`device.status`, `timing.status`, `audio.meter`,
    /// `system.metrics`) are EXCLUDED because a re-snapshot heals them to the
    /// latest value (a stale gap sample would be worse than none). A fresh
    /// connection (no resume cursor) delivers everything live, exactly as before.
    /// This skip is purely a per-client read decision; the engine's publish path
    /// is untouched (invariant #10).
    fn frame_for(
        &mut self,
        seq_event: &multiview_engine::SeqEvent<Event>,
    ) -> Option<RealtimeFrame> {
        if let Some(after) = self.resume_after {
            if seq_event.seq <= after {
                return None;
            }
            let topic = topic_for_event(&seq_event.event);
            if topic.is_high_rate() || (*seq_event.event).is_conflated() {
                return None;
            }
        }
        // Connect-time broadcast watermark (ADR-RT009): a **snapshot-backed** event
        // whose engine seq is at or before the broadcast frontier captured with the
        // connect snapshot is ALREADY reflected in that snapshot — drop it so it is
        // not re-delivered as a delta (duplicate) and a queued pre-snapshot
        // transition cannot replay after the newer snapshot (transient backward
        // roll). The drop is SCOPED to snapshot-backed events
        // ([`event_is_snapshot_backed`] — only `tile.state` and `device.status`,
        // the two classes the connect snapshot reproduces): a lossless / event-only
        // variant (`device.discovered`/`.mode`/`.error`, cast/media/alert/…) is in
        // NO snapshot and carries no resumable seq, so a global drop would
        // permanently lose one that lands in the subscribe→snapshot window — worse
        // than the duplicate this fixes. Checked BEFORE `issue_seq` so the drop
        // leaves no gap in the per-connection seq (like the resume/conflated/scope
        // skips), and before the object-scope filter so the read-side drops
        // compose. A pure per-client read decision on an event already pulled from
        // the bounded broadcast: no lock, no await, never touches the engine publish
        // path (invariant #10).
        if let Some(watermark) = self.snapshot_watermark {
            if seq_event.seq <= watermark && event_is_snapshot_backed(&seq_event.event) {
                return None;
            }
        }
        // Per-scope visibility (BOLA + ADR-W026): a scoped principal receives an
        // event only when its three-axis `AuthzScopes` permits the event's
        // `authz_scope()` — the SAME `scope_permits` predicate REST authorization
        // routes through, so the two surfaces cannot fork. Covers the object axis
        // (device.* / cast.session.* / tile / media-player), the output axis
        // (rist.link.stats), the program axis (timing.status), and the discovery
        // axis (device.discovered) uniformly; a `Public` event (tiles/alerts/audio,
        // control frames) is always delivered, gated only by the connect-time
        // role. Checked BEFORE `issue_seq` so a dropped out-of-scope delta leaves
        // no gap in the per-connection seq sequence (exactly like the
        // resume/conflated skips above).
        if !self.event_in_scope(&seq_event.event) {
            return None;
        }
        let seq = self.issue_seq();
        let event = (*seq_event.event).clone();
        let topic = topic_for_event(&event);
        // Resource scope (the tile/input/output id) the client keys this delta
        // by — read before the event is moved into the envelope.
        let scope = event_scope_id(&event);
        // The command-outcome correlation, read before the event moves: if this
        // event is the outcome of an accepted command, resolve the op id the
        // 202'd request recorded so the envelope echoes it as `corr` (ADR-W008).
        // Keyed by the engine seq so every subscriber stamps a consistent `corr`
        // for one outcome, while a re-emitted outcome (a new engine seq) does not
        // reuse a consumed correlation. Non-command events stay uncorrelated. The
        // registry lock is control-plane-only and never touches the engine hot
        // loop (invariant #10).
        let corr = self.corr.as_ref().and_then(|registry| {
            CorrKey::for_event(&event).and_then(|key| registry.resolve(&key, seq_event.seq))
        });
        let mut envelope = Envelope::new(
            topic,
            seq,
            MediaTime::from_nanos(i64::try_from(seq_event.seq).unwrap_or(i64::MAX)),
            event,
        );
        if let Some(id) = scope {
            envelope = envelope.with_id(id);
        }
        if let Some(op) = corr {
            envelope = envelope.with_corr(op.as_str());
        }
        Some(RealtimeFrame {
            kind: FrameKind::Delta,
            envelope,
        })
    }

    /// Whether the connecting principal's scope permits delivering this event
    /// (BOLA visibility across all axes, ADR-W005/ADR-W025/ADR-W026).
    ///
    /// Routes the event's [`Event::authz_scope`] through the shared
    /// [`scope_permits`] predicate against the session's three-axis
    /// [`AuthzScopes`] — the exact rule REST authorization uses, so the stream
    /// never delivers something the principal could not read via REST. A pure
    /// borrowed-match read predicate over small allowlists: no lock, no await, no
    /// allocation, never touches the engine publish path (invariant #10).
    fn event_in_scope(&self, event: &Event) -> bool {
        scope_permits(&self.scopes(), event.authz_scope())
    }

    /// Whether an object `id` is visible to this session's principal (BOLA
    /// visibility, ADR-W005/ADR-W025): `true` when unscoped or when `id` is in
    /// the object allowlist. Used to filter the connect-time snapshot frames
    /// (device status by device id, tiles by bound input id) through the SAME
    /// [`scope_permits`] rule as the per-delta [`event_in_scope`](Self::event_in_scope),
    /// so the snapshot and delta filters cannot fork.
    fn id_in_object_scope(&self, id: &str) -> bool {
        scope_permits(&self.scopes(), AuthzScope::Object(id))
    }
}

/// Parse the per-tile lifecycle entries out of the engine's opaque
/// latest-state blob, or [`None`] when it carries no `tiles` array (an older
/// engine, or nothing published yet). Malformed entries are skipped — a partial
/// baseline from a well-formed remainder beats none — and a blob whose entries
/// are ALL malformed yields an empty (still well-formed) baseline.
fn tiles_from_engine_snapshot(snapshot: &EngineStateSnapshot) -> Option<Vec<TileSnapshotEntry>> {
    let tiles = snapshot.get("tiles")?.as_array()?;
    Some(
        tiles
            .iter()
            .filter_map(|entry| serde_json::from_value(entry.clone()).ok())
            .collect(),
    )
}

/// The resource-scope id (the `Envelope::id`) a client keys an event delta by.
/// For a tile-state change it is the bound input/tile id, which the monitoring
/// UI uses to address the tile. Other events carry their scope in the envelope
/// the producer builds, so they return `None` here.
#[must_use]
pub fn event_scope_id(event: &Event) -> Option<String> {
    match event {
        Event::TileState(tile) => tile.input.clone(),
        // Devices-domain events scope by device id so the `ids` filter narrows
        // the coarse `devices` topic to a detail view (ADR-RT007).
        Event::DeviceStatus(status) => Some(status.device_id.clone()),
        Event::DeviceAdopted(adopted) => Some(adopted.device_id.clone()),
        Event::DeviceRemoved(removed) => Some(removed.device_id.clone()),
        Event::DeviceMode(mode) => Some(mode.device_id.clone()),
        Event::DeviceError(error) => Some(error.device_id.clone()),
        Event::DeviceSync(sync) => Some(sync.device_id.clone()),
        // Cast-session membership events scope by the session id (the same id
        // the conflated `device.status` lane keys the session's state under).
        Event::CastSessionStarted(started) => Some(started.session_id.clone()),
        Event::CastSessionRemoved(removed) => Some(removed.session_id.clone()),
        // `timing.status` scopes by the program/output stream the epoch maps.
        Event::TimingStatus(timing) => Some(timing.stream_id.clone()),
        // A media-player lifecycle event scopes by the player id (ADR-RT008), so
        // the `ids` filter narrows the coarse `switcher` topic to one player.
        Event::MediaPlayerState(e) => Some(e.player.clone()),
        // A discovery row has no registry id yet (untrusted inventory): it is
        // correlated to its scan operation via `corr`, never a fabricated id.
        _ => None,
    }
}

// The per-event authorization classification formerly computed here
// (`object_authz_scope_id`) is replaced by the total, wildcard-free
// `Event::authz_scope()` in `multiview-events` (ADR-W026): the realtime filter
// now routes it through the shared `scope_permits` predicate (see
// `SessionStream::event_in_scope`), which covers the object, output, program,
// and discovery axes instead of the object axis alone. Adding an `Event` variant
// now fails compilation in the events crate until it is classified — the
// `_ => None` firehose that silently delivered every unclassified event to
// scoped principals is gone.

/// Whether the connect-time snapshot reproduces this event's current value as a
/// same-topic snapshot frame (ADR-RT009) — the ONLY class the connect watermark
/// may drop.
///
/// `true` for exactly the two variants a fresh client receives a snapshot frame
/// for, so a pre-watermark delta of them is already in that snapshot and
/// re-delivering it would duplicate / roll the client back:
/// * `tile.state` — reproduced by the `tiles` snapshot ([`SessionStream::tiles_snapshot_frame`],
///   read from the engine state blob; the tick path publishes the state fold
///   before the event, so a dropped delta is already reflected).
/// * `device.status` — reproduced by the per-device `device.status` snapshot
///   ([`SessionStream::devices_snapshot_frames`], read from the
///   [`DeviceStatusRegistry`]; `DeviceBroadcaster::publish_status` updates the
///   registry before publishing the event, so a dropped delta is already
///   reflected).
///
/// `false` for **every other** event: the device lifecycle events
/// (`device.discovered`/`.mode`/`.error`/`.adopted`/`.removed`/`.sync`),
/// cast-session membership, `media.player_state`, alerts, input/job/alarm/tally/
/// salvo, and the un-re-snapshotted conflated telemetry (`timing.status`,
/// `audio.meter`, `system.metrics`, `rist.link.stats`). None appears in any
/// connect snapshot frame, so the watermark must NEVER drop them — a global drop
/// would permanently lose one that lands in the subscribe→snapshot window (it has
/// no snapshot to heal from and no seq the client can resume; RT003 losslessness).
#[must_use]
fn event_is_snapshot_backed(event: &Event) -> bool {
    matches!(event, Event::TileState(_) | Event::DeviceStatus(_))
}

/// The coarse topic an event is published on (the realtime-api topic map).
#[must_use]
pub fn topic_for_event(event: &Event) -> Topic {
    match event {
        Event::TileState(_) | Event::TilesSnapshot(_) => Topic::Tiles,
        Event::AudioMeter(_) => Topic::AudioMeters,
        // The program-bus EBU R128 loudness compliance lane (AUD-8) rides its own
        // conflated `audio.loudness` topic so the loudness meter can subscribe to
        // it independently of the high-rate per-track peak/RMS meters.
        Event::AudioLoudness(_) => Topic::AudioLoudness,
        // High-rate whole-system metrics (cpu/gpu/encoder) ride the conflated
        // `system` lane the footer subscribes to — NOT the control firehose.
        Event::SystemMetrics(_) => Topic::System,
        // Output sink status AND RIST link-health stats ride the `outputs` lane:
        // a RIST link's retransmit/RTT/quality telemetry (ADR-0095 Tier-1) is an
        // output-sink concern. `rist.link.stats` is conflated latest-wins
        // (`Event::is_conflated`), so the session pump excludes it from the
        // lossless replay ring exactly like the other conflated samples (inv #10).
        Event::OutputStatus(_) | Event::RistLinkStats(_) => Topic::Outputs,
        // Operator alerts AND health warnings (SA-0) AND shed-load decisions ride
        // the existing `alerts` lane — a health warning is a richer sibling of an
        // alert (ADR-0035), and a shed-load is a discrete, lossless
        // degradation-signal event (invariant #9) in the same operator-signal
        // family, so it stays in the lossless replay ring (NOT the conflated
        // `system` lane).
        Event::AlertRaised(_)
        | Event::AlertCleared(_)
        | Event::HealthWarningRaised(_)
        | Event::HealthWarningCleared(_)
        | Event::ShedLoad(_) => Topic::Alerts,
        // Both input connection state AND elementary-stream inventory deltas ride
        // the existing `inputs` lane (RT-3: `input.streams` is a delta on
        // re-probe / PMT-version bump, not a new topic).
        Event::InputConnection(_) | Event::InputStreams(_) => Topic::Inputs,
        Event::JobProgress(_) => Topic::Jobs,
        // Broadcast monitoring/control events ride their own topics so a client
        // can subscribe to the alarm or tally firehose independently.
        Event::AlarmRaised(_)
        | Event::AlarmUpdated(_)
        | Event::AlarmCleared(_)
        | Event::AlarmAcked(_) => Topic::Alarms,
        Event::TallyState(_)
        | Event::SalvoArmed(_)
        | Event::SalvoTaken(_)
        | Event::SalvoCancelled(_) => Topic::Tally,
        // Every Devices-domain event rides the one coarse `devices` lane
        // (ADR-RT007): the conflated `device.status`/`timing.status` telemetry
        // AND the lossless lifecycle events — fine scoping is the `ids`
        // filter, never more topics.
        Event::DeviceStatus(_)
        | Event::DeviceAdopted(_)
        | Event::DeviceRemoved(_)
        | Event::DeviceMode(_)
        | Event::DeviceError(_)
        | Event::DeviceSync(_)
        | Event::DeviceDiscovered(_)
        | Event::CastSessionStarted(_)
        | Event::CastSessionRemoved(_)
        | Event::TimingStatus(_) => Topic::Devices,
        // The broadcast-switcher lifecycle lane (ADR-RT008): a media player's
        // discrete transport-state transition rides the lossless `switcher`
        // topic (scoped finer by the player `id`), NOT the `$control` catch-all.
        Event::MediaPlayerState(_) => Topic::Switcher,
        _ => Topic::Control,
    }
}

/// Read the engine's latest state snapshot, defaulting to JSON `null` when the
/// engine has not published yet.
#[must_use]
fn current_engine_snapshot(state: &AppState) -> (EngineStateSnapshot, u64) {
    let seq = state.engine.state.sequence();
    let snapshot = state
        .engine
        .state
        .latest()
        .map_or(serde_json::Value::Null, |arc| (*arc).clone());
    (snapshot, seq)
}

/// `GET /api/v1/ws` — the primary WebSocket transport.
///
/// **Authenticated (and origin-checked) before the upgrade.** Streaming the engine
/// event firehose (tile state, alerts, input/output status) is a privileged read: a
/// valid credential with at least [`Action::Read`] ([`Role::Viewer`]) is required.
/// The [`RealtimeViewer`] extractor and the role gate run before
/// [`WebSocketUpgrade::on_upgrade`], so a cross-origin (SEC-13), unauthenticated, or
/// under-privileged request fails as a debuggable `403`/`401` `problem+json` HTTP
/// response rather than a silently-closed socket (realtime-api §6). Native clients
/// send `Authorization: Bearer` directly on the upgrade; a browser (which cannot set
/// that header) presents a single-use `?ticket=` from `POST /api/v1/ws/ticket`
/// instead of the durable bearer (SEC-01).
///
/// On success, upgrades the connection and runs a [`SessionStream`] that emits
/// the snapshot then streams deltas, writing each as a text frame. A write that
/// blocks blocks only this client's task; the engine is never awaited.
///
/// [`Role::Viewer`]: crate::auth::Role::Viewer
// `pub(crate)`, not `pub`: this handler is registered on the router within this
// crate (`lib.rs`) and is never referenced across a crate boundary, so it need
// not — and must not — widen the visibility of its internal `ConcurrencyPermitClaim`
// extractor (a `pub(crate)` permit newtype) into the public API (private-interfaces).
pub(crate) async fn ws_handler(
    // The auth gate is the FIRST extractor, so authentication is decided before
    // axum's `WebSocketUpgrade` extractor runs — an unauthenticated request is a
    // `401`/`403` `problem+json`, never pre-empted by the upgrade extractor's
    // `426` on a request without the upgrade handshake.
    RealtimeViewer(principal, live_baseline): RealtimeViewer,
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
    // SEC-14: claim the in-flight concurrency permit (if the cap is enabled) and hold
    // it for the SESSION's life. axum spawns the upgraded socket as a DETACHED task
    // ([`WebSocketUpgrade::on_upgrade`]), so a permit tied to the empty `101` response
    // body would release at the handshake and leave a live socket uncounted; moving it
    // into the session makes each live WebSocket count against the concurrency cap
    // until it closes. `None` (limits disabled) ⇒ no-op, as before SEC-14.
    crate::limits::ConcurrencyPermitClaim(concurrency_permit): crate::limits::ConcurrencyPermitClaim,
) -> Response {
    // Carry the authenticated principal into the session so the stream is a
    // read-side projection of only its in-scope device/cast objects (BOLA
    // visibility, ADR-W005/ADR-W025) AND re-resolves live if its authorization is
    // narrowed/revoked mid-session (ADR-RT010). An unscoped principal sees all.
    // `live_baseline` is the auth-time authorization generation (store keys only),
    // threaded so a change racing the upgrade is caught by the first reauthorize.
    ws.on_upgrade(move |socket| async move {
        // Hold the concurrency permit for the whole session; it drops when this
        // detached task ends (the socket closes), freeing the slot.
        let _concurrency_permit = concurrency_permit;
        run_ws_session(socket, state, principal, live_baseline).await;
    })
}

/// A pre-upgrade auth + origin gate for the realtime transports.
///
/// As a [`FromRequestParts`] extractor it (1) enforces the [`AllowedOrigins`]
/// CSWSH gate, then (2) resolves the `Bearer` / JWT / `?ticket=` [`Principal`] and
/// enforces [`Action::Read`] — and because extractors run in argument order,
/// placing it before [`WebSocketUpgrade`] makes both checks strictly precede the
/// upgrade. So a cross-origin request is a `403`, an unauthenticated /
/// under-privileged one a `401`/`403`, and never a `426` from the upgrade extractor
/// (nor a silently-closed socket).
///
/// Carries the resolved [`Principal`] and its live-reauth baseline generation
/// (`Some` for store-managed API keys, `None` otherwise; ADR-RT010) captured at
/// authentication, so the session installs live re-authorization against the
/// connect-time generation and catches a change racing the handshake.
pub struct RealtimeViewer(pub Principal, pub Option<u64>);

impl FromRequestParts<AppState> for RealtimeViewer {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // CSWSH gate FIRST (SEC-13): a WebSocket handshake bypasses SOP/CORS, so a
        // cross-origin upgrade is refused before auth and regardless of
        // `auth_disabled` — a foreign page can never reach the firehose.
        enforce_origin(state, &parts.headers).map_err(IntoResponse::into_response)?;
        // A browser WebSocket/EventSource cannot set an `Authorization` header, so
        // the browser path presents a short-lived single-use `?ticket=` instead of
        // the durable bearer (SEC-01; header still wins for native clients).
        let ticket = Query::<TicketQuery>::from_request_parts(parts, state)
            .await
            .ok()
            .and_then(|q| q.0.ticket);
        let (principal, live_baseline) =
            resolve_principal(state, &parts.headers, ticket.as_deref())
                .map_err(IntoResponse::into_response)?;
        principal
            .role
            .require(Action::Read)
            .map_err(IntoResponse::into_response)?;
        Ok(Self(principal, live_baseline))
    }
}

/// The browser realtime-auth carrier (ADR-RT011): a WebSocket / `EventSource`
/// cannot set an `Authorization` header, so the short-lived single-use ticket from
/// [`ws_ticket_handler`] is passed as `?ticket=`. Unlike the durable bearer it
/// replaces, a consumed or expired ticket in a proxy/access log is inert (SEC-01,
/// CWE-598).
#[derive(Debug, serde::Deserialize)]
pub struct TicketQuery {
    /// The single-use realtime ticket minted by `POST /api/v1/ws/ticket`.
    ticket: Option<String>,
}

/// Resolve a [`Principal`] from the `Authorization` header (API key, then JWT) or,
/// failing that, a single-use `?ticket=` (the browser WS/SSE path).
///
/// Returns the principal and its **live-reauth baseline** (ADR-RT010): `Some(gen)`
/// — the store authorization generation captured *before* the store lookup — for a
/// store-managed API-key principal (revocable/re-scopable), or `None` for a
/// local-admin (auth disabled) or JWT principal, which are not store-revocable.
/// Capturing the generation before the lookup makes a revoke/re-scope racing the
/// connect handshake observable to the first `reauthorize` (it advances past the
/// baseline), closing the connect-race. A **ticket** already carries the baseline
/// captured at mint, so its consumed value is threaded through unchanged.
fn resolve_principal(
    state: &AppState,
    headers: &HeaderMap,
    ticket: Option<&str>,
) -> Result<(Principal, Option<u64>), crate::error::ControlError> {
    // Auth disabled (explicit trusted-network mode): the realtime stream is open
    // as a local admin, matching the REST `Principal` extractor. Not store-managed,
    // so no live-authz baseline. (The CSWSH origin gate runs independently, so it
    // still applies in this mode — see `enforce_origin`.)
    if state.auth_disabled {
        return Ok((Principal::local_admin(), None));
    }
    // Capture the store's authorization generation BEFORE resolving a store key
    // (ADR-RT010 connect-race). Generation is monotonic, so a baseline captured here
    // is never newer than the generation consistent with whatever principal the
    // verify below reads — a too-old baseline only forces a harmless redundant first
    // re-resolve; a too-new one would mask a window change (the defect this fixes).
    let baseline = state.api_keys.generation();
    let header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if let Ok(principal) = state.api_keys.verify_authorization(header) {
        return Ok((principal, Some(baseline)));
    }
    // JWT principals are not in the API-key store (revocation is a separate denylist,
    // future work): no live-authz baseline.
    if let Some(Ok(principal)) = state.authenticate_jwt(header) {
        return Ok((principal, None));
    }
    // Browser path: atomically consume the single-use ticket. It already carries the
    // minting principal + its RT010 baseline, so no store re-probe is needed.
    if let Some(token) = ticket {
        if let Some(resolved) = state.ws_tickets.consume(token) {
            return Ok(resolved);
        }
    }
    Err(crate::error::ControlError::Unauthenticated)
}

/// The lifetime of a realtime auth ticket (ADR-RT011 / ADR-RT005 ~30 s): short
/// enough that a ticket leaked into a log is inert almost immediately, long enough
/// for a browser to receive the mint response and open the socket.
pub const WS_TICKET_TTL: Duration = Duration::from_secs(30);

/// The hard ceiling on live (unconsumed, unexpired) realtime tickets. Minting past
/// it drops the oldest (invariant #10 — the control plane can never grow without
/// bound under a mint flood). With single-use consumption and a 30 s TTL the live
/// set is normally tiny; this is a flood ceiling, not an operating size.
pub const WS_TICKET_CAPACITY: usize = 4096;

/// One issued-but-unconsumed realtime ticket: the authorization it grants and its
/// expiry deadline.
#[derive(Debug, Clone)]
struct TicketRecord {
    /// The full principal (role + all scope axes) the WS/SSE session adopts.
    principal: Principal,
    /// The RT010 live-reauth baseline generation captured at mint (`Some` for a
    /// store-managed API key, `None` for local-admin/JWT), so a key revoked between
    /// mint and connect is caught by the session's first reauthorize.
    baseline: Option<u64>,
    /// The instant past which this ticket is expired and inert.
    expires_at: Instant,
}

/// A bounded, single-use, TTL-swept store of short-lived realtime auth tickets
/// (ADR-RT011, implementing ADR-RT005).
///
/// A browser cannot set an `Authorization` header on `new WebSocket()` /
/// `EventSource`, so rather than smuggle the durable bearer through the URL query
/// (SEC-01, CWE-598 — it leaks into proxy/access logs and history), the SPA mints a
/// ticket via `POST /api/v1/ws/ticket` and connects with `?ticket=`. The ticket is
/// a high-entropy opaque token consumed **atomically** on the WS/SSE upgrade.
///
/// It is **control-plane only** — the engine never touches it — behind a short-held
/// [`Mutex`] (the [`CorrRegistry`] pattern): mint and consume are O(1) plus a
/// bounded amortized sweep, never `.await`, and the retained set is capped
/// ([`WS_TICKET_CAPACITY`], drop-oldest), so a mint flood can never grow it without
/// bound (invariant #10).
#[derive(Debug, Default)]
pub struct WsTicketStore {
    inner: Mutex<WsTicketInner>,
}

/// The mutable interior of a [`WsTicketStore`]: the live tickets plus their
/// insertion (== expiry) order for drop-oldest / TTL eviction.
#[derive(Debug, Default)]
struct WsTicketInner {
    /// token → record. Consuming a ticket removes it here (single-use); its entry
    /// in `order` becomes a tombstone pruned lazily by `sweep_expired` / drop-oldest.
    tickets: HashMap<String, TicketRecord>,
    /// Token issue order. All tickets share one TTL, so this is also expiry order —
    /// the front is always the oldest, letting the sweep stop at the first live,
    /// unexpired ticket. Bounded to [`WS_TICKET_CAPACITY`] on mint.
    order: VecDeque<String>,
}

impl WsTicketInner {
    /// Evict expired tickets (and consumed-ticket tombstones) from the front. Since
    /// every ticket shares [`WS_TICKET_TTL`], `order` is expiry-ordered, so the scan
    /// stops at the first still-live, unexpired ticket — amortized O(evicted).
    fn sweep_expired(&mut self, now: Instant) {
        while let Some(front) = self.order.front() {
            let evict = match self.tickets.get(front) {
                Some(record) => now >= record.expires_at,
                None => true, // a consumed-ticket tombstone
            };
            if !evict {
                break;
            }
            if let Some(token) = self.order.pop_front() {
                self.tickets.remove(&token);
            }
        }
    }
}

impl WsTicketStore {
    /// An empty ticket store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, WsTicketInner> {
        // Recover a poisoned lock rather than propagate a panic on the auth path
        // (safety rule 3): the store only ever swaps whole `TicketRecord`s, so a
        // guard poisoned by an unrelated prior panic exposes a well-formed map.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Mint a ticket for `principal` (carrying its RT010 `baseline`), valid for
    /// [`WS_TICKET_TTL`] from now.
    #[must_use]
    pub fn mint(&self, principal: Principal, baseline: Option<u64>) -> String {
        self.mint_at(principal, baseline, Instant::now())
    }

    /// [`mint`](Self::mint) with an explicit issue instant, for deterministic tests.
    #[must_use]
    pub fn mint_at(&self, principal: Principal, baseline: Option<u64>, now: Instant) -> String {
        let token = new_ticket_token();
        let record = TicketRecord {
            principal,
            baseline,
            expires_at: now.checked_add(WS_TICKET_TTL).unwrap_or(now),
        };
        let mut inner = self.lock();
        inner.sweep_expired(now);
        // Drop-oldest until there is room, bounding the RETAINED set (tombstones
        // included) to the capacity so a flood can never grow memory (invariant #10).
        while inner.order.len() >= WS_TICKET_CAPACITY {
            let Some(old) = inner.order.pop_front() else {
                break;
            };
            inner.tickets.remove(&old);
        }
        inner.order.push_back(token.clone());
        inner.tickets.insert(token.clone(), record);
        token
    }

    /// Atomically **consume** a ticket: remove it and return its
    /// `(principal, baseline)` iff it exists and is unexpired. Single-use — a
    /// second consume (or an expired one) returns [`None`], and either way the
    /// token is gone (an expired token is inert).
    #[must_use]
    pub fn consume(&self, token: &str) -> Option<(Principal, Option<u64>)> {
        self.consume_at(token, Instant::now())
    }

    /// [`consume`](Self::consume) with an explicit instant, for deterministic tests.
    #[must_use]
    pub fn consume_at(&self, token: &str, now: Instant) -> Option<(Principal, Option<u64>)> {
        // Remove from the map (single-use); the `order` entry is left as a tombstone
        // pruned lazily by `sweep_expired`/drop-oldest, keeping consume O(1).
        let record = self.lock().tickets.remove(token)?;
        if now >= record.expires_at {
            return None;
        }
        Some((record.principal, record.baseline))
    }

    /// The number of live tickets retained (a bound for tests / metrics).
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().tickets.len()
    }

    /// Whether no live tickets are retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A high-entropy opaque realtime ticket token: two CSPRNG-backed (`getrandom`)
/// `UUIDv4` values, hex-encoded — ≥240 bits of entropy, far beyond guessable within
/// a single-use ticket's ~30 s life.
fn new_ticket_token() -> String {
    let mut token = String::with_capacity(64);
    token.push_str(&uuid::Uuid::new_v4().simple().to_string());
    token.push_str(&uuid::Uuid::new_v4().simple().to_string());
    token
}

/// The realtime `Origin` allow-list (SEC-13 / CSWSH, ADR-RT011): the operator's
/// configured extra origins plus the always-permitted same-origin case.
///
/// A WebSocket / SSE handshake is exempt from the Same-Origin Policy and CORS, so
/// this is the only thing standing between a foreign page and the engine firehose.
#[derive(Debug, Clone, Default)]
pub struct AllowedOrigins {
    /// `control.allowed_origins`, strictly parsed + canonicalized to [`Origin`]
    /// values. Empty ⇒ same-origin only. A raw string can never be stored, so
    /// `null`/malformed can never be matched (fail-closed by construction).
    configured: Vec<Origin>,
}

impl AllowedOrigins {
    /// Build from the operator's `control.allowed_origins` list. Each entry is
    /// strictly parsed to an [`Origin`]; anything that does not parse is dropped,
    /// so a `null`/malformed value can never be stored and thus never matched
    /// (fail-closed). Config-load validation (`MultiviewConfig::validate`) already
    /// rejects a malformed entry with a hard error, so in production this only
    /// canonicalizes — the drop is a defense-in-depth backstop for any direct
    /// constructor (e.g. a test), closing the panel's "match parsed canonical
    /// origins so null/malformed can never be allowed by any construction path".
    #[must_use]
    pub fn new(configured: Vec<String>) -> Self {
        Self {
            configured: configured
                .into_iter()
                .filter_map(|origin| Origin::parse(&origin).ok())
                .collect(),
        }
    }

    /// Whether `origin` (the request `Origin` header) is permitted given the request
    /// `Host`. The `Origin` is strictly parsed first: `Origin: null`, a
    /// path/query/fragment/userinfo, a non-http(s) scheme, or any other malformed
    /// value is denied outright (fail-closed). A configured allow-list match wins;
    /// otherwise the parsed origin's authority (`host[:port]`) must equal the
    /// `Host` (same-origin — the embed-web SPA served from the appliance, zero
    /// config).
    #[must_use]
    pub fn permits(&self, origin: &str, host: Option<&str>) -> bool {
        let Ok(origin) = Origin::parse(origin) else {
            return false; // `null`/opaque/path-bearing/bad-scheme — fail-closed
        };
        if self.configured.contains(&origin) {
            return true;
        }
        match host {
            Some(host) => origin.matches_host(host),
            None => false,
        }
    }
}

/// Reject a cross-origin realtime upgrade (SEC-13 / CSWSH), on **both** WS and SSE
/// and **regardless of `auth_disabled`** (CSWSH needs no credential).
///
/// An **absent** `Origin` passes: a non-browser client is not an SOP subject and
/// not a CSWSH vector, and browsers always send `Origin` on a WS/SSE handshake. A
/// **present** `Origin` must pass [`AllowedOrigins::permits`] — and a present but
/// **unreadable** one (bytes that are not valid UTF-8, so no legitimate browser
/// origin) is denied, NOT folded into the absent case (fail-closed). A request
/// carrying **more than one** `Origin` header is denied as ambiguous (a browser
/// sends exactly one).
fn enforce_origin(state: &AppState, headers: &HeaderMap) -> Result<(), crate::error::ControlError> {
    // Read ALL `Origin` values. Absent → native client, allow. A browser sends
    // EXACTLY one; more than one is an ambiguous/smuggled request, and admitting on
    // whichever value this gate reads while a downstream component trusts another is
    // a CSWSH bypass — so a duplicate `Origin` fails closed.
    let mut origins = headers.get_all(header::ORIGIN).iter();
    let Some(origin_value) = origins.next() else {
        return Ok(()); // absent: native client, not a CSWSH subject
    };
    if origins.next().is_some() {
        return Err(crate::error::ControlError::Forbidden(
            "cross-origin realtime connection refused (multiple Origin headers)".to_owned(),
        ));
    }
    // Present but not valid UTF-8 → not a real browser `Origin`; fail closed
    // rather than treat it as absent (defect #3).
    let Ok(origin) = origin_value.to_str() else {
        return Err(crate::error::ControlError::Forbidden(
            "cross-origin realtime connection refused (unreadable Origin header)".to_owned(),
        ));
    };
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    if state.allowed_origins.permits(origin, host) {
        Ok(())
    } else {
        Err(crate::error::ControlError::Forbidden(format!(
            "cross-origin realtime connection refused (Origin {origin:?}); \
             set control.allowed_origins to permit it"
        )))
    }
}

/// The `POST /api/v1/ws/ticket` response: a short-lived, single-use realtime auth
/// ticket the browser passes as `?ticket=` on the WS/SSE upgrade (ADR-RT011).
#[derive(Debug, Clone, serde::Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct WsTicketResponse {
    /// The opaque single-use ticket. Present it once as `?ticket=` within
    /// `expires_in_secs`; it is consumed on the first WS/SSE upgrade.
    pub ticket: String,
    /// Seconds until the ticket expires ([`WS_TICKET_TTL`]).
    pub expires_in_secs: u64,
}

/// `POST /api/v1/ws/ticket` — mint a short-lived, single-use realtime auth ticket
/// for the browser transports (ADR-RT011, implementing ADR-RT005).
///
/// Authenticated exactly like a REST call — `Authorization: Bearer` header or JWT,
/// **never** a URL query (that is the SEC-01 leak this replaces). The ticket
/// carries the caller's full [`Principal`] (role + every scope axis) and its
/// RT010 baseline, so the stream it opens has exactly the authorization the bearer
/// would; the read gate ([`Action::Read`]) matches the WS/SSE gate. The durable
/// bearer therefore never appears in a WS/SSE URL.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/ws/ticket",
        tag = "realtime",
        // Authenticated exactly like a REST call: an `Authorization: Bearer`
        // API key or JWT (never a URL query — that is the SEC-01 leak this
        // replaces). Declared so the generated client carries typed auth.
        security(("bearer_auth" = [])),
        responses(
            (status = 200, description = "A short-lived single-use realtime ticket to present as `?ticket=` on the WS/SSE upgrade.", body = WsTicketResponse),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read (below the viewer role).", body = crate::problem::Problem),
        ),
    )
)]
pub async fn ws_ticket_handler(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let (principal, baseline) = match resolve_principal(&state, &headers, None) {
        Ok(resolved) => resolved,
        Err(err) => return err.into_response(),
    };
    if let Err(err) = principal.role.require(Action::Read) {
        return err.into_response();
    }
    let ticket = state.ws_tickets.mint(principal, baseline);
    axum::Json(WsTicketResponse {
        ticket,
        expires_in_secs: WS_TICKET_TTL.as_secs(),
    })
    .into_response()
}

/// The body of `GET /api/v1/auth/status` — the **unauthenticated** discovery
/// endpoint the SPA reads to decide whether to prompt for an API key (and to
/// validate one).
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct AuthStatus {
    /// Whether a verified credential is required to reach privileged routes
    /// (`false` only when the operator explicitly disabled auth).
    pub auth_required: bool,
    /// Whether the credential presented on THIS request authenticates — so the SPA
    /// can validate an entered key by calling this endpoint with it. Always `true`
    /// when auth is disabled.
    pub authenticated: bool,
}

/// `GET /api/v1/auth/status` — report whether authentication is required and
/// whether the presented `Authorization: Bearer` credential authenticates.
/// Deliberately **unauthenticated**: the SPA must reach it before it holds a token.
/// It leaks nothing beyond the two booleans.
///
/// Header-only: a durable bearer is **never** accepted in the URL query (SEC-01) —
/// the SPA validates an entered key by sending it in the `Authorization` header.
pub async fn auth_status_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> axum::Json<AuthStatus> {
    let authenticated = resolve_principal(&state, &headers, None).is_ok();
    axum::Json(AuthStatus {
        auth_required: state.auth_required(),
        authenticated,
    })
}

/// Install live re-authorization on `session` **iff** the principal is a
/// store-managed API key (ADR-RT010).
///
/// `live_baseline` (from [`resolve_principal`]) is `Some(generation)` for a
/// store-managed API-key principal and `None` for a local-admin (auth disabled) or
/// JWT principal — which are not store-revocable and keep their connect-time
/// authorization (a JWT denylist is separate future work). The handle is installed
/// for every store key, so even a key revoked in the connect window disconnects on
/// the first re-resolution. Consumes the `Principal` (its `key_id`/`role` move into
/// the session's live-authz handle).
fn install_live_reauth(
    session: SessionStream,
    state: &AppState,
    principal: Principal,
    live_baseline: Option<u64>,
) -> SessionStream {
    let Principal {
        key_id,
        role,
        scoped_object_ids,
        scoped_output_ids,
        scoped_discovery_domains,
    } = principal;
    // Carry ALL THREE authorization axes into the live session (ADR-W026): the
    // pre-W026 destructure dropped output + discovery via `..`, so those axes
    // were dead on the realtime stream even though the connecting principal
    // carried them.
    let session = session.with_scopes(
        scoped_object_ids,
        scoped_output_ids,
        scoped_discovery_domains,
    );
    // `live_baseline` is `Some` exactly for store-managed API-key principals
    // (`resolve_principal`), carrying the generation captured at auth. Install the
    // live-authz handle UNCONDITIONALLY for them — even if the key was already
    // revoked in the connect window (`principal_for_key` would now be `None`) — so
    // the connect-race gate / first reauthorize disconnects. The old racy
    // `principal_for_key(&key_id).is_some()` re-probe conflated "revoked store key"
    // with "not a store key" and silently skipped the handle, stranding a
    // revoked-in-window session authorized forever. Local-admin and JWT principals
    // (`None`) are not store-revocable and keep their connect-time authorization.
    match live_baseline {
        Some(baseline) => {
            session.with_live_reauth_at(Arc::clone(&state.api_keys), key_id, role, baseline)
        }
        None => session,
    }
}

/// Build the frame burst the transport sends on a mid-session scope change
/// (ADR-RT010, [`ReauthOutcome::ScopeChanged`]): the `$resync` rebuild directive
/// followed by the connect snapshot set (tiles + per-device status), now filtered
/// to the session's freshly-adopted scope — so the client rebuilds to exactly what
/// a fresh connect under the new scope would show. All reads are wait-free
/// control-plane loads (invariant #10).
fn build_resync_frames(session: &mut SessionStream, state: &AppState) -> Vec<RealtimeFrame> {
    let (snapshot, snapshot_seq) = current_engine_snapshot(state);
    let mut frames = vec![session.resync_frame(snapshot_seq)];
    if let Some(frame) = session.tiles_snapshot_frame(&snapshot, snapshot_seq) {
        frames.push(frame);
    }
    frames.extend(session.devices_snapshot_frames(&state.device_status, snapshot_seq));
    frames
}

/// Serialize a realtime frame and write it as a WebSocket text frame. Returns
/// `false` when the client write failed (drop the session); a serialization
/// failure skips the frame and returns `true`. The engine is never blocked by
/// either (invariant #10).
async fn ws_send_frame(socket: &mut WebSocket, frame: &RealtimeFrame) -> bool {
    let Ok(text) = frame.to_json() else {
        return true;
    };
    socket.send(Message::Text(text.into())).await.is_ok()
}

/// The WebSocket close frame for a mid-session authorization revocation (ADR-RT010).
fn forbidden_close_frame() -> CloseFrame {
    CloseFrame {
        code: WS_CLOSE_FORBIDDEN,
        reason: "authz revoked".into(),
    }
}

/// Run one upgraded WebSocket session to completion.
///
/// `principal` is the connecting principal: its object scope confines the stream
/// (BOLA visibility, ADR-W005/ADR-W025) and, for a store-managed API key, its
/// authorization is re-resolved live so a mid-session narrow/widen/revoke takes
/// effect without a reconnect (ADR-RT010).
async fn run_ws_session(
    mut socket: WebSocket,
    state: AppState,
    principal: Principal,
    live_baseline: Option<u64>,
) {
    let sub = state.engine.subscribe();
    let session_id = uuid_session_id();
    // Capture the broadcast watermark BETWEEN subscribing and reading the snapshot
    // (ADR-RT009). Subscribing first means no event published after the watermark
    // is missed; capturing the watermark before the snapshot read means — because
    // the engine publishes state-then-event (runtime.rs) — the snapshot reflects
    // every event with `seq <= watermark`, so dropping those deltas loses nothing.
    // Both reads are wait-free atomic loads that never touch the engine publish
    // path (invariant #10).
    let watermark = state.engine.events.sequence();
    let (snapshot, snapshot_seq) = current_engine_snapshot(&state);
    let session = SessionStream::new(sub, session_id, None)
        .with_corr_registry(Arc::clone(&state.corr))
        .with_snapshot_watermark(watermark);
    // Carry the object scope + wire live re-authorization for store-managed keys.
    let mut session = install_live_reauth(session, &state, principal, live_baseline);

    // Connect-race gate (ADR-RT010): re-resolve against the auth-time generation
    // BEFORE building the snapshot, so a revoke/re-scope that landed in the
    // auth→install window is honored up front — a revoked key closes here (no
    // snapshot at all), a re-scope adopts the new scope so the snapshot below is
    // filtered to it. A change landing in the sub-tick window AFTER this gate is
    // caught by the first pump iteration below — the same bounded latency the
    // per-delta re-check gives the steady-state stream. Fully closing that residual
    // window would require holding the store lock across the socket send, which
    // invariant #10 forbids, so bounded-latency is the correct guarantee here.
    match session.reauthorize() {
        ReauthOutcome::Disconnect => {
            // The socket is being torn down; a failed close-frame send only means the
            // peer already went away, so the send result is intentionally ignored.
            let _ = socket
                .send(Message::Close(Some(forbidden_close_frame())))
                .await;
            return;
        }
        ReauthOutcome::Unchanged | ReauthOutcome::ScopeChanged => {}
    }

    let hello = session.snapshot_frame(snapshot_seq);
    if !ws_send_frame(&mut socket, &hello).await {
        return;
    }
    // Seed the tile cache: the current per-tile lifecycle baseline, when the
    // engine blob carries one (realtime-api §5). Without it the client falls
    // back to the sparse deltas, exactly as before.
    if let Some(frame) = session.tiles_snapshot_frame(&snapshot, snapshot_seq) {
        if !ws_send_frame(&mut socket, &frame).await {
            return;
        }
    }
    // Seed the device cache: one latest-wins `device.status` snapshot per tracked
    // device (ADR-RT007). The conflated status lane is excluded from the lossless
    // replay ring, so this re-snapshot is the only way a connecting client learns
    // current device status. Reading the registry is a wait-free control-plane
    // load (invariant #10).
    for frame in session.devices_snapshot_frames(&state.device_status, snapshot_seq) {
        if !ws_send_frame(&mut socket, &frame).await {
            return;
        }
    }

    // Delta pump with live re-authorization (ADR-RT010). Each iteration wakes on a
    // delta OR the idle re-auth tick, then re-resolves authorization BEFORE
    // projecting/sending — so a mid-session scope change filters the very next
    // delta (no leak, gapless seq), a revoke tears the session down, and an idle
    // stream still honors a change within one tick. `recv_event().await` is
    // cancel-safe, and the engine is never awaited (invariant #10).
    let mut reauth_tick = tokio::time::interval(REAUTH_TICK);
    reauth_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let step = tokio::select! {
            _ = reauth_tick.tick() => None,
            step = session.recv_event() => Some(step),
            // Observe the client socket so a client-initiated close OR a
            // serve-shutdown-driven transport EOF (the shutdown-aware `TrackedStream`
            // returns EOF at drain; SEC-14 #126 R2 F1) ends this otherwise write-only
            // firehose session promptly, releasing its connection slot + concurrency
            // permit. Without this an idle session (no deltas to write) never notices
            // the socket closing until it next writes — which, for a silent engine, is
            // never. `recv()` is cancel-safe; a client frame carries no state for this
            // read-only transport, so it is dropped and the engine is never touched
            // (invariant #10).
            incoming = socket.recv() => match incoming {
                None | Some(Ok(Message::Close(_)) | Err(_)) => break,
                Some(Ok(_)) => continue,
            },
        };
        match session.reauthorize() {
            ReauthOutcome::Unchanged => {}
            ReauthOutcome::Disconnect => {
                let _ = socket
                    .send(Message::Close(Some(forbidden_close_frame())))
                    .await;
                return;
            }
            ReauthOutcome::ScopeChanged => {
                for frame in build_resync_frames(&mut session, &state) {
                    if !ws_send_frame(&mut socket, &frame).await {
                        return;
                    }
                }
            }
        }
        match step {
            // A re-auth tick (re-resolution already ran above) or a resume/lag
            // skip: nothing to send this iteration — loop and re-arm.
            None | Some(RecvStep::Skipped) => {}
            Some(RecvStep::Event(seq_event)) => {
                if let Some(frame) = session.frame_for(&seq_event) {
                    if !ws_send_frame(&mut socket, &frame).await {
                        // The client write failed: drop this session. The engine
                        // was never blocked by it.
                        break;
                    }
                }
            }
            Some(RecvStep::Closed) => break,
        }
    }
}

/// `GET /api/v1/events` — the one-way SSE fallback transport.
///
/// **Authenticated** exactly like the WebSocket transport: the `Authorization`
/// header for native clients, or a single-use `?ticket=` for browsers (a durable
/// bearer is never accepted in the URL query — SEC-01). Streaming the engine event
/// firehose requires at least [`Action::Read`] ([`Role::Viewer`]). The same
/// [`AllowedOrigins`] CSWSH gate as the WebSocket runs first, even in
/// `auth_disabled` mode. An out-of-policy origin is a `403`; an unauthenticated or
/// under-privileged request a `401`/`403` `problem+json` — all before any event.
///
/// On success, emits the same snapshot-then-delta envelope stream as named SSE
/// events. The underlying [`Sse`] body applies no back-pressure to the engine:
/// it pulls from the per-client [`SessionStream`], which lagged-skips a slow
/// consumer.
///
/// [`Role::Viewer`]: crate::auth::Role::Viewer
pub async fn sse_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<TicketQuery>,
) -> Response {
    // CSWSH gate FIRST (SEC-13), independent of auth so it also holds when auth is
    // disabled — parity with the WebSocket transport.
    if let Err(err) = enforce_origin(&state, &headers) {
        return err.into_response();
    }
    let (principal, live_baseline) =
        match resolve_principal(&state, &headers, query.ticket.as_deref()) {
            Ok(resolved) => resolved,
            Err(err) => return err.into_response(),
        };
    if let Err(err) = principal.role.require(Action::Read) {
        return err.into_response();
    }

    let sub = state.engine.subscribe();
    let session_id = uuid_session_id();
    // Capture the broadcast watermark between subscribe and the snapshot read, and
    // pair it with the snapshot (ADR-RT009) — identical to the WebSocket transport:
    // subscribe-first misses no post-watermark event; watermark-before-snapshot
    // (state-then-event publish ordering) means the snapshot reflects every event
    // with `seq <= watermark`; and both reads are wait-free atomic loads that never
    // touch the engine publish path (invariant #10).
    let watermark = state.engine.events.sequence();
    let (snapshot, snapshot_seq) = current_engine_snapshot(&state);
    // The authenticated principal's object scope confines the SSE stream to its
    // in-scope device/cast objects (BOLA visibility, ADR-W005/ADR-W025), exactly
    // as the WebSocket transport; an unscoped principal sees all. Store-managed
    // API-key principals additionally re-resolve live (ADR-RT010).
    let session = SessionStream::new(sub, session_id, None)
        .with_corr_registry(Arc::clone(&state.corr))
        .with_snapshot_watermark(watermark);
    let mut session = install_live_reauth(session, &state, principal, live_baseline);

    // Connect-race gate (ADR-RT010): re-resolve against the auth-time generation
    // before the stream builds any snapshot — a revoke that landed in the auth→install
    // window ends the request with a 403 (no snapshot); a re-scope is adopted so the
    // snapshot is filtered to the new scope. A change landing after this gate is caught
    // by the first pump iteration (bounded latency; a fully atomic authz+snapshot would
    // need a lock across the send, which invariant #10 forbids).
    if session.reauthorize() == ReauthOutcome::Disconnect {
        return crate::error::ControlError::Forbidden("authorization revoked".to_owned())
            .into_response();
    }

    let stream = async_stream::stream! {
        let hello = session.snapshot_frame(snapshot_seq);
        if let Ok(text) = hello.to_json() {
            yield Ok::<_, Infallible>(SseEvent::default().event("snapshot").data(text));
        }
        // Seed the tile cache: the current per-tile lifecycle baseline, when
        // the engine blob carries one (realtime-api §5) — labelled
        // `event: snapshot` exactly like `$hello`.
        if let Some(frame) = session.tiles_snapshot_frame(&snapshot, snapshot_seq) {
            if let Ok(text) = frame.to_json() {
                yield Ok(SseEvent::default().event("snapshot").data(text));
            }
        }
        // Seed the device cache: one latest-wins `device.status` snapshot per
        // tracked device (ADR-RT007), labelled `event: snapshot` exactly like
        // `$hello`. The conflated status lane is ring-excluded, so this
        // re-snapshot is the only way a connecting client learns current status.
        for frame in session.devices_snapshot_frames(&state.device_status, snapshot_seq) {
            if let Ok(text) = frame.to_json() {
                yield Ok(SseEvent::default().event("snapshot").data(text));
            }
        }
        // Delta pump with live re-authorization (ADR-RT010), identical in shape to
        // the WebSocket transport: wake on a delta OR the idle re-auth tick,
        // re-resolve BEFORE projecting, emit a `$resync` rebuild burst on a scope
        // change, and end the stream on a revoke (the client re-auths on reconnect;
        // SSE has no close code). The engine is never awaited (invariant #10).
        let mut reauth_tick = tokio::time::interval(REAUTH_TICK);
        reauth_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let step = tokio::select! {
                _ = reauth_tick.tick() => None,
                step = session.recv_event() => Some(step),
            };
            match session.reauthorize() {
                ReauthOutcome::Unchanged => {}
                ReauthOutcome::Disconnect => break,
                ReauthOutcome::ScopeChanged => {
                    for frame in build_resync_frames(&mut session, &state) {
                        if let Ok(text) = frame.to_json() {
                            yield Ok(SseEvent::default().event("snapshot").data(text));
                        }
                    }
                }
            }
            match step {
                // A re-auth tick or a resume/lag skip: nothing to send this
                // iteration — loop and re-arm.
                None | Some(RecvStep::Skipped) => {}
                Some(RecvStep::Event(seq_event)) => {
                    if let Some(frame) = session.frame_for(&seq_event) {
                        if let Ok(text) = frame.to_json() {
                            let id = frame.envelope.seq.get().to_string();
                            yield Ok(SseEvent::default().event("delta").id(id).data(text));
                        }
                    }
                }
                Some(RecvStep::Closed) => break,
            }
        }
    };

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// Mint a fresh session id.
fn uuid_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod topic_routing_tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::topic_for_event;
    use multiview_core::stream::StreamInventory;
    use multiview_events::{
        AudioLoudness, Event, InputStreams, RistLinkRole, RistLinkStats, SystemMetrics, Topic,
    };

    /// A RIST link-stats sample MUST ride the `outputs` lane (a RIST egress
    /// link's health is an output-sink concern) — never the `$control`
    /// catch-all — and be a conflated latest-wins telemetry sample (excluded
    /// from the lossless replay ring; pushed, never polled — inv #10).
    #[test]
    fn rist_link_stats_routes_to_the_outputs_topic_and_is_conflated() {
        let event = Event::RistLinkStats(RistLinkStats {
            link_id: "out-rist".to_owned(),
            role: RistLinkRole::Sender,
            flow_id: 1,
            cname: "egress".to_owned(),
            peer_count: 1,
            rtt_ms: 40,
            quality: 99.0,
            bandwidth_bps: 1_000_000,
            retry_bandwidth_bps: 1_000,
            sent: 100,
            received: 0,
            retransmitted: 2,
            lost: 0,
            recovered: 2,
            since: 1,
        });
        assert_eq!(topic_for_event(&event), Topic::Outputs);
        assert!(event.is_conflated(), "rist link stats is latest-wins");
    }

    /// The `input.streams` inventory-discovery event MUST ride the existing
    /// `inputs` lane (RT-3) — a delta on re-probe / PMT-version bump, not a new
    /// topic — so a client already subscribed to `inputs` sees it without a new
    /// subscription.
    #[test]
    fn input_streams_routes_to_the_inputs_topic() {
        let event = Event::InputStreams(InputStreams::new(
            "cam1",
            StreamInventory::new().with_input_id("cam1"),
        ));
        assert_eq!(topic_for_event(&event), Topic::Inputs);
        // The inputs lane is lossless (NOT a high-rate conflated lane): an
        // inventory delta must survive in the replay ring.
        assert!(!Topic::Inputs.is_high_rate());
    }

    /// High-rate whole-system metrics MUST route to the conflated `system` lane
    /// the footer subscribes to — never the `$control` catch-all. (Regression:
    /// `SystemMetrics` previously fell through to `_ => Topic::Control`, so the
    /// pushed samples never reached the `system` topic and the footer stayed
    /// empty.)
    #[test]
    fn system_metrics_routes_to_the_system_topic() {
        let event = Event::SystemMetrics(SystemMetrics {
            cpu_util: 0.4,
            mem_used_bytes: None,
            mem_total_bytes: None,
            self_cpu_util: None,
            self_mem_used_bytes: None,
            gpus: vec![],
            program_fps: None,
            sampled_hz: 1,
        });
        assert_eq!(topic_for_event(&event), Topic::System);
        // And `system` is a high-rate conflated lane (pushed, never polled).
        assert!(Topic::System.is_high_rate());
    }

    /// The program-bus loudness compliance lane (AUD-8) MUST route to its own
    /// conflated `audio.loudness` topic (so the meter subscribes independently of
    /// the per-track peak/RMS meters) and be a high-rate, conflated, latest-wins
    /// sample (excluded from the lossless replay ring; pushed, never polled —
    /// inv #10).
    #[test]
    fn audio_loudness_routes_to_the_audio_loudness_topic_and_is_conflated() {
        let event = Event::AudioLoudness(AudioLoudness {
            program: 0,
            momentary: Some(-22.5),
            short_term: Some(-23.0),
            integrated: Some(-23.0),
            lra: Some(4.0),
            true_peak_dbtp: Some(-2.5),
            target_lufs: -23.0,
            ceiling_dbtp: -1.5,
            tolerance_lu: 1.0,
            gain_db: Some(0.0),
            sampled_hz: 10,
        });
        assert_eq!(topic_for_event(&event), Topic::AudioLoudness);
        assert!(
            Topic::AudioLoudness.is_high_rate(),
            "audio.loudness is a conflated high-rate lane"
        );
        assert!(
            event.is_conflated(),
            "a loudness sample is conflated latest-wins, never lossless"
        );
    }

    /// A shed-load decision is a discrete, lossless degradation-signal event:
    /// it MUST route to the `alerts` lane (sibling to health warnings), NOT the
    /// conflated `system` lane — so every shed stays in the lossless replay ring
    /// and the §7.2 retention store records it (it is `!is_conflated`).
    #[test]
    fn shed_load_routes_to_the_alerts_topic_and_is_lossless() {
        let event = Event::ShedLoad(multiview_events::ShedLoad {
            reason: multiview_events::ShedReason::EncoderOverload,
            scope: multiview_events::ShedScope::Program,
            level: 1,
            dropped: 3,
        });
        assert_eq!(topic_for_event(&event), Topic::Alerts);
        assert!(
            !Topic::Alerts.is_high_rate(),
            "shed events must stay in the lossless replay ring"
        );
        assert!(
            !event.is_conflated(),
            "a shed is a discrete lossless event, never conflated"
        );
    }

    /// Every Devices-domain event (ADR-RT007) MUST route to the one coarse
    /// `devices` lane — never the `$control` catch-all — so the SPA Devices
    /// page subscribes once and switches exhaustively on `t`.
    #[test]
    fn device_events_route_to_the_devices_topic() {
        use multiview_core::time::Rational;
        use multiview_core::wallclock::WallClockRef;
        use multiview_events::{
            AddressFamily, ClockQuality, ClockSource, DeviceAdopted, DeviceDiscovered, DeviceError,
            DeviceMode, DeviceRemoved, DeviceState, DeviceStatus, DeviceSync, ImpactClass,
            ModePhase, SyncChange, TimingStatus,
        };

        let events: Vec<Event> = vec![
            Event::DeviceStatus(DeviceStatus::new("dev-a", DeviceState::Online)),
            Event::DeviceAdopted(DeviceAdopted {
                device_id: "dev-a".to_owned(),
                driver: "zowietek".to_owned(),
                name: None,
            }),
            Event::DeviceRemoved(DeviceRemoved::new("dev-a")),
            Event::DeviceMode(DeviceMode {
                device_id: "dev-a".to_owned(),
                mode: "decoder".to_owned(),
                phase: ModePhase::Finished,
                impact: ImpactClass::Device,
                detail: None,
            }),
            Event::DeviceError(DeviceError {
                device_id: "dev-a".to_owned(),
                code: None,
                message: "probe failed".to_owned(),
            }),
            Event::DeviceSync(DeviceSync {
                device_id: "dev-a".to_owned(),
                group: "lobby-wall".to_owned(),
                change: SyncChange::Left,
            }),
            Event::DeviceDiscovered(DeviceDiscovered::new(
                "zowietek".to_owned(),
                "http://[fd00:db8::42]".to_owned(),
                AddressFamily::Ipv6,
            )),
            Event::TimingStatus(TimingStatus {
                stream_id: "prog-main".to_owned(),
                epoch: WallClockRef::new(0, 0, Rational::new(90_000, 1)),
                link_offset_ns: 0,
                clock_source: ClockSource::System,
                clock_quality: ClockQuality::Locked,
                groups: vec![],
            }),
        ];
        for event in events {
            assert_eq!(
                topic_for_event(&event),
                Topic::Devices,
                "{} must ride the coarse devices topic",
                event.type_tag()
            );
        }
        // The mixed-cadence topic stays out of the per-topic high-rate set:
        // ring exclusion is per-event (`Event::is_conflated`), per ADR-RT007.
        assert!(!Topic::Devices.is_high_rate());
    }

    /// Device events scope their envelope `id` by device id (status/lifecycle)
    /// or stream id (`timing.status`), so the existing `ids` filter narrows the
    /// coarse topic to a detail view (ADR-RT007).
    #[test]
    fn device_events_scope_their_envelope_id() {
        use multiview_core::time::Rational;
        use multiview_core::wallclock::WallClockRef;
        use multiview_events::{
            AddressFamily, ClockQuality, ClockSource, DeviceDiscovered, DeviceRemoved, DeviceState,
            DeviceStatus, TimingStatus,
        };

        use super::event_scope_id;

        let status = Event::DeviceStatus(DeviceStatus::new("dev-a", DeviceState::Online));
        assert_eq!(event_scope_id(&status).as_deref(), Some("dev-a"));

        let removed = Event::DeviceRemoved(DeviceRemoved::new("dev-a"));
        assert_eq!(event_scope_id(&removed).as_deref(), Some("dev-a"));

        let timing = Event::TimingStatus(TimingStatus {
            stream_id: "prog-main".to_owned(),
            epoch: WallClockRef::new(0, 0, Rational::new(90_000, 1)),
            link_offset_ns: 0,
            clock_source: ClockSource::Ptp,
            clock_quality: ClockQuality::Acquiring,
            groups: vec![],
        });
        assert_eq!(event_scope_id(&timing).as_deref(), Some("prog-main"));

        // A discovery row has no registry id yet — it is scoped by `corr`
        // (the scan operation), never by a fabricated id.
        let discovered = Event::DeviceDiscovered(DeviceDiscovered::new(
            "zowietek".to_owned(),
            "http://[fd00:db8::42]".to_owned(),
            AddressFamily::Ipv6,
        ));
        assert_eq!(event_scope_id(&discovered), None);
    }
}

#[cfg(test)]
mod media_player_correlation_tests {
    //! Correlation + topic routing for the VT vamp/exit media-player surface
    //! (ADR-0097 §6, ADR-RT008): `CorrKey::MediaPlayer { player, state }`
    //! mirrors `CorrKey::Salvo { salvo, phase }` — the key is derived
    //! identically from the command (at 202 time) and from the matching
    //! `media.player_state` outcome event, so the realtime layer recovers the
    //! op id without an op id ever entering the `Event` enum.
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::{event_scope_id, topic_for_event, CorrKey};
    use crate::command::{Command, MediaTransportVerb, OperationId};
    use multiview_events::{Event, MediaPlayerEvent, MediaPlayerState, Topic};

    #[test]
    fn arm_media_exit_keys_the_armed_vamp_state() {
        // Arming the exit stages the transition to `Vamping { exit_armed: true }`
        // — exactly the salvo `Armed` analogue (ADR-0097 §3/§6).
        let cmd = Command::ArmMediaExit {
            op: OperationId::new(),
            player: "vt-1".to_owned(),
        };
        assert_eq!(
            CorrKey::for_command(&cmd),
            Some(CorrKey::MediaPlayer {
                player: "vt-1".to_owned(),
                state: MediaPlayerState::Vamping { exit_armed: true },
            })
        );
    }

    #[test]
    fn take_media_exit_keys_the_armed_vamp_state() {
        // Take is functionally arm-then-soonest-boundary (ADR-0097 §3): its
        // unambiguous outcome is still the armed vamp state.
        let cmd = Command::TakeMediaExit {
            op: OperationId::new(),
            player: "vt-1".to_owned(),
        };
        assert_eq!(
            CorrKey::for_command(&cmd),
            Some(CorrKey::MediaPlayer {
                player: "vt-1".to_owned(),
                state: MediaPlayerState::Vamping { exit_armed: true },
            })
        );
    }

    #[test]
    fn cancel_media_exit_keys_the_unarmed_vamp_state() {
        let cmd = Command::CancelMediaExit {
            op: OperationId::new(),
            player: "vt-1".to_owned(),
        };
        assert_eq!(
            CorrKey::for_command(&cmd),
            Some(CorrKey::MediaPlayer {
                player: "vt-1".to_owned(),
                state: MediaPlayerState::Vamping { exit_armed: false },
            })
        );
    }

    #[test]
    fn transport_play_pause_stop_cue_key_their_unambiguous_state() {
        let player = "vt-1".to_owned();
        let cases = [
            (MediaTransportVerb::Play, MediaPlayerState::Playing),
            (MediaTransportVerb::Pause, MediaPlayerState::Paused),
            (MediaTransportVerb::Stop, MediaPlayerState::Stopped),
            (
                MediaTransportVerb::Cue { frame: None },
                MediaPlayerState::Cued,
            ),
            (
                MediaTransportVerb::Cue { frame: Some(120) },
                MediaPlayerState::Cued,
            ),
        ];
        for (verb, state) in cases {
            let cmd = Command::MediaTransport {
                op: OperationId::new(),
                player: player.clone(),
                verb,
            };
            assert_eq!(
                CorrKey::for_command(&cmd),
                Some(CorrKey::MediaPlayer {
                    player: player.clone(),
                    state,
                })
            );
        }
    }

    #[test]
    fn transport_load_and_seek_are_uncorrelated() {
        // `load` may resolve to Loading then Cued (ambiguous) and `seek` leaves
        // the player in its current state (no single outcome) — both return
        // None rather than mis-correlate (operator instruction; ADR-0097 §6
        // "single unambiguous outcome").
        let load = Command::MediaTransport {
            op: OperationId::new(),
            player: "vt-1".to_owned(),
            verb: MediaTransportVerb::Load {
                asset: "opener".to_owned(),
            },
        };
        assert_eq!(CorrKey::for_command(&load), None);

        let seek = Command::MediaTransport {
            op: OperationId::new(),
            player: "vt-1".to_owned(),
            verb: MediaTransportVerb::Seek { frame: Some(48) },
        };
        assert_eq!(CorrKey::for_command(&seek), None);
    }

    #[test]
    fn media_player_state_event_projects_to_the_same_key() {
        // The outcome event projects to the SAME key the arm command produced,
        // so `for_command` (record at 202) and `for_event` (project at delivery)
        // round-trip — the salvo correlation discipline, for media players.
        let armed = Command::ArmMediaExit {
            op: OperationId::new(),
            player: "vt-1".to_owned(),
        };
        let event = Event::MediaPlayerState(
            MediaPlayerEvent::new("vt-1", MediaPlayerState::Vamping { exit_armed: true }, 240)
                .with_asset("opener"),
        );
        assert_eq!(
            CorrKey::for_command(&armed),
            CorrKey::for_event(&event),
            "arm command and its media.player_state outcome share a corr key"
        );
    }

    #[test]
    fn media_player_state_routes_to_the_switcher_topic_lossless() {
        // ADR-RT008: media.player_state is a lossless lifecycle event on the
        // `switcher` topic — NOT the `$control` catch-all, and NOT high-rate.
        let event =
            Event::MediaPlayerState(MediaPlayerEvent::new("vt-1", MediaPlayerState::Playing, 12));
        assert_eq!(topic_for_event(&event), Topic::Switcher);
        assert!(
            !Topic::Switcher.is_high_rate(),
            "the switcher lane is lossless (stays in the replay ring)"
        );
        assert!(
            !event.is_conflated(),
            "a media-player lifecycle transition is lossless, never conflated"
        );
    }

    #[test]
    fn media_player_state_scopes_its_envelope_id_by_player() {
        // The envelope `id` scope is the player id (ADR-RT008), so the `ids`
        // filter narrows the coarse `switcher` topic to one player.
        let event =
            Event::MediaPlayerState(MediaPlayerEvent::new("vt-2", MediaPlayerState::Cued, 0));
        assert_eq!(event_scope_id(&event).as_deref(), Some("vt-2"));
    }
}

#[cfg(test)]
mod object_authz_scope_tests {
    //! The realtime per-object authorization axis (BOLA visibility,
    //! ADR-W005/ADR-W025): the scope-id helper must return `Some(id)` for
    //! EXACTLY the events whose id a scoped principal is checked against by
    //! `authorize_object` on the matching REST read — so the realtime stream
    //! never delivers an object a principal could not individually GET.
    #![allow(clippy::unwrap_used, clippy::panic)]

    use multiview_events::{
        AuthzScope, DeviceState, DeviceStatus, Event, LifecycleState, MediaPlayerEvent,
        MediaPlayerState, TileState,
    };

    /// A media player is object-scoped on REST (`get_player`/`play`/`pause` all
    /// `authorize_object(&player_id)`), and `MediaPlayerEvent.player` IS that
    /// same id, so `authz_scope()` MUST classify it `Object(player)` — else a
    /// scoped principal denied `GET /media/players/{id}` still streams that
    /// player's transport transitions over `media.player_state` (a BOLA leak).
    #[test]
    fn media_player_state_is_object_scoped_by_player() {
        let event =
            Event::MediaPlayerState(MediaPlayerEvent::new("vt-2", MediaPlayerState::Cued, 0));
        assert_eq!(
            event.authz_scope(),
            AuthzScope::Object("vt-2"),
            "media.player_state must scope by the authorize_object player id (BOLA)"
        );
    }

    /// A tile binds an input; `get_input_streams` (`GET /inputs/{id}/streams`)
    /// `authorize_object(&input_id)`s, and `TileState.input` IS that same input
    /// id, so a `tile.state` carrying a bound input must scope by it — else a
    /// scoped principal observes out-of-scope inputs' tile transitions.
    #[test]
    fn tile_state_with_a_bound_input_is_object_scoped_by_input() {
        let event = Event::TileState(TileState {
            from: LifecycleState::Live,
            to: LifecycleState::NoSignal,
            input: Some("cam-7".to_owned()),
            trigger: "nosignal_timeout".to_owned(),
        });
        assert_eq!(
            event.authz_scope(),
            AuthzScope::Object("cam-7"),
            "tile.state must scope by the authorize_object input id when one is bound (BOLA)"
        );
    }

    /// A placeholder tile with no bound input carries no object id — it has
    /// nothing to authorize, so it stays on the role-gated firehose (`Public`).
    #[test]
    fn tile_state_without_an_input_carries_no_object_id() {
        let event = Event::TileState(TileState {
            from: LifecycleState::Reconnecting,
            to: LifecycleState::NoSignal,
            input: None,
            trigger: "placeholder".to_owned(),
        });
        assert_eq!(event.authz_scope(), AuthzScope::Public);
    }

    /// The device axis the classifier already covered must be preserved.
    #[test]
    fn device_object_axis_is_preserved() {
        let device = Event::DeviceStatus(DeviceStatus::new("dev-a", DeviceState::Online));
        assert_eq!(device.authz_scope(), AuthzScope::Object("dev-a"));
    }
}
