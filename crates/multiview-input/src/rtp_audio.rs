//! The shared RTP-audio → `AudioStore` frame-index rebase seam (ADR-T013).
//!
//! Every ingest whose audio arrives as **RTP packets with a sample-rate-keyed
//! media-clock timestamp** — WebRTC Opus ([ADR-T014]), AES67 / ST 2110-30 PCM
//! ([ADR-0033]), and any future RTP-audio source — routes through this **one**
//! type to turn an RTP timestamp into an **absolute `AudioStore` frame index**
//! on the unified timeline. Without it each ingest would invent a divergent
//! "RTP timestamp → store frame" path and they would drift apart in subtle,
//! soak-only-visible ways (wrap, anchor, gap placement). This pins that seam
//! once: the depacketizer owns only the per-codec step and the declared clock
//! rate; **wrap, anchor, discontinuity re-anchor, and the rescale to the
//! canonical store rate are the shared contract here.**
//!
//! It is the **audio analogue** of [`crate::normalize::PtsNormalizer`]
//! (ADR-T003): the same delta-based 32-bit unwrap and discontinuity re-anchor,
//! but expressed in **frames** (the store is frame-indexed and pulls frames per
//! tick) rather than a `MediaTime` instant, and keyed on the **stream's RTP
//! clock rate** (Opus 48 kHz, AES67 L24 48/96 kHz) rather than the video 90 kHz
//! — copying the video assumption would mis-scale every audio timestamp.
//!
//! ## Isolation (invariants #1 / #10)
//!
//! Pure and allocation-free per call. It runs on the ingest/decode side, never
//! the output clock; it **never paces** and **never back-pressures** — a stalled
//! RTP-audio source simply stops calling [`RtpAudioRebaser::rebase`] and the
//! store rides silence-fill ([`crate::rtp_audio`] never manufactures a fill
//! block; [`multiview_audio::store::AudioStore::read`] is gap-free).
//!
//! [ADR-T013]: https://github.com/aperim/multiview/blob/main/docs/decisions/ADR-T013.md
//! [ADR-T014]: https://github.com/aperim/multiview/blob/main/docs/decisions/ADR-T014.md
//! [ADR-0033]: https://github.com/aperim/multiview/blob/main/docs/decisions/ADR-0033.md

use multiview_core::time::{rescale, Rational};

/// The 32-bit RTP timestamp modulus (`2^32`) and its half, for the delta-based
/// forward/backward wrap detection (RFC 3550; the same width
/// [`crate::normalize::WrapBits::Rtp32`] unwraps for video).
const RTP_MODULUS: i64 = 1_i64 << 32;
const RTP_HALF_MODULUS: i64 = 1_i64 << 31;

/// The discontinuity threshold, in **store frames**: an unwrapped-tick jump that
/// maps to a span larger than this re-anchors rather than propagating a skip.
/// ~10 s at the canonical 48 kHz store rate — the frame-space equivalent of
/// [ADR-T003]'s ~10 s `PtsNormalizer` guard.
///
/// [ADR-T003]: https://github.com/aperim/multiview/blob/main/docs/decisions/ADR-T013.md
const DEFAULT_DISCONTINUITY_FRAMES: i64 = 480_000;

/// The result of mapping one RTP-audio packet onto the store timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RebasedAudio {
    /// The absolute store frame index the packet's first sample lands at
    /// (the index handed to [`multiview_audio::store::AudioStore::publish_at`]).
    pub store_frame: i64,
    /// Whether this packet caused a re-anchor (a new SSRC, an explicit
    /// discontinuity, or a jump beyond the threshold). The first packet of a
    /// stream is an *anchor*, not a re-anchor, and reports `false`.
    pub reanchored: bool,
}

/// Maps a stream of RTP-audio packets onto absolute `AudioStore` frame indices.
///
/// Construct one per source with [`RtpAudioRebaser::new`] (its declared wire
/// clock rate and the canonical store rate), then call
/// [`rebase`](RtpAudioRebaser::rebase) once per depacketized packet with its
/// 32-bit RTP timestamp, SSRC, and discontinuity flag. The returned
/// [`RebasedAudio::store_frame`] is the absolute index to
/// [`publish_at`](multiview_audio::store::AudioStore::publish_at).
///
/// The **anchor** is the store frame the first packet lands at (default `0`;
/// set it to the store's live edge with [`RtpAudioRebaser::with_anchor_frame`]
/// so the first received audio lands at "now" exactly as the video path anchors
/// the first frame). A re-anchor never moves the frame index backward — it
/// continues forward from the last produced frame so a soak never replays
/// hours of history or stalls.
#[derive(Debug, Clone)]
pub struct RtpAudioRebaser {
    /// The stream's RTP clock rate (Hz) — the wire timebase, e.g. Opus 48 000.
    wire_rate_hz: u32,
    /// The canonical store rate (Hz) the frame index is expressed in (48 000).
    store_rate_hz: u32,
    /// The store frame the next *anchor* lands at (the first packet, or the
    /// continue-forward point after a re-anchor).
    anchor_frame: i64,
    /// Re-anchor threshold in store frames.
    discontinuity_frames: i64,
    /// The accumulated 64-bit unwrap offset (in wire ticks) for the current
    /// SSRC's timeline.
    accumulated_wrap: i64,
    /// The previous masked raw RTP value, for delta-based wrap detection.
    last_raw: Option<i64>,
    /// The unwrapped wire tick the current anchor maps from (so a delta off the
    /// anchor gives the store frame).
    anchor_rtp: i64,
    /// The SSRC of the current stream; a change re-anchors.
    ssrc: Option<u32>,
    /// The last produced store frame, so a re-anchor never goes backward.
    last_store_frame: Option<i64>,
}

