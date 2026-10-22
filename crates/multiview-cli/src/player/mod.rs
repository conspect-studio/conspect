//! Media-player transport: the pure, deterministic playout state machine that
//! drives a pre-declared media-player **channel** ([ADR-0057](../../../../docs/decisions/ADR-0057.md),
//! [media-playout §7](../../../../docs/research/media-playout.md)) plus the
//! **vamp-and-exit** extension ([ADR-0097](../../../../docs/decisions/ADR-0097.md)).
//!
//! # Why a pure core
//!
//! The transport logic — what to publish, when to wrap a loop, when a vamp's
//! armed exit fires, how to stamp each frame output-anchored — is a pure
//! function of integer frame indices and the output tick counter. It carries
//! **no** libav/GPU dependency, so it lives here (feature-independent, in the
//! CI-green default build) and is unit- and property-tested with zero hardware.
//! The `ffmpeg`-gated ingest executor (`open_and_stream` in
//! [`crate::pipeline`]) is the thin shell that *performs* the
//! [`PlayerAction`]s this core returns: it owns the demuxer, the decoder, the
//! container seek, the `avcodec_flush_buffers` after every seek, and the
//! `TileStore::publish`.
//!
//! # Invariants
//!
//! The core never blocks and never reads a wall clock: a media player is
//! *sampled* per output tick like every source (invariant #1) — it returns the
//! frame to publish, it does not pace. All durations are **integer frame
//! counts** at the program's exact rational cadence ([ADR-T015](../../../../docs/decisions/ADR-T015.md),
//! invariant #3); frames stamp **output-anchored**
//! (`publish_at(k) = anchor_pts + k × frame_period`, media-playout §7.2), never
//! source-relative.
//!
//! Because the core **synthesizes its own monotone output-anchored timeline**,
//! the executor stamps published frames from [`PlayerAction::Publish`]'s `at`
//! directly and **does not route the decoded frame's raw PTS through the
//! `PtsNormalizer`** at all (not on a wrap, not on first play, not on a
//! cue/seek). The wrapped-source-PTS monotonic-clamp hazard therefore cannot
//! arise on the player path — the timeline is monotone by construction
//! ([ADR-0097](../../../../docs/decisions/ADR-0097.md) §5, "Implementation
//! refinement"). The `avcodec_flush_buffers` after every seek is retained and
//! non-negotiable (a decoder-state hazard independent of the stamping
//! timeline).

mod control;
mod handle;
mod mailbox;
mod transport;

pub use control::{AudioTransport, PlayerControl, PlayerControlBus};
pub use handle::PlayerHandle;
pub use mailbox::{TransportMailbox, TransportVerb};
pub use transport::{
    EofPolicy, MediaPlayer, MediaPlayerState, PlayerAction, PlayoutGeometry, PlayoutGeometryError,
};
