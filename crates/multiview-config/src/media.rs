//! Media players & the media library — the config serde mirror of the
//! media-player subsystem (ADR-0057 Decisions 1/2, ADR-0097 vamp window,
//! [media-playout §3/§15](../../docs/research/media-playout.md)).
//!
//! An operator declares a **media library** (a root directory plus a set of
//! [`MediaAsset`]s — stills, clips, audio) and up to two **media players**
//! ([`MediaPlayer`]) — pre-declared, bus-selectable source channels each owning
//! one supervised ingest from boot. Loading/cueing swaps *what* a player plays;
//! it never spawns or tears down a thread. A player is bound onto the canvas by
//! the additive [`SourceKind::MediaPlayer`](crate::SourceKind::MediaPlayer)
//! binding, and a still is bound directly by
//! [`SourceKind::Still`](crate::SourceKind::Still).
//!
//! This crate carries only the **declared** intent (the operator's authored
//! fields). The probed machine-truth (container/codec/dimensions/cadence/
//! duration/alpha) is read-only runtime state with no representation here — it is
//! never authored and never exported (media-playout §3).
//!
//! Time is declared in **integer frames** at the canvas cadence, never raw URLs
//! or float seconds. Authors who think in milliseconds convert once at the API
//! boundary via [`frames_for_ms`] (ADR-T015 §2) — converting at one boundary
//! avoids the double-rounding the brief warns against.

use serde::{Deserialize, Serialize};

use multiview_core::time::Rational;

/// The end-of-asset behaviour for a media player or asset
/// (ADR-0057 Decision 4, [media-playout §7.4](../../docs/research/media-playout.md)).
///
/// This is the **config serde mirror** of the runtime `EofPolicy` in
/// `multiview-cli` (`src/player/transport.rs`); the CLI maps this declarative
/// value onto the runtime policy. The two are kept structurally identical — the
/// same four variants, `hold_last_frame` as the default — so the mapping is a
/// total one-to-one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EofPolicy {
    /// Freeze on the final real frame and hold it forever (the default; the
    /// as-built `NoSignalPolicy::HoldForever` behaviour).
    #[default]
    HoldLastFrame,
    /// Seek back to the in-point in place and keep decoding — loops forever.
    Loop,
    /// Publish one terminal frame (opaque black for opaque assets, fully
    /// transparent for alpha assets) and hold it.
    Black,
    /// Publish the terminal frame, report ended, and release the decoder; the
    /// switcher state machine applies the bus/keyer consequence.
    AutoOff,
}

/// The media kind of a declared [`MediaAsset`].
///
/// Determines how the asset is decoded and bound: a `still` is a single decoded
/// frame held indefinitely, a `clip` is a video file played over its
/// in/out/vamp window, and an `audio` asset carries only audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MediaAssetKind {
    /// A single still image (held indefinitely once decoded).
    Still,
    /// A video clip played over its `[in_point, out_point)` window.
    Clip,
    /// An audio-only asset.
    Audio,
}

/// A single declared media asset in the [`MediaLibrary`].
///
/// Carries the operator's **declared** intent only: the id, an optional label,
/// the media kind, the path (relative to [`MediaLibrary::root`] or absolute),
/// the integer-frame in/out and vamp window, an optional trigger point, and the
/// default EOF / loop behaviour a player inherits when it loads this asset.
///
/// The vamp window (`vamp_in_frames`/`vamp_out_frames`, ADR-0097) is the
/// sub-range that loops while vamping; when both are present they must nest as
/// `in_point ≤ vamp_in < vamp_out ≤ out_point` (enforced by
/// [`MultiviewConfig::validate`](crate::MultiviewConfig::validate)). Probed
/// machine-truth (container/codec/width/height/fps/duration/alpha) is **not**
/// modelled here — it is read-only runtime state, never authored (media-playout
/// §3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct MediaAsset {
    /// Stable asset id (referenced by a player's `default` and by
    /// [`SourceKind::Still`](crate::SourceKind::Still)).
    pub id: String,
    /// Human-friendly display label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The media kind (`still` | `clip` | `audio`).
    pub kind: MediaAssetKind,
    /// Filesystem path (relative to [`MediaLibrary::root`], or absolute).
    pub path: String,
    /// Clip in-point in frames (inclusive). Absent ⇒ the start of the asset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_point_frames: Option<u64>,
    /// Clip out-point in frames (exclusive). Absent ⇒ the end of the asset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_point_frames: Option<u64>,
    /// Vamp-segment start in frames (inclusive, ADR-0097). Absent ⇒ the whole
    /// trimmed clip is the vamp loop. Must be paired with `vamp_out_frames`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vamp_in_frames: Option<u64>,
    /// Vamp-segment end in frames (exclusive, ADR-0097). Must be paired with
    /// `vamp_in_frames` and satisfy `vamp_in < vamp_out`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vamp_out_frames: Option<u64>,
    /// Optional trigger point in frames (a marker a cue can fire at). Absent ⇒
    /// no trigger marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_point_frames: Option<u64>,
    /// The default EOF behaviour a player inherits when it loads this asset.
    #[serde(default, skip_serializing_if = "is_default_eof_policy")]
    pub default_eof_policy: EofPolicy,
    /// Whether the asset defaults to looping (the vamp window) when loaded.
    #[serde(default, skip_serializing_if = "is_false")]
    pub default_loop: bool,
}

