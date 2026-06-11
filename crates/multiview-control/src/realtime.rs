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
use std::sync::{Arc, Mutex};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Query, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use multiview_core::time::MediaTime;
use multiview_engine::{EventSubscription, RecvError, TryRecvError};
use multiview_events::{
    DeviceStatus, Envelope, Event, FrameKind, Hello, OutputRunState, SalvoPhase, SchemaVersion,
    Seq, TileSnapshotEntry, TilesSnapshot, Topic,
};

use crate::auth::{Action, Principal};
use crate::command::{Command, OperationId};
use crate::devices::DeviceStatusRegistry;
use crate::state::{AppState, EngineStateSnapshot};

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
        let tiles = tiles_from_engine_snapshot(snapshot)?;
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
        let status = registry.snapshot_all().into_iter().next()?;
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
        registry
            .snapshot_all()
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
        // Two read modes:
        // * **Resume replay** (`resume_after` set): drain the bounded broadcast
        //   ring **non-blocking** via `try_recv`. The gap is finite (it cannot
        //   exceed the ring), so `Empty` means the gap is fully replayed and we
        //   return `Ok(None)` immediately rather than awaiting — awaiting here
        //   would wedge a caller that polls past the gap. The session pump owns
        //   the replay→live-tail handoff (re-snapshot the conflated lanes, then
        //   read live with a `resume_after == None` stream). That handoff is not
        //   wired yet: no current caller sets `resume_after` — `SessionStream` is
        //   only constructed with `None` (the connect path), so this branch runs
        //   solely in the broadcaster resume tests until a `since_seq` cursor
        //   lands.
        // * **Live tail** (`resume_after == None`, the connect path): `await` the
        //   next event cooperatively. A slow client lags and is skipped; the
        //   engine is never back-pressured (invariant #10).
        if self.resume_after.is_some() {
            return match self.sub.try_recv() {
                Ok(seq_event) => Ok(self.frame_for(&seq_event)),
                // The gap is fully replayed: nothing more buffered. Return
                // promptly so a bounded drain loop terminates; the pump then
                // re-snapshots and reconnects for the live tail.
                Err(TryRecvError::Empty) => Ok(None),
                Err(TryRecvError::Lagged(_)) => {
                    self.sub = self.sub.resubscribe();
                    Ok(None)
                }
                Err(TryRecvError::Closed) => Err(RecvError::Closed),
            };
        }
        match self.sub.recv().await {
            Ok(seq_event) => Ok(self.frame_for(&seq_event)),
            Err(RecvError::Lagged(_)) => {
                // Drop-oldest overflow for THIS slow client only: resubscribe at
                // the head and let the client re-baseline. The engine never saw
                // any back-pressure (invariant #10).
                self.sub = self.sub.resubscribe();
                Ok(None)
            }
            Err(RecvError::Closed) => Err(RecvError::Closed),
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
        // `timing.status` scopes by the program/output stream the epoch maps.
        Event::TimingStatus(timing) => Some(timing.stream_id.clone()),
        // A discovery row has no registry id yet (untrusted inventory): it is
        // correlated to its scan operation via `corr`, never a fabricated id.
        _ => None,
    }
}

