//! The per-channel player handle carried on an `IngestPlan`: the thread-local
//! [`MediaPlayer`] transport core plus the shared [`TransportMailbox`] seam.
//!
//! The ingest thread owns the [`MediaPlayer`] outright (no lock on the hot
//! path); the control plane reaches it only through the `Arc<TransportMailbox>`
//! (drained between frames). See [`super`] for the design.

use std::sync::Arc;

use super::{EofPolicy, MediaPlayer, PlayerControlBus, PlayoutGeometry, TransportMailbox};

/// What an `IngestPlan` carries when the source is a media-player channel:
/// the initial transport geometry/policy used to build the channel's
/// [`MediaPlayer`], the shared command mailbox, and the wait-free control bus the
/// video rail publishes its authoritative transport state onto for the audio rail
/// to follow (ADR-T019 §1).
#[derive(Clone)]
pub struct PlayerHandle {
    /// The player channel id (matches the config `media_players[].id`).
    pub id: String,
    /// The integer-frame playout geometry (in/out + vamp window).
    pub geometry: PlayoutGeometry,
    /// The end-of-asset behaviour.
    pub eof_policy: EofPolicy,
    /// Whether the channel begins looping (vamping) on its first play, vs a
    /// single play-through. Derived from the config `loop_default`.
    pub loop_on_start: bool,
    /// The shared, bounded two-class command seam (state verbs conflated
    /// latest-wins; targeted load/cue/seek a bounded drop-oldest FIFO) the ingest
    /// thread drains between frames. **The video rail is the sole consumer**
    /// (ADR-T019 §1): it drains this and publishes the result on [`Self::control_bus`].
    pub mailbox: Arc<TransportMailbox>,
    /// The wait-free single-authority control bus: the video `stream_player`
    /// publishes its applied transport state here (after draining `mailbox`), and
    /// the audio `player_audio_loop` SAMPLES it and follows — so both rails wrap/
    /// exit on the same instant by construction (ADR-T019 §1).
    pub control_bus: Arc<PlayerControlBus>,
    #[cfg(feature = "ffmpeg")]
    pub runtime: Option<PlayerRuntime>,
    pub na_image: Option<Arc<multiview_compositor::pipeline::Nv12Image>>,
}

impl PlayerHandle {
    /// Construct a handle (with a fresh [`PlayerControlBus`] the boot path shares
    /// with the channel's audio rail via [`Self::control_bus`]).
    #[must_use]
    pub fn new(
        id: String,
        geometry: PlayoutGeometry,
        eof_policy: EofPolicy,
        loop_on_start: bool,
        mailbox: Arc<TransportMailbox>,
    ) -> Self {
        Self {
            id,
            geometry,
            eof_policy,
            loop_on_start,
            mailbox,
            control_bus: Arc::new(PlayerControlBus::new()),
            #[cfg(feature = "ffmpeg")]
            runtime: None,
            na_image: None,
        }
    }

