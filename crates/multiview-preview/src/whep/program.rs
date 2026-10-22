//! The **PROGRAM-output WebRTC focus path** (PRV-5, program scope) — gated behind
//! the off-by-default `webrtc` feature alongside the rest of [`super`].
//!
//! A *program focus* promotes the composed multiview canvas from the cheap
//! WS-JPEG grid to a single low-latency WebRTC preview encode (preview brief §4,
//! the `POST …/preview/program/whep` route). Per the brief's three-scope model
//! (§1) and ADR-P005, the canvas-tap path in this module is the *pre-encode
//! canvas* downscale, non-negotiably labeled
//! [`FidelityLabel::PreEncodeCanvasApprox`]. ADR-P006 (PRV-5b) extends the
//! real-tap preference to the program scope: when the program rendition itself
//! is WebRTC-compatible (H.264, B-frame-free), a program focus is instead fed
//! the real encoded bitstream via a fanout `PacketSink` on the
//! `multiview-output` `PacketRouter` and labeled
//! [`FidelityLabel::RealEncodedOutput`] — the label always names which path
//! fed the surface.
//!
//! ## What lives here (the testable core)
//!
//! * [`ProgramFrame`] — one sampled program-canvas frame (an NV12 plane + its
//!   geometry + a 90 kHz RTP timestamp). This is what the program downscale tap
//!   carries; the engine publishes it into a *dedicated* preview ring, never the
//!   encoder's NV12 readback ring (ADR-P001).
//! * [`ProgramTap`] — the **conditional tap**: a thin wrapper over a
//!   [`crate::TapRegistry`] keyed at the singleton `program` entity. The expensive
//!   GPU downscale blit (the `start` closure) runs **only** on the first
//!   subscriber (ADR-P003: zero cost when nobody is watching) and is torn down on
//!   the last leave.
//! * [`PreviewEncoder`] / [`IdentityPreviewEncoder`] — the NV12 → encoded-sample
//!   seam. The production path is a low-latency H.264 baseline encode; this seam
//!   keeps the wiring testable with no native codec, and the live H.264 + str0m
//!   egress lives in the `multiview-webrtc` crate's `native` WHEP egress
//!   transport (`WhepEgress`, ADR-0048 / ADR-P006).
//! * [`ProgramFocusSource`] — a [`PreviewMediaSource`] that **samples** the tap
//!   (drop-oldest, never pacing — inv #1/#10), runs each sampled frame through
//!   the encoder, and pushes the encoded sample into a bounded **drop-oldest**
//!   [`SampleSink`] the transport drains.
//! * [`ProgramFocusSession`] — the `program`-scope focus lifecycle: it owns the
//!   [`crate::FocusLease`] (the concurrency-cap slot, PRV-3) and the
//!   [`ProgramFocusSource`] (which owns the [`crate::TapLease`]). Dropping the
//!   session frees the cap slot **and** auto-stops the tap (last-leave teardown).
//!
//! ## What is NOT here (the live path)
//!
//! The GPU downscale blit needs a real GPU/compositor and the H.264 encode + str0m
//! ICE/DTLS/SRTP egress need a native stack + UDP/STUN reachability — none of which
//! is reliable in CI. The live str0m egress lives in `multiview-webrtc` (the
//! single str0m owner, ADR-0048 / ADR-P006); this
//! module is socket-free and codec-free: the tap, the encode *seam*, the bounded
//! feed, and the session lifecycle are all unit-testable with injected frames and
//! the in-memory fake transport in `tests/program_output_whep.rs`.
//!
//! ## Isolation (invariant #1 + #10)
//!
//! The program tap is a *preview* consumer: it subscribes to the engine's
//! dedicated drop-oldest program-preview ring and only ever **samples** the latest
//! published frame. It never paces, stalls, or back-pressures the protected output
//! clock; a slow, stalled, or absent focus consumer merely lags and loses the
//! oldest buffered frames (and the encode→feed leg drops the oldest encoded
//! samples). Nothing here publishes onto, or awaits, the protected output path.
use std::sync::Arc;

use multiview_engine::isolation::{EventSubscription, TryRecvError};

use crate::tap::{TapError, TapLease, TapRegistry};
use crate::token::{TapKey, TapScope};
use crate::whep::transport::{
    sample_feed, EncodedSample, PreviewMediaSource, SampleFeed, SampleKind, SampleSink,
};
use crate::whep::PreviewCodec;
use crate::FocusLease;

