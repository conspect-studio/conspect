//! GP-7 — the guarded-passthrough splice seam (the `ffmpeg` feature; ADR-0030 §4
//! "The splice seam — `GuardedPacketSource`").
//!
//! A **guarded passthrough** (ADR-0030 §4) is packet-copy while the input is
//! healthy and splices a **pre-baked** slate (GP-4 [`BakedSlate`]) into the
//! copied elementary stream on input loss — so failover costs **zero** live
//! encode and holds **zero** of the gated NVENC concurrent-session ceiling
//! (invariant-#1-PRESERVING: the degenerate clock emits a valid coded AU every
//! tick, forever, and never `.await`s an input).
//!
//! [`GuardedPacketSource`] is that seam. It implements
//! [`PacketSource`](crate::sink::PacketSource) and is the **sole** producer
//! feeding [`PacketMuxSink::run_av`](crate::sink::PacketMuxSink::run_av). Each
//! [`next_packet`](GuardedPacketSource::next_packet) returns either the next
//! copied input packet (while LIVE) or the next pre-baked slate packet (while
//! SLATE, looped by advancing the GP-6 restamp offset per wrap) — re-stamped so
//! the muxer's interleaved write never aborts on non-monotonic DTS.
//!
//! It also primes the RT-13 (ADR-0034) seamless output→program switch: the same
//! copy-vs-alternate AtomicU8 flip + per-stream restamp + strict-IDR re-entry is
//! the general primitive that reuses unchanged.
//!
//! ## Threading model (ADR-0030 §4 isolation requirement)
//!
//! The single cross-thread coupling is one wait-free `AtomicU8` **decision** flag
//! (`COPY` | `SPLICE`):
//!
//! * The **decision** side runs the GP-5 watchdog
//!   ([`PacketLiveness::should_splice`]) over an injected monotonic clock and
//!   **`Release`-writes** the flag ([`evaluate`](GuardedPacketSource::evaluate)).
//!   In the assembled engine (GP-8) this is the passthrough's degenerate
//!   `EngineRuntime` clock thread, ticking on cadence; here it can equally be
//!   driven inline.
//! * The **egress** side ([`next_packet`](GuardedPacketSource::next_packet),
//!   driven by `PacketMuxSink::run_av` on its dedicated mux thread)
//!   **`Acquire`-reads** the flag to choose copied-input vs slate bytes.
//!
//! Because a true second OS thread is awkward to *require* inside a
//! `PacketSource::next_packet` pull loop, `next_packet` **self-drives** the
//! decision once per call from the injected clock (it calls `evaluate` itself)
//! **in addition to** any external clock-thread driver — both paths only ever
//! `Release`-write the same flag, and the watchdog read is **fail-safe** (a stale
//! `Acquire` read of an older stamp yields a larger elapsed ⇒ biases toward
//! `SPLICE`, never a false `LIVE`), so the two drivers are race-free and
//! identical in effect. The copy side `record_packet`s the watchdog (Release) on
//! every pulled input packet (the producer-liveness clock).
//!
//! **Recovery is is_idr-gated, independent of the watchdog flip.** Once spliced,
//! the seam stays SLATE — even after the input resumes and the watchdog flips
//! back to `COPY` — until a **TRUE strict-IDR** video AU arrives (GP-1
//! [`is_idr`], **not** `AV_PKT_FLAG_KEY`, which `FFmpeg` sets for HEVC CRA and
//! H.264 recovery-point I-frames whose leading pictures reference now-absent
//! frames). Only at a strict RAP does it re-anchor the offset and resume copy.
//!
//! ## Isolation (#1/#10)
//!
//! `next_packet` is **wait-free with respect to the input**: it never blocks on
//! the demuxer (the live [`PacketSource`] is itself non-blocking — its demuxer is
//! opened with the GP-0 `AVIOInterruptCB` + `rw_timeout`) and never `.await`s. A
//! wedged downstream **push peer** is shed by the existing drop-oldest +
//! `SINK_WEDGE_GRACE` detach posture wrapping the mux thread (the cli's egress
//! fan-out), so it can never back-pressure this producer — the decision side
//! keeps flipping the flag and the egress keeps emitting a valid AU regardless.
//!
//! ## Re-stamp (#3 for the copy path)
//!
//! One persistent [`RestampAccumulator`] **per stream** (video + audio, which
//! fail independently) spans BOTH seams. The monotonic clamp (`last_dts + 1`) is
//! the abort guard; only the `offset` changes at a boundary, so raw deltas
//! (durations + the B-frame reorder gap) pass through untouched. The seam
//! `rebase`s re-anchor the offset: input→slate (the slate's first IDR lands at
//! `last_dts + 1`), each slate loop wrap (the offset advances by one loop
//! duration), and slate→input recovery (the recovery IDR lands at `last_dts + 1`).
//!
//! ## No new FFI
//!
//! This module writes **no** `unsafe` and performs **no** FFI: it re-stamps a
//! packet by taking its owned (ref-counted, writable) copy from the safe
//! [`EncodedPacket`] wrapper, setting the new `(dts, pts)` through `ffmpeg_next`'s
//! safe `Packet` setters, and re-wrapping by [`StreamKind`]. The crate stays
//! `forbid(unsafe_code)`.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Instant;

