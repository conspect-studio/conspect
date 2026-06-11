//! Receiver-side snapshot-then-delta ordering bookkeeping.
//!
//! ADR-RT003: on subscribe the server sends one snapshot per topic, then only
//! deltas; within a topic `seq` is monotonic and deltas are causally ordered
//! **after** their snapshot, so `snapshot ⊕ ordered deltas = current truth`.
//! This [`TopicCursor`] is the per-topic state a receiver keeps to enforce that
//! contract: a snapshot (re)establishes the baseline; each delta must strictly
//! advance the `seq`; an out-of-order delta is rejected.
use crate::envelope::FrameKind;
use crate::error::{Error, Result};
use crate::seq::Seq;
use crate::topic::Topic;

/// Tracks the last accepted `seq` for one topic and the gaps observed.
///
/// Construct with [`TopicCursor::new`]; feed frames in arrival order with
/// [`TopicCursor::accept`]. The cursor starts with no baseline — the first
/// accepted frame **must** be a [`FrameKind::Snapshot`].
#[derive(Debug, Clone)]
pub struct TopicCursor {
    topic: Topic,
    last: Option<Seq>,
}

/// The outcome of accepting a frame against a [`TopicCursor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Accepted {
    /// A snapshot established (or re-established) the baseline at this `seq`.
    SnapshotBaseline {
        /// The new baseline sequence.
        seq: Seq,
    },
    /// A delta advanced the cursor with no gap (it was the immediate successor).
    Delta {
        /// The delta's sequence.
        seq: Seq,
    },
    /// A delta advanced the cursor but `gap` intervening frames were missed —
    /// a re-snapshot of the topic is warranted.
    DeltaWithGap {
        /// The delta's sequence.
        seq: Seq,
        /// How many frames were skipped between the last accepted `seq` and
        /// this one.
        gap: u64,
    },
}

impl TopicCursor {
    /// A fresh cursor for `topic` with no baseline yet.
    #[must_use]
    pub const fn new(topic: Topic) -> Self {
        Self { topic, last: None }
    }

    /// The topic this cursor tracks.
    #[must_use]
    pub const fn topic(&self) -> Topic {
        self.topic
    }

    /// The last `seq` accepted on this topic, if any.
    #[must_use]
    pub const fn last_seq(&self) -> Option<Seq> {
        self.last
    }

    /// Accept a frame of `kind` at `seq` arriving in wire order.
    ///
    /// A [`FrameKind::Snapshot`] always succeeds and resets the baseline to
    /// `seq` (this is how `$resync` rebuilds state). A [`FrameKind::Delta`]
    /// must arrive after a baseline and strictly advance the `seq`.
    ///
    /// # Errors
    ///
    /// - [`Error::NonMonotonic`] if a delta arrives before any snapshot, or
    ///   with a `seq` that does not strictly exceed the last accepted `seq`.
    pub fn accept(&mut self, kind: FrameKind, seq: Seq) -> Result<Accepted> {
        match kind {
            FrameKind::Snapshot => {
                self.last = Some(seq);
                Ok(Accepted::SnapshotBaseline { seq })
            }
            FrameKind::Delta => {
                let last = self.last.ok_or_else(|| Error::NonMonotonic {
                    got: seq.get(),
                    last: 0,
                    topic: self.topic.as_str().to_owned(),
                })?;
                if seq <= last {
                    return Err(Error::NonMonotonic {
                        got: seq.get(),
                        last: last.get(),
                        topic: self.topic.as_str().to_owned(),
                    });
                }
                self.last = Some(seq);
                // `gap_to` is `Some` only when `seq` strictly follows `last`,
                // which we just established; a positive value means drops.
                match last.gap_to(seq) {
                    Some(0) | None => Ok(Accepted::Delta { seq }),
                    Some(gap) => Ok(Accepted::DeltaWithGap { seq, gap }),
                }
            }
        }
    }
}
