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
#[derive(Debug, Clone)]
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