use multiview_ffmpeg::{is_idr, CodecKind, EncodedPacket, NalFraming, StreamKind};
use multiview_framestore::PacketLiveness;

use crate::restamp::RestampAccumulator;
use crate::sink::PacketSource;
use crate::slate::BakedSlate;
use crate::Result;

/// A monotonic nanosecond clock the guarded seam samples for its liveness
/// decision (the injected `TimeSource` of ADR-0030 §4).
///
/// Mirrors the engine's `TimeSource` contract (monotonic non-decreasing
/// `now_nanos`) without depending on `multiview-engine` (which depends on this
/// crate — the dependency must not cycle). Implemented by [`RealMonotonicClock`]
/// in production and [`ManualClock`] in tests / deterministic drives.
pub trait MonotonicClock: Send + Sync {
    /// Nanoseconds since this clock's fixed origin. Monotonic non-decreasing.
    fn now_nanos(&self) -> i64;
}

impl<C: MonotonicClock + ?Sized> MonotonicClock for Arc<C> {
    fn now_nanos(&self) -> i64 {
        (**self).now_nanos()
    }
}

/// A real monotonic clock backed by [`std::time::Instant`] (`CLOCK_MONOTONIC` on
/// Linux, `mach_continuous_time` on macOS).
#[derive(Debug, Clone)]
pub struct RealMonotonicClock {
    origin: Instant,
}

impl RealMonotonicClock {
    /// A clock whose origin is now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for RealMonotonicClock {
    fn default() -> Self {
        Self::new()
    }
}

impl MonotonicClock for RealMonotonicClock {
    fn now_nanos(&self) -> i64 {
        // `Instant` is monotonic by contract; saturate at ~292 years rather than
        // risk an `as`-cast or an overflow panic.
        i64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(i64::MAX)
    }
}

/// A manually-advanced monotonic clock for deterministic drives and tests.
///
/// Time only moves when [`advance`](ManualClock::advance) / [`set`](ManualClock::set)
/// is called, so a guarded seam can be stepped across its splice/recovery seams
/// with zero real sleeps and zero flakiness. Wait-free (one [`AtomicI64`]), so it
/// is sound to share (e.g. an `Arc<ManualClock>`) between a decision driver and
/// the egress.
///
/// [`AtomicI64`]: std::sync::atomic::AtomicI64
#[derive(Debug)]
pub struct ManualClock {
    now_nanos: std::sync::atomic::AtomicI64,
}

impl ManualClock {
    /// A clock positioned at `t = 0`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            now_nanos: std::sync::atomic::AtomicI64::new(0),
        }
    }

    /// Advance by `delta_ns` (saturating; never moves backwards under a
    /// non-negative `delta_ns`).
    pub fn advance(&self, delta_ns: i64) {
        let prev = self.now_nanos.load(Ordering::Acquire);
        self.now_nanos
            .store(prev.saturating_add(delta_ns.max(0)), Ordering::Release);
    }

    /// Set the clock to `now_ns` (monotonic: never moves backwards).
    pub fn set(&self, now_ns: i64) {
        let prev = self.now_nanos.load(Ordering::Acquire);
        self.now_nanos.store(prev.max(now_ns), Ordering::Release);
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl MonotonicClock for ManualClock {
    fn now_nanos(&self) -> i64 {
        self.now_nanos.load(Ordering::Acquire)
    }
}

/// What the guarded seam is currently emitting on the egress side.
///
/// The publicly observable egress truth (the per-program robustness badge / the
/// chaos-gate assertion read it). Distinct from the internal cross-thread
/// **decision** flag: the egress can lag the decision because slate→input
/// recovery is is_idr-gated (the seam stays [`Slate`](GuardMode::Slate) after the
/// watchdog flips back to copy, until a true IDR arrives).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum GuardMode {
    /// Copying the live input packets (healthy steady state).
    Live,
    /// Emitting the pre-baked slate (input lost; failover).
    Slate,
}