/// One sampled frame of the composed program canvas, as the downscale tap
/// produces it: an NV12 plane plus its geometry and a 90 kHz RTP timestamp.
///
/// NV12 layout (invariant #5): a `width * height` luma plane immediately
/// followed by a `width * height / 2` interleaved `CbCr` plane (1.5 B/px). The
/// preview core never materializes RGBA — the encoder seam re-packs these planes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramFrame {
    width: u32,
    height: u32,
    plane: Arc<[u8]>,
    rtp_timestamp: u32,
}

impl ProgramFrame {
    /// Build a program frame from an NV12 `plane` of `width`x`height` and its
    /// 90 kHz RTP `rtp_timestamp`.
    #[must_use]
    pub fn new(width: u32, height: u32, plane: impl Into<Arc<[u8]>>, rtp_timestamp: u32) -> Self {
        Self {
            width,
            height,
            plane: plane.into(),
            rtp_timestamp,
        }
    }

    /// The frame width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// The frame height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// The NV12 plane bytes (`width*height` luma + `width*height/2` `CbCr`).
    #[must_use]
    pub fn plane(&self) -> &[u8] {
        &self.plane
    }

    /// The frame's 90 kHz RTP presentation timestamp.
    #[must_use]
    pub const fn rtp_timestamp(&self) -> u32 {
        self.rtp_timestamp
    }
}

/// The fixed `program` tap key: the singleton composed-canvas entity.
fn program_key() -> TapKey {
    TapKey::new(TapScope::Program, "program")
}

/// The **conditional** program-canvas tap.
///
/// A thin, single-entity wrapper over a [`TapRegistry`] keyed at the singleton
/// `program` entity. Per ADR-P003 the expensive resource — the GPU downscale blit
/// that writes a small canvas copy into the dedicated preview ring — is created
/// **only** on the first subscriber and torn down on the last leave, so the tap
/// costs ~nothing while nobody is watching. Cheap to clone (it shares the
/// registry's `Arc`); hand a clone to the program-focus route.
#[derive(Clone, Default)]
pub struct ProgramTap {
    registry: TapRegistry<ProgramFrame>,
}

impl std::fmt::Debug for ProgramTap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProgramTap")
            .field("active", &self.subscriber_count())
            .finish()
    }
}

impl ProgramTap {
    /// Build an inactive program tap (no blit running).
    #[must_use]
    pub fn new() -> Self {
        Self {
            registry: TapRegistry::new(),
        }
    }

    /// Subscribe to the program canvas tap, lazily starting the downscale blit on
    /// the first subscriber.
    ///
    /// On the **first** subscriber `start` is invoked exactly once to append the
    /// GPU downscale blit and return `(subscription, stop)`, where `subscription`
    /// reads the dedicated program-preview ring and `stop` tears the blit down.
    /// On **subsequent** subscribers `start` is not called — every viewer fans out
    /// from the one shared tap. The returned [`TapLease`]'s `Drop` decrements the
    /// refcount and auto-stops the blit at zero (ADR-P003 idle cost).
    ///
    /// # Errors
    ///
    /// Returns [`TapError::Poisoned`] only if the registry's internal bookkeeping
    /// mutex was poisoned by a panic in another preview task — never the engine.
    pub fn subscribe<F, S>(&self, start: F) -> Result<TapLease<ProgramFrame>, TapError>
    where
        F: FnOnce() -> (EventSubscription<ProgramFrame>, S),
        S: FnOnce() + Send + 'static,
    {
        self.registry.subscribe(program_key(), start)
    }

    /// The number of live program-focus subscribers (`0` when the blit is idle).
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.registry.subscriber_count(&program_key())
    }

    /// Whether the program downscale blit is currently running (≥1 subscriber).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.subscriber_count() > 0
    }
}

/// The NV12 → encoded-sample seam the preview encoder pool implements.
///
/// The production path is a low-latency H.264 baseline encode of the downscaled
/// canvas; modeling it as a trait keeps the program-focus wiring testable with no
/// native codec ([`IdentityPreviewEncoder`]) and lets a hardware/software H.264
/// encoder plug in behind the same seam (the live path, gated behind
/// the `multiview-webrtc` `native` WHEP egress transport).
pub trait PreviewEncoder: Send + Sync {
    /// The codec the samples this encoder emits are encoded with.
    fn codec(&self) -> PreviewCodec;