/// The media library: a root directory plus the declared assets.
///
/// Absent on a document ⇒ no media library is configured.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct MediaLibrary {
    /// Root directory relative asset paths resolve against. Absent ⇒ asset
    /// paths are taken as-is (absolute, or relative to the process CWD).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// The declared media assets.
    #[serde(default)]
    pub assets: Vec<MediaAsset>,
}

/// A media-player channel (ADR-0057 Decision 2): a pre-declared, bus-selectable
/// source that owns one supervised ingest from boot.
///
/// Bound onto the canvas by
/// [`SourceKind::MediaPlayer`](crate::SourceKind::MediaPlayer). Loading/cueing
/// swaps the asset the channel plays without spawning or tearing down a thread.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct MediaPlayer {
    /// Stable player id (referenced by
    /// [`SourceKind::MediaPlayer`](crate::SourceKind::MediaPlayer)).
    pub id: String,
    /// The asset the channel loads at boot (a [`MediaAsset::id`]). Absent ⇒ the
    /// channel boots idle (`NO_SIGNAL` placeholder) until a load command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// The EOF behaviour applied at end-of-asset (overrides the loaded asset's
    /// `default_eof_policy`).
    #[serde(default, skip_serializing_if = "is_default_eof_policy")]
    pub eof_policy: EofPolicy,
    /// Whether the channel loops the vamp window by default.
    #[serde(default, skip_serializing_if = "is_false")]
    pub loop_default: bool,
    /// Audio routing for the player's program audio. Absent ⇒ the engine's
    /// default (the player's audio joins the program bus like a managed source);
    /// the runtime mix/route is owned by `multiview-audio` + the engine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<MediaPlayerAudio>,
}

/// How a [`MediaPlayer`]'s program audio is routed.
///
/// Mirrors the managed-source posture (`multiview-audio` owns the mix/route):
/// config carries only the operator's routing intent.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct MediaPlayerAudio {
    /// Whether the player contributes audio to the program bus (`true`, the
    /// default) or is muted at source (`false`).
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub to_program: bool,
    /// Per-channel gain in dB applied before the program mix. Absent ⇒ unity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gain_db: Option<f64>,
}

/// Convert a duration in milliseconds to a frame count at `cadence`
/// (ADR-T015 §2).
///
/// Uses the normative exact-integer formula, round-half-up with a 1-frame floor
/// so a requested duration never silently becomes a 0-frame no-op:
///
/// ```text
/// frames(ms) = max(1, floor((2·ms·num + 1000·den) / (2000·den)))
/// ```
///
/// All arithmetic is in `i128`, so it never overflows for realistic `ms` and
/// `num`/`den`. A degenerate (zero-denominator or non-positive) cadence cannot
/// define a frame period; the result is clamped to the 1-frame minimum rather
/// than panicking.
///
/// # Examples
///
/// ```
/// use multiview_config::frames_for_ms;
/// use multiview_core::time::Rational;
///
/// assert_eq!(frames_for_ms(1000, Rational::new(60_000, 1001)), 60);
/// assert_eq!(frames_for_ms(8, Rational::new(25, 1)), 1); // min-1 clamp
/// ```
#[must_use]
pub fn frames_for_ms(ms: u64, cadence: Rational) -> u64 {
    // A usable cadence is a strictly-positive rational. Anything else cannot
    // define a frame period, so fall back to the documented 1-frame minimum.
    if cadence.den <= 0 || cadence.num <= 0 {
        return 1;
    }
    let ms = i128::from(ms);
    let num = i128::from(cadence.num);
    let den = i128::from(cadence.den);
    // floor((2·ms·num + 1000·den) / (2000·den)); all operands positive here, so
    // truncating division is a floor.
    let numerator = 2 * ms * num + 1000 * den;
    let denominator = 2000 * den;
    let frames = numerator / denominator;
    // max(1, …): a requested duration is never a 0-frame no-op.
    u64::try_from(frames).unwrap_or(u64::MAX).max(1)
}

/// `skip_serializing_if` predicate: an [`EofPolicy`] equal to the default.
// serde's `skip_serializing_if` contract calls the predicate with the field by
// reference; the derive fixes the signature, so the by-value shape the lint
// asks for cannot be used here.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_default_eof_policy(policy: &EofPolicy) -> bool {
    *policy == EofPolicy::HoldLastFrame
}

/// `skip_serializing_if` predicate for a default-`false` bool.
// serde's `skip_serializing_if` contract fixes this by-reference signature.
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(value: &bool) -> bool {
    !*value
}

/// `skip_serializing_if` predicate for a default-`true` bool.
// serde's `skip_serializing_if` contract fixes this by-reference signature.
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_true(value: &bool) -> bool {
    *value
}

/// Default for a `to_program` audio flag: contribute to the program bus.
const fn default_true() -> bool {
    true
}