/// The wait-free cross-thread **decision** flag value: copy the live input.
const DECISION_COPY: u8 = 0;
/// The wait-free cross-thread **decision** flag value: splice to slate.
const DECISION_SPLICE: u8 = 1;

/// Static configuration of a [`GuardedPacketSource`]: how to classify a strict
/// IDR on the video stream (GP-1) and the program frame interval used by the
/// slate-loop wrap accounting.
#[derive(Debug, Clone, Copy)]
pub struct GuardedConfig {
    /// The video codec, for the GP-1 strict-IDR classifier (`is_idr`).
    video_codec: CodecKind,
    /// The video NAL/OBU framing, for the GP-1 strict-IDR classifier.
    video_framing: NalFraming,
    /// The program frame interval `T = 1/fps` in nanoseconds (documented for the
    /// degenerate-clock pacing the slate loop rides; the wrap accounting itself
    /// is stream-unit, not wall-clock).
    frame_interval_ns: i64,
}

impl GuardedConfig {
    /// Build the config from the probed video codec + framing (GP-2 probe) and
    /// the program frame interval.
    #[must_use]
    pub const fn new(
        video_codec: CodecKind,
        video_framing: NalFraming,
        frame_interval_ns: i64,
    ) -> Self {
        Self {
            video_codec,
            video_framing,
            frame_interval_ns,
        }
    }

    /// The video codec for the strict-IDR classifier.
    #[must_use]
    pub const fn video_codec(&self) -> CodecKind {
        self.video_codec
    }

    /// The video framing for the strict-IDR classifier.
    #[must_use]
    pub const fn video_framing(&self) -> NalFraming {
        self.video_framing
    }

    /// The program frame interval `T = 1/fps` in nanoseconds.
    #[must_use]
    pub const fn frame_interval_ns(&self) -> i64 {
        self.frame_interval_ns
    }
}

/// Per-stream re-stamp + slate-loop position state.
///
/// One of these governs the video stream and (when present) one the audio
/// stream. The [`RestampAccumulator`] spans both seams; `slate_pos` is the
/// index into the slate's coded ring for this stream (looped, advancing the
/// restamp offset per wrap).
#[derive(Debug)]
struct StreamState {
    /// The persistent monotonic clamp + per-boundary offset (GP-6).
    restamp: RestampAccumulator,
    /// The slate replay cursor for this stream (`0..slate_len`, looped).
    slate_pos: usize,
    /// Whether the slate's first packet of the *current* loop has been
    /// re-anchored yet (so we `rebase` once per loop entry / wrap). Tracks the
    /// SLATE loop only — distinct from [`Self::live_reentry_pending`].
    slate_rebased: bool,
    /// Whether this stream's next LIVE packet must re-anchor the offset (set on
    /// a slate→input recovery so the copied stream re-enters at `last_dts + 1`).
    /// Audio uses this to re-enter "at its next AU boundary"; video re-anchors
    /// directly on the recovery IDR.
    live_reentry_pending: bool,
}

impl StreamState {
    const fn new() -> Self {
        Self {
            restamp: RestampAccumulator::new(),
            slate_pos: 0,
            slate_rebased: false,
            live_reentry_pending: false,
        }
    }
}