    /// Encode one sampled program `frame` into an [`EncodedSample`] (or `None` if
    /// this frame produced no output access unit — e.g. a B-frame buffered by a
    /// real encoder). The 90 kHz RTP timestamp rides through from the frame.
    fn encode(&self, frame: &ProgramFrame) -> Option<EncodedSample>;
}

/// A dependency-free **identity** [`PreviewEncoder`] for the seam tests.
///
/// It does **no** real video compression: it wraps each program frame's NV12
/// plane bytes verbatim into an [`EncodedSample`], carrying the frame's 90 kHz
/// timestamp through and marking the first emitted sample a keyframe. It exists
/// only so the tap → encode → feed wiring and the session lifecycle are testable
/// with no native codec; the real low-latency H.264 baseline encoder plugs into
/// the same [`PreviewEncoder`] seam, with live egress via the `multiview-webrtc`
/// `native` transport (ADR-0048 / ADR-P006).
#[derive(Debug)]
pub struct IdentityPreviewEncoder {
    codec: PreviewCodec,
    emitted: std::sync::atomic::AtomicU64,
}

impl IdentityPreviewEncoder {
    /// Build an identity encoder advertising `codec`.
    #[must_use]
    pub const fn new(codec: PreviewCodec) -> Self {
        Self {
            codec,
            emitted: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

impl PreviewEncoder for IdentityPreviewEncoder {
    fn codec(&self) -> PreviewCodec {
        self.codec
    }

    fn encode(&self, frame: &ProgramFrame) -> Option<EncodedSample> {
        // The first emitted sample is the keyframe (a real encoder forces an IDR
        // for the first delivered access unit so the decoder has a sync point).
        let first = self
            .emitted
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            == 0;
        Some(EncodedSample {
            data: Arc::clone(&frame.plane),
            rtp_timestamp: frame.rtp_timestamp,
            keyframe: first,
            // A program-canvas encode is video on the 90 kHz RTP clock.
            kind: SampleKind::Video,
        })
    }
}

/// A [`PreviewMediaSource`] for a `program`-scope focus: it samples the program
/// tap, encodes each sampled frame, and pushes the encoded sample into a bounded
/// **drop-oldest** feed the transport drains.
///
/// This canvas-tap source is **video-only** — its samples are
/// [`SampleKind::Video`] and its [`PreviewMediaSource::audio_feed`] is the
/// trait default `None`: program audio is the shared program Opus rendition
/// (ADR-0049), a different source, never the canvas tap (ADR-P006).
///
/// ## Isolation (invariant #1 + #10)
///
/// [`Self::pump_available`] drains the tap lease with non-blocking `try_recv` and
/// the [`SampleSink`] push is wait-free drop-oldest — neither paces the output
/// clock nor back-pressures the publisher. A program focus is best-effort and
/// sheddable-first (PRV-4 rung): if the encoder is shed or the feed lags, the
/// oldest sampled/encoded frames are dropped, never queued against the engine.
pub struct ProgramFocusSource<E: PreviewEncoder> {
    /// The program-canvas tap lease, behind a short-lived bookkeeping `Mutex` so
    /// the `&self` pump can drive its `&mut`-only `try_recv`. This mutex is preview
    /// bookkeeping the engine never touches (invariant #10).
    lease: std::sync::Mutex<TapLease<ProgramFrame>>,
    encoder: E,
    sink: SampleSink,
    feed: std::sync::Mutex<Option<SampleFeed>>,
    codec: PreviewCodec,
}

impl<E: PreviewEncoder + std::fmt::Debug> std::fmt::Debug for ProgramFocusSource<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProgramFocusSource")
            .field("codec", &self.codec)
            .field("buffered", &self.sink.buffered())
            .finish_non_exhaustive()
    }
}

impl<E: PreviewEncoder> ProgramFocusSource<E> {
    /// Build a program-focus source over `lease` (a program-canvas tap lease),
    /// encoding with `encoder` into a bounded drop-oldest feed of ring `depth`.
    ///
    /// `depth` is the shallow preview ring depth (ADR-P001: 1–3); it is clamped to
    /// at least 1 by [`sample_feed`]. The codec the source advertises is the
    /// encoder's codec.
    #[must_use]
    pub fn new(lease: TapLease<ProgramFrame>, encoder: E, depth: usize) -> Self {
        let codec = encoder.codec();
        let (sink, feed) = sample_feed(depth);
        Self {
            lease: std::sync::Mutex::new(lease),
            encoder,
            sink,
            feed: std::sync::Mutex::new(Some(feed)),
            codec,
        }
    }

