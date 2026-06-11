//! Subscription control payloads and the snapshot/resume bookkeeping types.
//!
//! These are the `data` bodies of the `$control`-topic frames a client uses to
//! shape its own view (`$subscribe`/`$unsubscribe`/`$set_rate`) and to resume
//! after a disconnect (`$resume`), plus the server's matching acks and the
//! self-healing `$resync`/`$lag` notifications (ADR-RT003).
use serde::{Deserialize, Serialize};

use crate::seq::Seq;
use crate::topic::Topic;

/// `$subscribe` body — the client asks to receive one or more topics.
///
/// `ids` restricts delivery to a resource subset; `rate_hz` requests a max
/// cadence (only meaningful for high-rate topics, server-clamped); `since_seq`
/// performs subscribe + resume in one shot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscribe {
    /// Topics to subscribe to.
    pub topics: Vec<Topic>,
    /// Optional resource-id allowlist restricting which resources stream.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ids: Vec<String>,
    /// Optional requested max cadence (Hz); server clamps and reports effective.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_hz: Option<u32>,
    /// Optional resume cursor: subscribe and replay from after this `seq`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_seq: Option<Seq>,
}

/// `$subscribed` body — one ack per topic, sent before that topic's snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscribed {
    /// The topic that was subscribed.
    pub topic: Topic,
    /// The cadence the server actually applied after clamping.
    pub effective_rate_hz: u32,
    /// The `seq` the forthcoming snapshot is current as of.
    pub snapshot_seq: Seq,
}

/// `$unsubscribe` body — stop delivering the given topics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unsubscribe {
    /// Topics to stop receiving.
    pub topics: Vec<Topic>,
}

/// `$set_rate` body — change the wire cadence of a (high-rate) topic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetRate {
    /// The topic whose cadence is changing.
    pub topic: Topic,
    /// The requested cadence (Hz); server clamps to `[min, max]`.
    pub rate_hz: u32,
}

/// `$resume` body — presented on reconnect to replay the gap if possible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resume {
    /// The session to resume.
    pub session_id: String,
    /// The last `seq` the client successfully observed.
    pub last_seq: Seq,
}

/// Why a [`Resync`] was issued (the gap could not be replayed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ResyncReason {
    /// The requested `last_seq` had already been evicted from the replay ring.
    SeqEvicted,
    /// The `session_id` is unknown (e.g. the server restarted).
    UnknownSession,
    /// The session expired (TTL elapsed since the client went away).
    SessionExpired,
}

/// `$resync` body — the gap is unrecoverable; the client MUST **rebuild** state
/// (not merge) from the fresh snapshot that follows. The listed topics must be
/// re-snapshotted on a new `seq` baseline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resync {
    /// Why the resume failed.
    pub reason: ResyncReason,
    /// The topics the client must rebuild.
    pub resubscribe: Vec<Topic>,
}

/// What the server did with the dropped frames a [`Lag`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LagAction {
    /// The drops were latest-wins coalesced (high-rate lane).
    Conflated,
    /// A targeted re-snapshot of the topic should follow.
    Resnapshot,
}

/// `$lag` body — this connection's bounded queue overflowed; the affected topic
/// dropped `dropped_n` frames and should be re-snapshotted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lag {
    /// The topic whose frames were dropped.
    pub topic: Topic,
    /// How many frames were dropped.
    pub dropped_n: u64,
    /// What the server did about it.
    pub action: LagAction,
}

/// `$hello` body — the first server frame after auth. Advertises the
/// negotiable parameters of the connection (ADR-RT002 §2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// The server-assigned session id (used by a later `$resume`).
    pub session_id: String,
    /// Envelope schema majors this server can speak.
    pub server_v: Vec<crate::envelope::SchemaVersion>,
    /// Heartbeat interval the server will use (milliseconds).
    pub heartbeat_ms: u32,
    /// Minimum clamped wire cadence (Hz).
    pub min_rate_hz: u32,
    /// Maximum clamped wire cadence (Hz).
    pub max_rate_hz: u32,
    /// Default wire cadence applied when a subscribe omits `rate_hz`.
    pub default_rate_hz: u32,
    /// Size of the per-session/per-topic replay ring (frames).
    pub replay_ring: u32,
}

/// `$error` body — a control-plane error reported to the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolError {
    /// A short, stable machine-readable error code.
    pub code: String,
    /// A human-readable description.
    pub message: String,
}