    /// Build the channel's initial [`MediaPlayer`], parked at the in-point
    /// (`Cued`) with the handle's geometry/policy, anchored at the zero output
    /// media time (re-anchored on the first `play`/`vamp`).
    #[must_use]
    pub fn build_player(&self) -> MediaPlayer {
        MediaPlayer::new(
            self.geometry,
            self.eof_policy,
            multiview_core::time::MediaTime::ZERO,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct PlayerStatus {
    /// Committed load generation (`0` before the first clip commits).
    pub clip_epoch: u64,
    /// The last failed transport operation; `None` while the player is healthy.
    pub fault: Option<PlayerFault>,
}

impl PlayerStatus {
    /// The status after the clip generation `commit_epoch` commits.
    ///
    /// A commit clears the fault **only** when it is for the same or a newer
    /// generation than the status tracks; a stale commit (an older generation)
    /// is ignored outright, so a late or duplicated commit can never erase a
    /// newer failure. Pure and total, so the ordering rule is unit-testable
    /// without a running runtime.
    #[must_use]
    pub const fn after_commit(self, commit_epoch: u64) -> Self {
        if commit_epoch < self.clip_epoch {
            self
        } else {
            Self {
                clip_epoch: commit_epoch,
                fault: None,
            }
        }
    }

    /// The status after a `cue`/`seek` to `frame` failed for clip generation
    /// `epoch`. The fault keeps the generation it applies to, so the operator
    /// (and the next commit) can tell which clip the failure belongs to.
    #[must_use]
    pub const fn after_seek_failure(self, epoch: u64, frame: u64) -> Self {
        Self {
            clip_epoch: if epoch > self.clip_epoch {
                epoch
            } else {
                self.clip_epoch
            },
            fault: Some(PlayerFault::SeekFailed {
                generation: epoch,
                frame,
            }),
        }
    }
}

/// A redacted, generation-correlated player transport failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum PlayerFault {
    /// Durable loads require a resolvable identity; the live slot stays unchanged.
    LoadReferenceMissing {
        /// The staged load generation that could not commit.
        generation: u64,
    },
    /// Durable precommit failed; no filesystem detail is exposed.
    LoadJournalFailed {
        /// The staged load generation that could not commit.
        generation: u64,
    },
    /// Seek or cue failed; the player holds its last good picture and pauses its
    /// audio until a newer load commits.
    SeekFailed {
        /// The clip generation the failed verb applied to.
        generation: u64,
        /// The requested source frame.
        frame: u64,
    },
}

impl PlayerFault {
    /// The clip generation this fault was recorded against.
    #[must_use]
    pub const fn generation(self) -> u64 {
        match self {
            Self::SeekFailed { generation, .. }
            | Self::LoadReferenceMissing { generation }
            | Self::LoadJournalFailed { generation } => generation,
        }
    }
}

#[cfg(feature = "ffmpeg")]
pub fn publish_status(
    slot: &multiview_framestore::SwapCell<Arc<PlayerStatus>>,
    update: impl FnOnce(PlayerStatus) -> PlayerStatus,
) {
    slot.store(Arc::new(update(*slot.load())));
}

#[cfg(feature = "ffmpeg")]
#[derive(Clone)]
pub struct PlayerRuntime {
    /// Off-ingest opener that owns every libav teardown.
    pub opener: Arc<super::PlayerOpener>,
    /// Trusted startup library IDs only; public asset strings never become paths.
    pub assets: Arc<std::collections::HashMap<String, super::ClipSpec>>,
    /// Bounded staged/retired/shutdown handoffs.
    pub coordinator: Arc<super::LoadCoordinator<super::StagedClip>>,
    /// Capacity-one joint audio adoption slot.
    pub audio_commit: Arc<super::AudioCommitSlot<super::StagedAudio>>,
    /// Tile-sized NA card.
    pub na_image: Arc<multiview_compositor::pipeline::Nv12Image>,
    /// Committed reference identity and generation; no path disclosure.
    pub binding: Arc<multiview_framestore::SwapCell<Arc<super::SlotSnapshot>>>,
    /// Separate non-dropping compare-and-detach control admission.
    pub detach: Arc<super::DetachPort>,
    /// Configured off-clock durable current-binding journal.
    pub journal: Option<super::BindingJournal>,
    /// Latest redacted runtime status; wait-free bounded snapshot.
    pub status: Arc<multiview_framestore::SwapCell<Arc<PlayerStatus>>>,
    /// First-image adoption facts; provider still requires matching audio ACK.
    pub native: Arc<
        multiview_framestore::SwapCell<Option<Arc<multiview_control::player_load::NativeLoadAck>>>,
    >,
    /// Test-only nonblocking publication observer (bounded receiver in tests).
    #[cfg(test)]
    pub observer: Option<
        std::sync::mpsc::SyncSender<(
            multiview_core::time::MediaTime,
            multiview_core::time::MediaTime,
            Arc<multiview_compositor::pipeline::Nv12Image>,
        )>,
    >,
    /// One-shot panic injection after all native owners enter fixed slots.
    #[cfg(test)]
    pub panic_after_commit: Arc<std::sync::atomic::AtomicBool>,
    /// Bounded test-only loop trace, inspected only after a failing condition.
    #[cfg(test)]
    pub trace:
        Option<std::sync::mpsc::SyncSender<(multiview_core::time::MediaTime, i64, bool, bool)>>,
}
