//! The single versioned wire envelope used in both directions.
//!
//! Per ADR-RT002 every message — events, control frames, WHEP signaling — uses
//! ONE envelope so a single parse/validate/route path serves WS and SSE. The
//! shape is `{v, t, topic, id, seq, ts, corr, data}`; the `t`/`data` pair is
//! carried by the flattened, internally-tagged [`crate::event::Event`] payload.
use multiview_core::time::MediaTime;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::seq::Seq;
use crate::topic::Topic;

/// The envelope schema **major** version (the `v` field).
///
/// A receiver rejects an unknown major (ADR-RT002). Additive event types and
/// fields are minor changes that do not bump this; breaking changes do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SchemaVersion(pub u16);

impl SchemaVersion {
    /// The current wire major (`multiview.v1`).
    pub const V1: Self = Self(1);
}

impl Default for SchemaVersion {
    fn default() -> Self {
        Self::V1
    }
}

/// Whether a frame is the full-state baseline for its topic or an incremental
/// update layered on top of it.
///
/// `snapshot ⊕ ordered deltas = current truth` (ADR-RT003). A receiver MUST
/// treat a fresh [`FrameKind::Snapshot`] as a state **rebuild** (it establishes
/// a new `seq` baseline), and each [`FrameKind::Delta`] as an update that must
/// arrive in strictly increasing `seq` order after that baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameKind {
    /// Full current state of a topic, current as of its `seq`/`ts`. Establishes
    /// (or re-establishes, on `$resync`) the per-topic baseline.
    Snapshot,
    /// An incremental update layered onto the most recent snapshot.
    Delta,
}

/// The single versioned wire frame.
///
/// `T` is the payload type — typically [`crate::event::Event`], which flattens
/// to the `t` discriminator plus its `data`. Metadata fields (`v`, `topic`,
/// `id`, `seq`, `ts`, `corr`) are uniform across every frame so one
/// parse/validate/route path serves WS and SSE.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope<T> {
    /// Envelope schema major (`v`). A receiver rejects an unknown major.
    pub v: SchemaVersion,
    /// Subscription routing key. Control frames use [`Topic::Control`].
    pub topic: Topic,
    /// Optional resource scope (tile/input/output/job id) for fine filtering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Per-connection monotonic resume cursor (gaps indicate drops).
    pub seq: Seq,
    /// Engine monotonic timestamp (same clock family as output PTS).
    pub ts: MediaTime,
    /// Optional correlation id echoing a REST command / job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corr: Option<String>,
    /// The typed payload. For events this flattens to `t` + `data`.
    #[serde(flatten)]
    pub payload: T,
}

impl<T> Envelope<T> {
    /// Construct an envelope at the current schema major ([`SchemaVersion::V1`])
    /// with no `id` or `corr`.
    #[must_use]
    pub fn new(topic: Topic, seq: Seq, ts: MediaTime, payload: T) -> Self {
        Self {
            v: SchemaVersion::V1,
            topic,
            id: None,
            seq,
            ts,
            corr: None,
            payload,
        }
    }

    /// Builder: attach a resource-scope `id`.
    #[must_use]
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Builder: attach a correlation id.
    #[must_use]
    pub fn with_corr(mut self, corr: impl Into<String>) -> Self {
        self.corr = Some(corr.into());
        self
    }

    /// Validate this frame's schema major against a `supported` allowlist.
    ///
    /// # Errors
    ///
    /// [`Error::UnsupportedSchemaVersion`] when `self.v` is not in `supported`.
    pub fn ensure_supported(&self, supported: &[SchemaVersion]) -> Result<()> {
        if supported.contains(&self.v) {
            Ok(())
        } else {
            Err(Error::UnsupportedSchemaVersion {
                got: self.v,
                supported: supported.to_vec(),
            })
        }
    }
}