/// The coarse topic an event is published on (the realtime-api topic map).
#[must_use]
pub fn topic_for_event(event: &Event) -> Topic {
    match event {
        Event::TileState(_) | Event::TilesSnapshot(_) => Topic::Tiles,
        Event::AudioMeter(_) => Topic::AudioMeters,
        // High-rate whole-system metrics (cpu/gpu/encoder) ride the conflated
        // `system` lane the footer subscribes to — NOT the control firehose.
        Event::SystemMetrics(_) => Topic::System,
        Event::OutputStatus(_) => Topic::Outputs,
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
        | Event::TimingStatus(_) => Topic::Devices,
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
/// **Authenticated before the upgrade.** Streaming the engine event firehose
/// (tile state, alerts, input/output status) is a privileged read: a valid
/// `Bearer` API key with at least [`Action::Read`] ([`Role::Viewer`]) is
/// required. Auth is resolved *pre-upgrade* — the [`Principal`] extractor and
/// the role gate run before [`WebSocketUpgrade::on_upgrade`], so an
/// unauthenticated or under-privileged request fails as a debuggable
/// `401`/`403` `problem+json` HTTP response rather than a silently-closed socket
/// (realtime-api §6). API/non-browser clients send `Authorization: Bearer`
/// directly on the upgrade.
///
/// On success, upgrades the connection and runs a [`SessionStream`] that emits
/// the snapshot then streams deltas, writing each as a text frame. A write that
/// blocks blocks only this client's task; the engine is never awaited.
///
/// [`Role::Viewer`]: crate::auth::Role::Viewer
pub async fn ws_handler(
    // The auth gate is the FIRST extractor, so authentication is decided before
    // axum's `WebSocketUpgrade` extractor runs — an unauthenticated request is a
    // `401`/`403` `problem+json`, never pre-empted by the upgrade extractor's
    // `426` on a request without the upgrade handshake.
    RealtimeViewer(_principal): RealtimeViewer,
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
) -> Response {
    ws.on_upgrade(move |socket| run_ws_session(socket, state))
}

/// A pre-upgrade auth gate for the realtime transports.
///
/// As a [`FromRequestParts`] extractor it resolves the `Bearer` / JWT /
/// `?access_token=` [`Principal`] and enforces [`Action::Read`] — and because
/// extractors run in argument order, placing it before [`WebSocketUpgrade`] makes
/// authentication strictly precede the upgrade. So an unauthenticated /
/// under-privileged client always gets a debuggable `401`/`403` HTTP response,
/// not a `426` from the upgrade extractor (nor a silently-closed socket).
pub struct RealtimeViewer(pub Principal);

impl FromRequestParts<AppState> for RealtimeViewer {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // A browser WebSocket/EventSource cannot set an `Authorization` header,
        // so the bearer token is also accepted as `?access_token=` (header wins).
        let access_token = Query::<AccessTokenQuery>::from_request_parts(parts, state)
            .await
            .ok()
            .and_then(|q| q.0.access_token);
        let principal = resolve_principal(state, &parts.headers, access_token.as_deref())
            .map_err(IntoResponse::into_response)?;
        principal
            .role
            .require(Action::Read)
            .map_err(IntoResponse::into_response)?;
        Ok(Self(principal))
    }
}

/// The browser-transport token fallback: a WebSocket / `EventSource` cannot set
/// an `Authorization` header, so the bearer token may be passed as the
/// `access_token` query parameter (same-origin only).
#[derive(Debug, serde::Deserialize)]
pub struct AccessTokenQuery {
    /// The raw `key_id.secret` bearer token, when not sent as a header.
    access_token: Option<String>,
}

/// Resolve a [`Principal`] from the `Authorization` header (API key, then JWT) or,
/// failing that, the `access_token` query parameter (the browser WS/SSE path).
fn resolve_principal(
    state: &AppState,
    headers: &HeaderMap,
    access_token: Option<&str>,
) -> Result<Principal, crate::error::ControlError> {
    // Auth disabled (explicit trusted-network mode): the realtime stream is open
    // as a local admin, matching the REST `Principal` extractor.
    if state.auth_disabled {
        return Ok(Principal::local_admin());
    }
    let header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if let Ok(principal) = state.api_keys.verify_authorization(header) {
        return Ok(principal);
    }
    if let Some(Ok(principal)) = state.authenticate_jwt(header) {
        return Ok(principal);
    }
    if let Some(token) = access_token {
        return state.api_keys.verify(token);
    }
    Err(crate::error::ControlError::Unauthenticated)
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
/// whether the presented credential (header `Bearer` or `?access_token=`)
/// authenticates. Deliberately **unauthenticated**: the SPA must reach it before
/// it holds a token. It leaks nothing beyond the two booleans.
pub async fn auth_status_handler(
    State(state): State<AppState>,
    Query(access): Query<AccessTokenQuery>,
    headers: HeaderMap,
) -> axum::Json<AuthStatus> {
    let authenticated = resolve_principal(&state, &headers, access.access_token.as_deref()).is_ok();
    axum::Json(AuthStatus {
        auth_required: state.auth_required(),
        authenticated,
    })
}

/// Run one upgraded WebSocket session to completion.
async fn run_ws_session(mut socket: WebSocket, state: AppState) {
    let sub = state.engine.subscribe();
    let session_id = uuid_session_id();
    let mut session =
        SessionStream::new(sub, session_id, None).with_corr_registry(Arc::clone(&state.corr));

    let (snapshot, snapshot_seq) = current_engine_snapshot(&state);
    let hello = session.snapshot_frame(snapshot_seq);
    if let Ok(text) = hello.to_json() {
        if socket.send(Message::Text(text.into())).await.is_err() {
            return;
        }
    }
    // Seed the tile cache: the current per-tile lifecycle baseline, when the
    // engine blob carries one (realtime-api §5). Without it the client falls
    // back to the sparse deltas, exactly as before.
    if let Some(frame) = session.tiles_snapshot_frame(&snapshot, snapshot_seq) {
        if let Ok(text) = frame.to_json() {
            if socket.send(Message::Text(text.into())).await.is_err() {
                return;
            }
        }
    }
    // Seed the device cache: one latest-wins `device.status` snapshot per tracked
    // device (ADR-RT007). The conflated status lane is excluded from the lossless
    // replay ring, so this re-snapshot is the only way a connecting client learns
    // current device status. Reading the registry is a wait-free control-plane
    // load (invariant #10).
    for frame in session.devices_snapshot_frames(&state.device_status, snapshot_seq) {
        if let Ok(text) = frame.to_json() {
            if socket.send(Message::Text(text.into())).await.is_err() {
                return;
            }
        }
    }

    loop {
        match session.next_delta().await {
            Ok(Some(frame)) => {
                let Ok(text) = frame.to_json() else { continue };
                if socket.send(Message::Text(text.into())).await.is_err() {
                    // The client write failed: drop this session. The engine was
                    // never blocked by it.
                    break;
                }
            }
            Ok(None) => {}
            Err(_closed) => break,
        }
    }
}

/// `GET /api/v1/events` — the one-way SSE fallback transport.
///
/// **Authenticated** with the same `Bearer` API key as the WebSocket transport:
/// SSE uses the `Authorization` header (realtime-api §6), and streaming the
/// engine event firehose requires at least [`Action::Read`] ([`Role::Viewer`]).
/// An unauthenticated or under-privileged request is rejected with a
/// `401`/`403` `problem+json` response before any event is emitted.
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
    Query(auth): Query<AccessTokenQuery>,
) -> Response {
    let principal = match resolve_principal(&state, &headers, auth.access_token.as_deref()) {
        Ok(principal) => principal,
        Err(err) => return err.into_response(),
    };
    if let Err(err) = principal.role.require(Action::Read) {
        return err.into_response();
    }

    let sub = state.engine.subscribe();
    let session_id = uuid_session_id();
    let mut session =
        SessionStream::new(sub, session_id, None).with_corr_registry(Arc::clone(&state.corr));
    let (snapshot, snapshot_seq) = current_engine_snapshot(&state);

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
        loop {
            match session.next_delta().await {
                Ok(Some(frame)) => {
                    if let Ok(text) = frame.to_json() {
                        let id = frame.envelope.seq.get().to_string();
                        yield Ok(SseEvent::default().event("delta").id(id).data(text));
                    }
                }
                Ok(None) => {}
                Err(_closed) => break,
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
    use multiview_events::{Event, InputStreams, SystemMetrics, Topic};

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
            Event::DeviceDiscovered(DeviceDiscovered {
                driver: "zowietek".to_owned(),
                address: "http://[fd00:db8::42]".to_owned(),
                family: AddressFamily::Ipv6,
                name: None,
            }),
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
        let discovered = Event::DeviceDiscovered(DeviceDiscovered {
            driver: "zowietek".to_owned(),
            address: "http://[fd00:db8::42]".to_owned(),
            family: AddressFamily::Ipv6,
            name: None,
        });
        assert_eq!(event_scope_id(&discovered), None);
    }
}