/// The guarded-passthrough splice seam: a [`PacketSource`] that copies the live
/// input while healthy and splices the pre-baked slate on loss (ADR-0030 §4).
///
/// `L` is the live input packet source (a [`PacketSource`] over copied input
/// packets — a demuxer-backed source in production, a fake in tests); `C` is the
/// injected monotonic clock the liveness decision samples.
#[derive(Debug)]
pub struct GuardedPacketSource<L: PacketSource, C: MonotonicClock> {
    /// The live input packet source (copied input packets).
    live: L,
    /// The pre-baked failover slate (GP-4).
    slate: BakedSlate,
    /// The video packet-liveness watchdog (GP-5); the copy side `record_packet`s
    /// it, the decision side `classify`/`should_splice`s it.
    video_liveness: Arc<PacketLiveness>,
    /// The optional audio packet-liveness watchdog (per-PID independent failure);
    /// `None` for a video-only program.
    audio_liveness: Option<Arc<PacketLiveness>>,
    /// The injected monotonic clock.
    clock: C,
    /// Static config (strict-IDR classifier inputs + cadence).
    config: GuardedConfig,
    /// The wait-free cross-thread decision flag (`COPY` | `SPLICE`),
    /// `Release`-written by [`evaluate`](Self::evaluate), `Acquire`-read by
    /// [`next_packet`](Self::next_packet). Behind `Arc` so a clock thread can
    /// hold a clone and flip it independently (the single cross-thread coupling).
    decision: Arc<AtomicU8>,
    /// What the egress is currently emitting (the observable truth; lags the
    /// decision across is_idr-gated recovery).
    emitting: GuardMode,
    /// Per-stream restamp + slate-loop cursors.
    video: StreamState,
    audio: StreamState,
    /// During a SLATE outage, which slate stream to emit next (round-robin
    /// video↔audio so both ride the failover; the muxer re-interleaves by DTS).
    emit_audio_next: bool,
    /// Whether at least one live input packet has ever been recorded.
    ///
    /// Startup grace: a freshly-opened passthrough begins LIVE and must give the
    /// input the chance to deliver its first packet — the watchdog has nothing
    /// recorded yet, so its `should_splice` reads `true` (fail-safe), but that
    /// `true` means "splice if no byte arrives", not "splice instead of the first
    /// read". So before `started`, a `SPLICE` decision does not short-circuit the
    /// copy: we attempt the pull, and only splice if it yields nothing (hard
    /// death). Once `started`, a `SPLICE` decision is an observed loss and splices
    /// immediately. This preserves "never false-LIVE" (we never claim LIVE
    /// without an actual packet) while not failing a healthy startup to slate.
    started: bool,
}

impl<L: PacketSource, C: MonotonicClock> GuardedPacketSource<L, C> {
    /// Assemble a guarded passthrough over the live source `live`, the pre-baked
    /// `slate`, the GP-5 watchdog(s), the injected `clock`, and the strict-IDR
    /// `config`.
    ///
    /// The seam starts emitting [`GuardMode::Live`]; the first
    /// [`next_packet`](Self::next_packet) self-evaluates the watchdog, so a seam
    /// over a never-recorded input fails safe to slate immediately (the watchdog
    /// reports splice before any byte arrives).
    #[must_use]
    pub fn new(
        live: L,
        slate: BakedSlate,
        video_liveness: Arc<PacketLiveness>,
        audio_liveness: Option<Arc<PacketLiveness>>,
        clock: C,
        config: GuardedConfig,
    ) -> Self {
        Self {
            live,
            slate,
            video_liveness,
            audio_liveness,
            clock,
            config,
            decision: Arc::new(AtomicU8::new(DECISION_COPY)),
            emitting: GuardMode::Live,
            video: StreamState::new(),
            audio: StreamState::new(),
            emit_audio_next: false,
            started: false,
        }
    }

    /// The wait-free decision flag, clonable so a clock/decision thread (GP-8)
    /// can drive [`evaluate`](Self::evaluate) independently of the egress.
    ///
    /// Both drivers only ever `Release`-write the flag; the read is fail-safe, so
    /// sharing it across threads is race-free (ADR-0030 §4).
    #[must_use]
    pub fn decision_flag(&self) -> Arc<AtomicU8> {
        Arc::clone(&self.decision)
    }

    /// What the egress is currently emitting (the observable robustness state).
    #[must_use]
    pub fn mode(&self) -> GuardMode {
        self.emitting
    }