    /// Sample and encode **all** currently-buffered program frames, pushing each
    /// encoded sample into the drop-oldest feed. Returns how many samples were
    /// produced.
    ///
    /// Non-blocking and sampling-only: it drains the tap lease with `try_recv`
    /// until the ring is empty (or the publisher is gone), so it never awaits a
    /// frame and never paces the engine (invariant #1). A `Lagged` skip (the focus
    /// consumer fell behind the drop-oldest ring) is absorbed — the loop simply
    /// continues from the oldest still-buffered frame, exactly the inv-#10
    /// contract. Synchronous on purpose: sampling the slot must never block, so
    /// there is nothing to await.
    pub fn pump_available(&self) -> usize {
        // Hold the lease's bookkeeping mutex for the drain. This lock is never
        // touched by the engine (invariant #10); if it is poisoned (a panic in
        // another preview task) we produce nothing rather than propagate a panic —
        // preview is best-effort.
        let Ok(mut lease) = self.lease.lock() else {
            return 0;
        };
        let mut produced = 0usize;
        loop {
            match lease.try_recv() {
                Ok(seq) => {
                    if let Some(sample) = self.encoder.encode(&seq.event) {
                        // The lag flag is the "preview falling behind" signal; the
                        // push is wait-free drop-oldest and the pump never blocks
                        // on it.
                        let _ = self.sink.push(sample);
                        produced = produced.saturating_add(1);
                    }
                }
                // Lagged: the focus consumer skipped some frames (drop-oldest); the
                // loop continues from the oldest still-buffered frame.
                Err(TryRecvError::Lagged(_)) => {}
                // Nothing more buffered, or the publisher is gone: done for now.
                Err(TryRecvError::Empty | TryRecvError::Closed) => break,
            }
        }
        produced
    }

    /// The drop-oldest feed the transport drains encoded samples from.
    ///
    /// Taken exactly once when the transport accepts the session (matching the
    /// [`PreviewMediaSource::feed`] contract). A second call yields a fresh empty
    /// feed rather than panicking — preview is best-effort.
    #[must_use]
    pub fn take_feed(&self) -> SampleFeed {
        self.feed
            .lock()
            .ok()
            .and_then(|mut g| g.take())
            .unwrap_or_else(|| sample_feed(1).1)
    }
}

impl<E: PreviewEncoder> PreviewMediaSource for ProgramFocusSource<E> {
    fn codec(&self) -> PreviewCodec {
        self.codec
    }

    fn feed(&self) -> SampleFeed {
        self.take_feed()
    }
}

/// The fidelity label every preview surface carries on-video (ADR-P005).
///
/// The label is non-negotiable and names which path fed the surface. The
/// canvas-tap path in this module ([`ProgramFocusSource`]) is **always**
/// [`Self::PreEncodeCanvasApprox`] — it is the pre-encode canvas downscale,
/// not the real encoded bitstream. [`Self::RealEncodedOutput`] marks a tap of
/// a real encoded rendition (carrying the tapped protocol): an OUTPUT-scope
/// preview, **or** — per ADR-P006 (PRV-5b) — a PROGRAM focus fed the real
/// program bitstream via a fanout `PacketSink` registered on the
/// `multiview-output` `PacketRouter` when the program rendition is
/// WebRTC-compatible (H.264, B-frame-free); otherwise the program focus falls
/// back to this module's canvas-approx path and label.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FidelityLabel {
    /// The surface is the pre-encode program canvas downscale, not a real
    /// encoded output. The mandatory label for the canvas-approx path — the
    /// program-focus fallback whenever the real rendition does not qualify
    /// (ADR-P006).
    PreEncodeCanvasApprox,
    /// The surface is a tap of a real encoded output rendition over `protocol`
    /// (e.g. `"rtsp"`, `"ll-hls"`). Used by OUTPUT-scope previews and by
    /// PROGRAM-scope sessions fed from the fanout `PacketSink` tap of a
    /// WebRTC-compatible program rendition (PRV-5b / ADR-P006).
    RealEncodedOutput {
        /// The tapped output protocol (the `<proto>` in `tap:<proto>`).
        protocol: String,
    },
}