impl RtpAudioRebaser {
    /// Build a rebaser for a stream whose RTP clock is `wire_rate_hz`, producing
    /// frame indices at the canonical `store_rate_hz`. When the two rates are
    /// equal (the Opus / AES67 Class-A common case, both 48 kHz) the rescale is
    /// identity. A zero rate is clamped to `1` so the math never divides by
    /// zero (a degenerate stream then maps every tick to the anchor frame).
    #[must_use]
    pub fn new(wire_rate_hz: u32, store_rate_hz: u32) -> Self {
        Self {
            wire_rate_hz: wire_rate_hz.max(1),
            store_rate_hz: store_rate_hz.max(1),
            anchor_frame: 0,
            discontinuity_frames: DEFAULT_DISCONTINUITY_FRAMES,
            accumulated_wrap: 0,
            last_raw: None,
            anchor_rtp: 0,
            ssrc: None,
            last_store_frame: None,
        }
    }

    /// Set the store frame the first received packet anchors at — the store's
    /// **live edge** at the moment ingest starts, so the first audio lands at the
    /// output clock's "now" (the audio analogue of anchoring the first video
    /// frame to `master_now`). Defaults to `0`.
    #[must_use]
    pub const fn with_anchor_frame(mut self, frame: i64) -> Self {
        self.anchor_frame = frame;
        self
    }

    /// Override the re-anchor threshold (in store frames). A mapped jump larger
    /// than this is treated as a timeline break.
    #[must_use]
    pub const fn with_discontinuity_frames(mut self, frames: i64) -> Self {
        self.discontinuity_frames = if frames < 1 { 1 } else { frames };
        self
    }

    /// Map one depacketized RTP-audio packet onto an absolute store frame index.
    ///
    /// `rtp_timestamp` is the packet's 32-bit RTP media timestamp (verbatim),
    /// `ssrc` its synchronization source, and `discontinuity` whether the
    /// depacketizer flagged a decode gap before it (a lost packet / DTX gap). A
    /// new SSRC, a set `discontinuity`, or a mapped jump beyond the threshold
    /// **re-anchors** forward; otherwise the packet's RTP delta off the current
    /// anchor (rescaled wire→store with exact integer math) gives the frame.
    pub fn rebase(&mut self, rtp_timestamp: u32, ssrc: u32, discontinuity: bool) -> RebasedAudio {
        let raw = i64::from(rtp_timestamp);
        let ssrc_changed = self.ssrc != Some(ssrc);

        // A new SSRC starts a fresh unwrap timeline; reset the accumulator/last so
        // the new stream's first delta is not measured against the old clock.
        if ssrc_changed {
            self.accumulated_wrap = 0;
            self.last_raw = None;
        }

        let unwrapped = self.unwrap(raw);
        self.ssrc = Some(ssrc);

        // The first packet of the rebaser's life is a pure anchor (not a
        // re-anchor); thereafter an SSRC change / explicit flag / large mapped
        // jump re-anchors forward.
        let is_first = self.last_store_frame.is_none();
        let mapped = self.map_to_store(unwrapped.saturating_sub(self.anchor_rtp));
        let candidate = self.anchor_frame.saturating_add(mapped);
        let jumped = self
            .last_store_frame
            .is_some_and(|prev| (candidate.saturating_sub(prev)).abs() > self.discontinuity_frames);

        if is_first {
            self.anchor_rtp = unwrapped;
            let frame = self.anchor_frame;
            self.last_store_frame = Some(frame);
            return RebasedAudio {
                store_frame: frame,
                reanchored: false,
            };
        }

        if ssrc_changed || discontinuity || jumped {
            // Re-anchor: continue forward from the last produced frame. The new
            // stream's first packet maps to that point; subsequent deltas ride off
            // the new `anchor_rtp` so they advance correctly.
            let continue_at = self.last_store_frame.unwrap_or(self.anchor_frame);
            self.anchor_frame = continue_at;
            self.anchor_rtp = unwrapped;
            self.last_store_frame = Some(continue_at);
            return RebasedAudio {
                store_frame: continue_at,
                reanchored: true,
            };
        }

        self.last_store_frame = Some(candidate);
        RebasedAudio {
            store_frame: candidate,
            reanchored: false,
        }
    }

    /// Rescale a wire-tick delta to a store-frame delta with exact integer math
    /// (identity when the rates are equal). Never float (invariant #3).
    fn map_to_store(&self, wire_delta: i64) -> i64 {
        if self.wire_rate_hz == self.store_rate_hz {
            return wire_delta;
        }
        // delta_frames = wire_delta * (1/wire_rate) / (1/store_rate)
        //              = rescale(wire_delta, 1/wire_rate, 1/store_rate)
        rescale(
            wire_delta,
            Rational::new(1, i64::from(self.wire_rate_hz)),
            Rational::new(1, i64::from(self.store_rate_hz)),
        )
    }

    /// Delta-based 32-bit RTP unwrap (the same algorithm `PtsNormalizer` uses for
    /// `WrapBits::Rtp32`): a negative delta beyond half the modulus is a forward
    /// wrap, a positive delta beyond half is a backward wrap.
    fn unwrap(&mut self, raw: i64) -> i64 {
        if let Some(last) = self.last_raw {
            let delta = raw.saturating_sub(last);
            if delta < -RTP_HALF_MODULUS {
                self.accumulated_wrap = self.accumulated_wrap.saturating_add(RTP_MODULUS);
            } else if delta > RTP_HALF_MODULUS {
                self.accumulated_wrap = self.accumulated_wrap.saturating_sub(RTP_MODULUS);
            }
        }
        self.last_raw = Some(raw);
        raw.saturating_add(self.accumulated_wrap)
    }
}