    /// Run the GP-5 watchdog as of `now_ns` and `Release`-write the decision
    /// flag (`SPLICE` if either the video **or** the audio stream should splice;
    /// else `COPY`). Wait-free; called inline by [`next_packet`](Self::next_packet)
    /// from the injected clock, and additionally by a clock thread in the
    /// assembled engine.
    ///
    /// This only governs the LIVE→SLATE direction; the SLATE→LIVE direction is
    /// is_idr-gated in [`next_packet`](Self::next_packet) (the flag flipping back
    /// to `COPY` does not by itself resume the copy).
    pub fn evaluate(&self, now_ns: i64) {
        let video_splice = self.video_liveness.should_splice(now_ns);
        let audio_splice = self
            .audio_liveness
            .as_ref()
            .is_some_and(|a| a.should_splice(now_ns));
        let flag = if video_splice || audio_splice {
            DECISION_SPLICE
        } else {
            DECISION_COPY
        };
        self.decision.store(flag, Ordering::Release);
    }

    /// Whether the decision flag currently says splice (`Acquire`).
    fn decision_is_splice(&self) -> bool {
        self.decision.load(Ordering::Acquire) == DECISION_SPLICE
    }

    /// Record a pulled live input packet into the matching watchdog (the
    /// producer-liveness clock; Release), using the packet's own DTS (or its PTS
    /// when DTS is absent) as the advancing-DTS signal.
    fn record_live(&self, now_ns: i64, packet: &EncodedPacket) {
        let dts = packet.dts().or_else(|| packet.pts()).unwrap_or(0);
        match packet.kind() {
            StreamKind::Video => self.video_liveness.record_packet(now_ns, dts),
            StreamKind::Audio => {
                if let Some(audio) = &self.audio_liveness {
                    audio.record_packet(now_ns, dts);
                }
            }
            // A future elementary-stream kind is recorded against neither
            // watchdog (it cannot keep video/audio alive); the existing streams'
            // liveness is unaffected.
            _ => {}
        }
    }

    /// Whether `packet` is a TRUE strict-IDR video AU (GP-1 `is_idr`, **not**
    /// `AV_PKT_FLAG_KEY`). Audio / non-video packets are never a video RAP.
    fn is_video_idr(&self, packet: &EncodedPacket) -> bool {
        if packet.kind() != StreamKind::Video {
            return false;
        }
        // Take the owned (ref-counted, no data copy) packet to read the AU bytes
        // through the safe wrapper; `is_idr` is a cheap header inspection.
        let owned = packet.to_owned_packet();
        match owned.data() {
            Some(au) => is_idr(au, self.config.video_codec, self.config.video_framing),
            None => false,
        }
    }

    /// Re-stamp `packet` for the muxer: apply this stream's [`RestampAccumulator`]
    /// to its raw `(dts, pts)`, set the clamped pair on an owned copy, and
    /// re-wrap by [`StreamKind`] (preserving the keyframe flag, which rides the
    /// raw packet).
    ///
    /// A packet with no DTS uses its PTS for both inputs (the clamp still pins
    /// monotonicity); the emitted packet always carries an explicit `(dts, pts)`.
    // `raw_dts`/`raw_pts` are the irreducible domain terms of the GP-6 restamp
    // rule (they differ only by the canonical d/p suffix `similar_names` flags);
    // renaming would diverge from the ADR-0030 §4 formula — same suppression as
    // `restamp.rs::restamp`.
    #[allow(clippy::similar_names)]
    fn restamp_emit(state: &mut StreamState, packet: &EncodedPacket) -> EncodedPacket {
        let kind = packet.kind();
        let raw_dts = packet.dts().or_else(|| packet.pts()).unwrap_or(0);
        let raw_pts = packet.pts().or_else(|| packet.dts()).unwrap_or(raw_dts);
        let (dts, pts) = state.restamp.restamp(raw_dts, raw_pts);
        let mut owned = packet.to_owned_packet();
        owned.set_dts(Some(dts));
        owned.set_pts(Some(pts));
        match kind {
            StreamKind::Audio => EncodedPacket::from_audio_packet(owned),
            // Video (and, defensively, any other kind) re-wraps as video — the
            // guarded seam only ever carries program video + audio.
            _ => EncodedPacket::from_packet(owned),
        }
    }

