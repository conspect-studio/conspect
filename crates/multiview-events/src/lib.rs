//! # multiview-events
//!
//! Shared realtime event types and the **single versioned message envelope**
//! used in both directions by the engine, the control plane, and clients.
//!
//! This crate is pure-Rust (no FFI, no native deps) and exists so there is
//! exactly one parse/validate/route path for the WebSocket and SSE transports
//! (ADR-RT002). It defines:
//!
//! - [`Envelope`] — the versioned wire frame `{v, t, topic, id, seq, ts, corr,
//!   data}`, generic over its payload.
//! - [`Event`] — the internally-tagged (`t`/`data`) discriminated union of every
//!   control frame and data event. **Never `#[serde(untagged)]`** (conventions
//!   §5): the explicit `t` tag gives one unambiguous parse path.
//! - [`Topic`] — the coarse, group-level subscription routing keys (plus the
//!   `$control` pseudo-topic).
//! - [`Seq`] / [`SeqCounter`] — the per-connection monotonic resume cursor and
//!   the helper that guarantees strictly increasing issuance.
//! - [`FrameKind`] + [`TopicCursor`] — the snapshot-then-delta ordering model
//!   (`snapshot ⊕ ordered deltas = current truth`, ADR-RT003), with the
//!   receiver-side per-topic monotonicity check and gap detection that drives
//!   re-snapshot.
//!
//! All of this is best-effort, fan-out metadata: it carries the realtime
//! state out of the engine but, by invariant #10, can **never** back-pressure
//! it. These types are deliberately small, `Clone`, and `serde`-round-trippable.
//!
//! The library target is `multiview_events`.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod asyncapi;
pub mod envelope;
pub mod error;
pub mod event;
pub mod ordering;
pub mod seq;
pub mod subscription;
pub mod topic;

pub use envelope::{Envelope, FrameKind, SchemaVersion};
pub use error::{Error, Result};
pub use event::{
    AchievedSync, AddressFamily, AlarmTransition, Alert, AlertSeverity, AudioMeter, ClockQuality,
    ClockSource, DeviceAdopted, DeviceCapabilities, DeviceDiscovered, DeviceError, DeviceMode,
    DeviceRemoved, DeviceState, DeviceStatus, DeviceStreamRole, DeviceStreamStatus, DeviceSync,
    DeviceSyncSummary, Event, GpuMetrics, GpuVendor, HealthWarning, ImpactClass, InputConnection,
    InputStreams, JobProgress, LifecycleState, ModePhase, OutputRunState, OutputStatus, SalvoEvent,
    SalvoPhase, ShedLoad, ShedReason, ShedScope, SyncCapability, SyncChange, SyncGroupSkew,
    SystemMetrics, TallyEvent, TallyTarget, TileSnapshotEntry, TileState, TilesSnapshot,
    TimingStatus, WarningCode, WarningSeverity,
};
pub use ordering::{Accepted, TopicCursor};
pub use seq::{Seq, SeqCounter};
pub use subscription::{
    Hello, Lag, LagAction, ProtocolError, Resume, Resync, ResyncReason, SetRate, Subscribe,
    Subscribed, Unsubscribe,
};
pub use topic::Topic;

/// An [`Envelope`] whose payload is an [`Event`] — the canonical realtime frame.
pub type EventEnvelope = Envelope<Event>;
