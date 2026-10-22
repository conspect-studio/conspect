//! The **single-authority transport coupling** between a media player's video
//! rail and its audio rail (ADR-T019 §1, the Defect-1 fix).
//!
//! # Why
//!
//! A media player has two data-plane rails: the video `stream_player`
//! ([`crate::pipeline`]) and the audio `player_audio_loop` ([`crate::audio`]),
//! each on its own decode thread. Both must wrap/exit the loop on the **same
//! instant**. They cannot both drain the shared
//! [`TransportMailbox`](super::TransportMailbox) — `drain()` is a destructive
//! `mem::take`, so whichever rail drained a verb first would consume it and the
//! other would miss it (the rails would then desync).
//!
//! # The contract
//!
//! The **video rail is the sole consumer** of the transport mailbox. After it
//! applies a verb it **publishes** its authoritative transport decision to this
//! wait-free [`PlayerControlBus`]; the **audio rail samples** the bus each block
//! and **follows** it — it never independently consumes transport verbs. The
//! audio rail is therefore *sampled* by, and follows, the one authority
//! (invariant #1: audio samples, never paces; the bus is a wait-free
//! [`arc_swap::ArcSwap`], so neither rail ever blocks the other — invariant #10).
//! Same-boundary alignment is true **by construction** (one authority, one
//! geometry).

use std::sync::Arc;

use arc_swap::ArcSwap;

use multiview_core::time::MediaTime;

/// The transport state the audio rail mirrors from the video rail. A strict
/// subset of the video [`MediaPlayerState`](super::MediaPlayerState): the audio
/// deck only ever loops (`Vamping`), holds silence (`Paused`), or re-cues
/// (`Stopped`) — it carries none of the video machine's `Cued`/`Loading`/EOF
/// terminal picture states (its bus contribution is just silence in those).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum AudioTransport {
    /// Loop the vamp segment (the steady playout for a looping/playing channel).
    /// The default a boot-vamping player starts in.
    #[default]
    Vamping,
    /// Hold silence (the video is paused / held / cued / between assets).
    Paused,
    /// Re-cue to the head and hold silence until a fresh `Vamping`.
    Stopped,
}

/// One published snapshot of the video rail's authoritative transport decision.
///
/// `generation` is monotonic so a late-sampling audio block still observes a
/// change exactly once (it compares the generation it last applied). The
/// `exit_arm_anchor` carries the **output media-time at which the video armed its
/// vamp exit**, so the audio rail can arm its own deck at the *same* next-vamp
/// boundary ([`LoopDeck::arm_exit_at`](multiview_audio::LoopDeck::arm_exit_at)) —
/// `None` when no exit is armed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayerControl {
    /// Monotonic publish counter (a fresh load with a higher generation than the
    /// audio rail last applied means "apply this").
    pub generation: u64,
    /// The transport state the audio rail mirrors.
    pub state: AudioTransport,
    /// The output media-time the video armed its vamp exit at, or `None`.
    pub exit_arm_anchor: Option<MediaTime>,
}

impl Default for PlayerControl {
    fn default() -> Self {
        Self {
            generation: 0,
            state: AudioTransport::Vamping,
            exit_arm_anchor: None,
        }
    }
}

/// A wait-free, single-writer / single-reader (by convention) control bus the
/// video rail publishes its transport state into and the audio rail samples.
///
/// Shared by `Arc` between the two decode threads. Publishing is a wait-free
/// `ArcSwap` store at operator rate (only when a verb changes the transport
/// state — not per frame); sampling is a wait-free `ArcSwap` load per audio
/// block. Neither rail ever blocks the other, and neither can stall the output
/// clock (invariants #1/#10).
#[derive(Debug)]
pub struct PlayerControlBus {
    slot: ArcSwap<PlayerControl>,
    /// The next generation to stamp (only the video rail writes it, between
    /// frames, so a plain `Cell`-free monotonic via the stored value suffices —
    /// we read-modify-write the slot under the single-writer convention).
    next_gen: std::sync::atomic::AtomicU64,
}

impl Default for PlayerControlBus {
    fn default() -> Self {
        Self::new()
    }
}

impl PlayerControlBus {
    /// A fresh bus, defaulting to `Vamping` with no exit armed — so a
    /// boot-vamping player loops from the first audio block even before the video
    /// rail's first publish.
    #[must_use]
    pub fn new() -> Self {
        Self {
            slot: ArcSwap::from_pointee(PlayerControl::default()),
            next_gen: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// Publish the video rail's authoritative transport decision (called by the
    /// video `stream_player` after it applies a verb). Stamps a fresh monotonic
    /// generation. Wait-free `ArcSwap` store — never blocks the audio rail.
    pub fn publish(&self, state: AudioTransport, exit_arm_anchor: Option<MediaTime>) {
        let generation = self
            .next_gen
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.slot.store(Arc::new(PlayerControl {
            generation,
            state,
            exit_arm_anchor,
        }));
    }

    /// Sample the latest published control (called by the audio rail each block).
    /// Wait-free `ArcSwap` load.
    #[must_use]
    pub fn load(&self) -> PlayerControl {
        *self.slot.load_full()
    }
}