    /// Re-stamp and emit one copied LIVE input packet, routing by stream.
    ///
    /// A live **audio** packet re-entering after a recovery re-anchors its offset
    /// at this AU boundary (ADR-0030 §4: "audio re-enters at its next AU
    /// boundary"); the `slate_rebased` latch flips so the rebase fires exactly
    /// once per re-entry, and steady-state live audio then passes through with the
    /// running offset (raw deltas preserved).
    fn emit_live(&mut self, packet: &EncodedPacket) -> EncodedPacket {
        let state = match packet.kind() {
            StreamKind::Audio => &mut self.audio,
            _ => &mut self.video,
        };
        if state.live_reentry_pending {
            let raw = packet.dts().or_else(|| packet.pts()).unwrap_or(0);
            state.restamp.rebase(raw);
            state.live_reentry_pending = false;
        }
        Self::restamp_emit(state, packet)
    }

    /// Transition LIVE→SLATE: re-anchor both stream restamps onto the slate's
    /// first coded packet (boundary 1) and reset the slate cursors. Idempotent
    /// for the already-SLATE case (no-op).
    fn enter_slate(&mut self) {
        if self.emitting == GuardMode::Slate {
            return;
        }
        self.emitting = GuardMode::Slate;
        self.video.slate_pos = 0;
        self.video.slate_rebased = false;
        self.audio.slate_pos = 0;
        self.audio.slate_rebased = false;
        self.emit_audio_next = false;
    }

    /// Pull-and-emit the next SLATE packet, round-robining video↔audio so both
    /// failover streams ride the outage. Advances the per-stream cursor, `rebase`s
    /// once per loop entry/wrap (boundary-1 / per-wrap offset advance), and
    /// re-stamps. Always yields a packet (the slate has >= 1 video AU).
    fn next_slate(&mut self) -> EncodedPacket {
        // Prefer audio only when it's audio's turn AND audio exists; otherwise
        // video. This keeps a video-only slate purely video and interleaves a
        // video+audio slate (the muxer re-orders by DTS regardless).
        let want_audio = self.emit_audio_next && self.slate_has_audio();
        self.emit_audio_next = !self.emit_audio_next && self.slate_has_audio();
        if want_audio {
            if let Some(pkt) = self.next_slate_stream(false) {
                return pkt;
            }
        }
        // Video path (the slate always has >= 1 video AU, so this branch is the
        // guaranteed producer).
        match self.next_slate_stream(true) {
            Some(pkt) => pkt,
            // The slate is guaranteed non-empty on video (GP-4 bakes >= 1 video
            // AU); if it somehow were empty, fall back to an audio packet rather
            // than stall. This branch is unreachable for a valid BakedSlate.
            None => self
                .next_slate_stream(false)
                .unwrap_or_else(|| self.empty_video_keepalive()),
        }
    }

    /// Whether the slate carries audio AUs.
    fn slate_has_audio(&self) -> bool {
        self.slate.audio().is_some_and(|a| !a.is_empty())
    }

    /// Emit the next slate packet for one stream (video if `video`, else audio),
    /// advancing + looping its cursor and re-stamping. `None` if that stream's
    /// ring is empty.
    fn next_slate_stream(&mut self, video: bool) -> Option<EncodedPacket> {
        let ring: &Arc<[EncodedPacket]> = if video {
            self.slate.video()
        } else {
            self.slate.audio()?
        };
        if ring.is_empty() {
            return None;
        }
        let state = if video {
            &mut self.video
        } else {
            &mut self.audio
        };

        // At the first packet of a loop (cursor 0), re-anchor the offset to the
        // slate's first raw DTS so the loop continues at `last_dts + 1` with the
        // raw deltas preserved (boundary-1 on entry; per-wrap offset advance).
        if state.slate_pos == 0 && !state.slate_rebased {
            if let Some(first) = ring.first() {
                let raw = first.dts().or_else(|| first.pts()).unwrap_or(0);
                state.restamp.rebase(raw);
                state.slate_rebased = true;
            }
        }

        let packet = ring.get(state.slate_pos)?;
        let emitted = Self::restamp_emit(state, packet);

        // Advance + wrap.
        state.slate_pos += 1;
        if state.slate_pos >= ring.len() {
            state.slate_pos = 0;
            // Force a re-anchor at the next loop entry (the per-wrap offset
            // advance), so the replayed raw DTS climbs past `last_dts`.
            state.slate_rebased = false;
        }
        Some(emitted)
    }