impl FidelityLabel {
    /// The label for a program focus on the **canvas-approx** path: pre-encode
    /// canvas approx. (A program focus fed by the real-rendition fanout tap is
    /// labeled [`Self::RealEncodedOutput`] instead — ADR-P006.)
    #[must_use]
    pub const fn program() -> Self {
        Self::PreEncodeCanvasApprox
    }

    /// Whether this label denotes a real encoded output tap — true for an
    /// OUTPUT-scope tap and for a program focus fed via the fanout
    /// `PacketSink` path (PRV-5b / ADR-P006).
    #[must_use]
    pub const fn is_real_encoded(&self) -> bool {
        matches!(self, Self::RealEncodedOutput { .. })
    }

    /// The on-video / descriptor label string (ADR-P005's exact wording).
    #[must_use]
    pub fn as_str(&self) -> String {
        match self {
            Self::PreEncodeCanvasApprox => "PRE-ENCODE CANVAS APPROX".to_owned(),
            Self::RealEncodedOutput { protocol } => {
                format!("REAL ENCODED OUTPUT (tap:{protocol})")
            }
        }
    }
}

/// A live `program`-scope WebRTC focus session.
///
/// It owns the two leases that bound the focus: the [`FocusLease`] (the
/// concurrency-cap slot, PRV-3 — freed on `Drop`) and the [`ProgramFocusSource`]
/// (which owns the [`TapLease`] — auto-stopping the downscale blit on the last
/// leave, ADR-P003). Dropping the session therefore frees the cap slot **and**
/// tears the tap down, in one move. As a canvas-tap session its fidelity label
/// is fixed to [`FidelityLabel::PreEncodeCanvasApprox`] (ADR-P005); the
/// real-rendition program path of ADR-P006 (PRV-5b) rides the fanout
/// `PacketSink`, not this type.
///
/// The transport's per-session ICE/DTLS/SRTP lifecycle is tracked separately by
/// the [`crate::whep::transport::SessionHandle`] the transport returns; this type
/// owns the *preview-side* resources (cap + tap + encode feed) so a torn-down
/// session releases them deterministically.
pub struct ProgramFocusSession<K, E>
where
    K: Eq + std::hash::Hash + Clone,
    E: PreviewEncoder,
{
    // Field order is the drop order: the cap lease drops first, then the source
    // (and its tap lease). Both are independent; the order is not load-bearing,
    // but is fixed for clarity.
    _cap: FocusLease<K>,
    source: ProgramFocusSource<E>,
}

impl<K, E> std::fmt::Debug for ProgramFocusSession<K, E>
where
    K: Eq + std::hash::Hash + Clone + std::fmt::Debug,
    E: PreviewEncoder + std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The `_cap` focus lease is deliberately omitted (it holds only a
        // back-reference to the gate); `finish_non_exhaustive` records that.
        f.debug_struct("ProgramFocusSession")
            .field("label", &FidelityLabel::PreEncodeCanvasApprox)
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

impl<K, E> ProgramFocusSession<K, E>
where
    K: Eq + std::hash::Hash + Clone,
    E: PreviewEncoder,
{
    /// Build a program-focus session from an admitted cap lease and a media
    /// source. The session owns both; dropping it frees the cap and the tap.
    #[must_use]
    pub fn new(cap: FocusLease<K>, source: ProgramFocusSource<E>) -> Self {
        Self { _cap: cap, source }
    }

    /// The fidelity label for this session — always
    /// [`FidelityLabel::PreEncodeCanvasApprox`], because this type is the
    /// canvas-tap path (ADR-P005). A program focus fed the real program
    /// bitstream via the fanout `PacketSink` (PRV-5b / ADR-P006) carries
    /// [`FidelityLabel::RealEncodedOutput`] and does not ride this type.
    #[must_use]
    pub const fn label(&self) -> FidelityLabel {
        FidelityLabel::PreEncodeCanvasApprox
    }

    /// The media source the transport drains (for `feed()` / `pump_available()`).
    #[must_use]
    pub fn source(&self) -> &ProgramFocusSource<E> {
        &self.source
    }
}