    /// A last-resort empty video keep-alive packet (only reachable if a slate
    /// were baked with zero video AUs, which GP-4 never produces). Carries the
    /// running monotonic DTS so the stream never regresses.
    fn empty_video_keepalive(&mut self) -> EncodedPacket {
        let (dts, pts) = self.video.restamp.restamp(0, 0);
        let mut owned = ffmpeg_next::codec::packet::Packet::empty();
        owned.set_dts(Some(dts));
        owned.set_pts(Some(pts));
        EncodedPacket::from_packet(owned)
    }

    /// Transition SLATE→LIVE at a recovery IDR (boundary 2): re-anchor the video
    /// restamp onto the recovery IDR's raw DTS so it lands at `last_dts + 1`, and
    /// re-anchor audio at its next AU boundary as it re-enters.
    fn recover_to_live(&mut self, idr: &EncodedPacket) -> EncodedPacket {
        self.emitting = GuardMode::Live;
        let raw = idr.dts().or_else(|| idr.pts()).unwrap_or(0);
        self.video.restamp.rebase(raw);
        // Audio re-enters at its next AU boundary (ADR-0030 §4): flag the audio
        // stream so the first LIVE audio packet `emit_live` sees re-anchors its
        // offset there, while the video resumes immediately on this IDR.
        self.audio.live_reentry_pending = true;
        Self::restamp_emit(&mut self.video, idr)
    }
}

impl<L: PacketSource, C: MonotonicClock> PacketSource for GuardedPacketSource<L, C> {
    /// Pull the next packet to mux: the next copied input packet (LIVE) or the
    /// next pre-baked slate packet (SLATE), re-stamped monotonic. Never `None`
    /// while clocked (the seam always has a valid AU to emit) — it returns
    /// `Ok(None)` only on a clean LIVE end-of-program with no slate fallback
    /// requested, which a live guarded passthrough never reaches.
    fn next_packet(&mut self) -> Result<Option<EncodedPacket>> {
        // Self-drive the decision from the injected clock (in addition to any
        // external clock-thread driver). Wait-free + fail-safe.
        let now = self.clock.now_nanos();
        self.evaluate(now);

        match self.emitting {
            GuardMode::Live => {
                // The decision flag may demand a splice (input lost). Honour it
                // before pulling — the pull might block on a dead demuxer were it
                // not for the GP-0 interrupt, and the watchdog already knows.
                // BUT only once the input has actually started: before the first
                // recorded packet the watchdog's `should_splice` is the fail-safe
                // "splice if no byte arrives" default, which must not pre-empt the
                // very first read of a healthy startup (see `started`).
                if self.started && self.decision_is_splice() {
                    self.enter_slate();
                    return Ok(Some(self.next_slate()));
                }
                // Copy the next live input packet. A drained live source is hard
                // death: mark the watchdog dead and splice (the degenerate clock
                // keeps emitting forever — never `Ok(None)` on loss).
                let Some(packet) = self.live.next_packet()? else {
                    self.video_liveness.mark_eof();
                    if let Some(audio) = &self.audio_liveness {
                        audio.mark_eof();
                    }
                    self.enter_slate();
                    return Ok(Some(self.next_slate()));
                };
                self.record_live(now, &packet);
                self.started = true;
                Ok(Some(self.emit_live(&packet)))
            }
            GuardMode::Slate => {
                // Attempt recovery: pull the live source (non-blocking). The
                // input returning is the recovery signal; re-entry is is_idr-gated
                // (NOT is_key), so discard every input packet until a TRUE video
                // IDR, then re-anchor and resume copy. Until then, keep emitting
                // slate (finishing the in-flight loop).
                if let Some(packet) = self.live.next_packet()? {
                    self.record_live(now, &packet);
                    self.started = true;
                    if self.is_video_idr(&packet) {
                        // Finish nothing extra — re-anchor on this true RAP and
                        // resume copy (DTS strictly increasing across the seam).
                        return Ok(Some(self.recover_to_live(&packet)));
                    }
                    // Non-IDR (incl. an is_key recovery-point I-frame) or audio:
                    // discard and keep emitting slate.
                }
                Ok(Some(self.next_slate()))
            }
        }
    }
}
