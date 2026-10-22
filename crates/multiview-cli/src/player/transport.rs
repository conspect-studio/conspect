//! The pure media-player transport state machine.
//!
//! See the [module docs](super) for the why. This file is the deterministic
//! core: [`MediaPlayer`] holds the [`MediaPlayerState`], the
//! [`PlayoutGeometry`] (integer-frame in/out + vamp window), the EOF policy,
//! and the output-anchored stamping cursor. The executor feeds it the source
//! frame index of each freshly decoded frame via [`MediaPlayer::on_decoded`]
//! and performs the returned [`PlayerAction`]; transport verbs
//! ([`MediaPlayer::play`], [`MediaPlayer::pause`], [`MediaPlayer::stop`],
//! [`MediaPlayer::arm_exit`], …) mutate the state between frames.

use multiview_core::time::{rescale, MediaTime, Rational};

/// The end-of-asset behaviour for a player channel
/// ([ADR-0057](../../../../docs/decisions/ADR-0057.md) Decision 4,
/// [media-playout §7.4](../../../../docs/research/media-playout.md)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum EofPolicy {
    /// Freeze on the final frame and hold it forever (the as-built
    /// `NoSignalPolicy::HoldForever` behaviour; the default).
    #[default]
    HoldLastFrame,
    /// Seek back to the in-point in place and keep decoding — loops forever.
    Loop,
    /// Publish one terminal frame (opaque black for opaque assets, fully
    /// transparent for alpha assets) and hold it.
    Black,
    /// Publish the terminal frame, report `Ended`, and release the decoder; the
    /// switcher state machine applies the bus/keyer consequence.
    AutoOff,
}

/// Why a [`PlayoutGeometry`] was rejected at construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PlayoutGeometryError {
    /// `in_point > vamp_in`, `vamp_out > out_point`, or the window is otherwise
    /// not nested as `in_point ≤ vamp_in < vamp_out ≤ out_point`.
    #[error(
        "playout window must satisfy in_point ≤ vamp_in < vamp_out ≤ out_point \
         (got in={in_point} vamp_in={vamp_in} vamp_out={vamp_out} out={out_point})"
    )]
    Window {
        /// The clip in-point in frames.
        in_point: u64,
        /// The vamp-segment start in frames.
        vamp_in: u64,
        /// The vamp-segment end (exclusive) in frames.
        vamp_out: u64,
        /// The clip out-point (exclusive) in frames.
        out_point: u64,
    },
    /// The cadence is not a positive rational, so frame-period math is undefined.
    #[error("cadence must be a positive rational (got {num}/{den})")]
    Cadence {
        /// The numerator that was supplied.
        num: i64,
        /// The denominator that was supplied.
        den: i64,
    },
}

/// The integer-frame playout geometry of a loaded asset on its player channel.
///
/// All four points are frame indices at `cadence`. The clip plays
/// `[in_point, out_point)`; the **vamp segment** `[vamp_in, vamp_out)` is the
/// sub-range that loops while vamping ([ADR-0097](../../../../docs/decisions/ADR-0097.md)).
/// Defaulting `vamp_in = in_point` and `vamp_out = out_point` makes the whole
/// clip the vamp loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayoutGeometry {
    in_point: u64,
    out_point: u64,
    vamp_in: u64,
    vamp_out: u64,
    cadence: Rational,
    frame_period_ns: i64,
}

impl PlayoutGeometry {
    /// Construct and validate a geometry. Enforces
    /// `in_point ≤ vamp_in < vamp_out ≤ out_point` (strict on the vamp pair, so
    /// a vamp segment is at least one frame) and a positive rational cadence.
    ///
    /// # Errors
    ///
    /// [`PlayoutGeometryError::Window`] if the four points are not nested;
    /// [`PlayoutGeometryError::Cadence`] if `cadence` is not a positive rational.
    pub fn new(
        in_point: u64,
        out_point: u64,
        vamp_in: u64,
        vamp_out: u64,
        cadence: Rational,
    ) -> Result<Self, PlayoutGeometryError> {
        if !cadence.is_valid() || cadence.is_zero() || cadence.num <= 0 {
            return Err(PlayoutGeometryError::Cadence {
                num: cadence.num,
                den: cadence.den,
            });
        }
        if !(in_point <= vamp_in && vamp_in < vamp_out && vamp_out <= out_point) {
            return Err(PlayoutGeometryError::Window {
                in_point,
                vamp_in,
                vamp_out,
                out_point,
            });
        }
        // Frame period in ns = (cadence.den / cadence.num) seconds in ns.
        let period_tb = Rational::new(cadence.den, cadence.num);
        let ns_tb = Rational::new(1, 1_000_000_000);
        let frame_period_ns = rescale(1, period_tb, ns_tb).max(1);
        Ok(Self {
            in_point,
            out_point,
            vamp_in,
            vamp_out,
            cadence,
            frame_period_ns,
        })
    }

    /// The clip in-point (frames).
    #[must_use]
    pub const fn in_point(&self) -> u64 {
        self.in_point
    }

    /// The clip out-point, exclusive (frames).
    #[must_use]
    pub const fn out_point(&self) -> u64 {
        self.out_point
    }

    /// The vamp-segment start (frames).
    #[must_use]
    pub const fn vamp_in(&self) -> u64 {
        self.vamp_in
    }

    /// The vamp-segment end, exclusive (frames).
    #[must_use]
    pub const fn vamp_out(&self) -> u64 {
        self.vamp_out
    }

    /// The declared cadence.
    #[must_use]
    pub const fn cadence(&self) -> Rational {
        self.cadence
    }

    /// One frame period in nanoseconds (exact rational, ≥ 1).
    #[must_use]
    pub const fn frame_period_ns(&self) -> i64 {
        self.frame_period_ns
    }

    /// The number of frames in the vamp segment (`vamp_out − vamp_in`).
    #[must_use]
    pub const fn vamp_len(&self) -> u64 {
        self.vamp_out - self.vamp_in
    }

    /// The number of frames in the full trimmed clip (`out_point − in_point`).
    #[must_use]
    pub const fn trimmed_len(&self) -> u64 {
        self.out_point - self.in_point
    }
}

/// The transport state of a media-player channel.
///
/// Extends the [media-playout §7.1](../../../../docs/research/media-playout.md)
/// machine (`Idle → Loading → Cued → Playing`, `Paused`, EOF terminals) with
/// the [ADR-0097](../../../../docs/decisions/ADR-0097.md) `Vamping { exit_armed }`
/// state, distinct from `Playing` under an EOF `loop` policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MediaPlayerState {
    /// No asset bound.
    Idle,
    /// An asset is bound and the container is opening.
    Loading,
    /// Parked on the in-point (or a cued frame); the first frame is published
    /// so PVW shows it. Reports cued once the store is primed.
    Cued,
    /// Playing forward. Under an EOF `loop` policy this loops forever.
    Playing,
    /// Paused: the cursor is held, the picture frozen (heartbeat-republished).
    Paused,
    /// Vamping: looping the vamp segment to fill until a cue. `exit_armed`
    /// commits a clean exit at the next vamp boundary.
    Vamping {
        /// Whether an exit has been armed (fires at the next vamp boundary).
        exit_armed: bool,
    },
    /// EOF held: frozen on the final real frame (`hold_last_frame`).
    Holding,
    /// EOF black: holding the terminal black (opaque) / transparent (alpha)
    /// frame (`black`), so a keyed fill cleanly vanishes.
    Black,
    /// EOF ended: the terminal frame was published and the channel reports
    /// `Ended` (`auto_off`); the switcher applies the bus/keyer consequence.
    Ended,
}

/// What the executor must do with the frame the decoder just produced.
///
/// Returned by [`MediaPlayer::on_decoded`]. The executor owns the libav side:
/// it performs the publish (with the given output-anchored stamp), or the
/// seek + decoder flush + discontinuity mark at a wrap, or holds the last-good
/// frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PlayerAction {
    /// Publish the just-decoded frame, stamped at `at` (output-anchored).
    Publish {
        /// The output-anchored media-time stamp for this frame.
        at: MediaTime,
    },
    /// The lap boundary was reached: seek the open container to `frame`, flush
    /// the decoder, mark the next normalized frame as a discontinuity, and keep
    /// decoding from there. The just-decoded frame (at/after the boundary) is
    /// **discarded**, not published.
    SeekFlushTo {
        /// The source frame index to seek to (the vamp/clip in-point).
        frame: u64,
    },
    /// Do not advance: republish the held last-good frame (paused / still /
    /// EOF-held) at `at` so the tile reads LIVE rather than ageing.
    Hold {
        /// The output-anchored media-time stamp for the heartbeat republish.
        at: MediaTime,
    },
    /// The asset ended under a non-looping policy: report `Ended` and release.
    Ended,
}

/// The pure media-player transport state machine.
///
/// Construct with [`MediaPlayer::new`]; drive transport with the verb methods;
/// feed each decoded source frame to [`MediaPlayer::on_decoded`] and perform the
/// returned [`PlayerAction`]. Heartbeat ticks (when no new frame decoded, e.g.
/// paused or EOF-held) go through [`MediaPlayer::on_heartbeat`].
#[derive(Debug, Clone)]
pub struct MediaPlayer {
    state: MediaPlayerState,
    geometry: PlayoutGeometry,
    eof_policy: EofPolicy,
    /// The output media time of the start tick — the anchor for stamping.
    anchor: MediaTime,
    /// Count of frames published since the current `anchor` (monotonic across
    /// laps within an anchor epoch); the `k` in
    /// `publish_at(k) = anchor + k × frame_period`.
    emitted: u64,
    /// The output media time most recently emitted (for the heartbeat cadence
    /// and the **strict-monotonic guarantee**: a re-anchor — play/vamp after a
    /// frame was already emitted — must never rewind the published timeline, so
    /// every stamp is floored at `last_at + frame_period`).
    last_at: MediaTime,
    /// Whether any frame has been emitted yet (so the very first frame stamps
    /// exactly at `anchor`, while every later frame respects the monotone floor).
    emitted_any: bool,
}

impl MediaPlayer {
    /// Construct a player parked at the in-point (`Cued`), with the given
    /// geometry and EOF policy, anchored at output media time `anchor`.
    #[must_use]
    pub fn new(geometry: PlayoutGeometry, eof_policy: EofPolicy, anchor: MediaTime) -> Self {
        Self {
            state: MediaPlayerState::Cued,
            geometry,
            eof_policy,
            anchor,
            emitted: 0,
            last_at: anchor,
            emitted_any: false,
        }
    }

    /// The current transport state.
    #[must_use]
    pub const fn state(&self) -> MediaPlayerState {
        self.state
    }

    /// The geometry.
    #[must_use]
    pub const fn geometry(&self) -> &PlayoutGeometry {
        &self.geometry
    }

    /// `true` while in a state that publishes fresh decoded frames.
    #[must_use]
    pub const fn is_playing_state(&self) -> bool {
        matches!(
            self.state,
            MediaPlayerState::Playing | MediaPlayerState::Vamping { .. }
        )
    }

    /// `true` once an exit has been armed on a vamping channel.
    #[must_use]
    pub const fn exit_armed(&self) -> bool {
        matches!(self.state, MediaPlayerState::Vamping { exit_armed: true })
    }

    // ---- transport verbs (mutate state between frames) ------------------

    /// Begin (or resume) plain forward playback, re-anchoring stamping at
    /// `anchor` (the output media time of the start tick).
    pub fn play(&mut self, anchor: MediaTime) {
        self.state = MediaPlayerState::Playing;
        self.reanchor(anchor);
    }

    /// Begin vamping the vamp segment (exit not yet armed), re-anchoring at
    /// `anchor`.
    pub fn vamp(&mut self, anchor: MediaTime) {
        self.state = MediaPlayerState::Vamping { exit_armed: false };
        self.reanchor(anchor);
    }

    /// Pause: hold the current frame.
    pub fn pause(&mut self) {
        self.state = MediaPlayerState::Paused;
    }

    /// Stop: re-cue to the in-point.
    pub fn stop(&mut self) {
        self.state = MediaPlayerState::Cued;
    }

    /// Arm the vamp exit: it fires at the next vamp boundary. No-op echo if not
    /// vamping or already armed.
    pub fn arm_exit(&mut self) {
        if let MediaPlayerState::Vamping { .. } = self.state {
            self.state = MediaPlayerState::Vamping { exit_armed: true };
        }
    }

    /// Take the vamp exit: arm it for the soonest boundary. Functionally `arm`;
    /// never forces a mid-lap cut.
    pub fn take_exit(&mut self) {
        self.arm_exit();
    }

    /// Cancel a pending vamp exit: keep looping. No-op if not armed.
    pub fn cancel_exit(&mut self) {
        if let MediaPlayerState::Vamping { exit_armed: true } = self.state {
            self.state = MediaPlayerState::Vamping { exit_armed: false };
        }
    }

    // ---- per-frame decisions -------------------------------------------

    /// Decide what to do with the frame the decoder just produced, identified
    /// by its `source_frame` index.
    ///
    /// This is the heart of the state machine: in a publishing state it returns
    /// an output-anchored [`PlayerAction::Publish`]; on reaching the lap
    /// boundary it returns [`PlayerAction::SeekFlushTo`] (loop / vamp wrap) or,
    /// with an armed exit, transitions out and returns the terminal action; in
    /// a held state it returns [`PlayerAction::Hold`].
    #[must_use]
    pub fn on_decoded(&mut self, source_frame: u64) -> PlayerAction {
        match self.state {
            MediaPlayerState::Playing => {
                // Plain playback runs to the clip out-point; under a `loop`
                // policy it wraps to the in-point, else it ends per the policy.
                if source_frame < self.geometry.out_point {
                    self.publish()
                } else if matches!(self.eof_policy, EofPolicy::Loop) {
                    Self::wrap_to(self.geometry.in_point)
                } else {
                    self.terminate()
                }
            }
            MediaPlayerState::Vamping { exit_armed } => {
                if source_frame < self.geometry.vamp_out {
                    self.publish()
                } else if exit_armed {
                    // The boundary reached with the exit armed: consume the
                    // latch exactly once and settle per the EOF policy. The
                    // exit overrides the loop — a vamp never loops once armed.
                    self.terminate()
                } else {
                    Self::wrap_to(self.geometry.vamp_in)
                }
            }
            // Cued: publish the cued frame so PVW shows it; do not advance laps.
            MediaPlayerState::Cued => self.publish(),
            // Held / paused: a late-arriving decoded frame is held, not shown.
            MediaPlayerState::Paused
            | MediaPlayerState::Holding
            | MediaPlayerState::Black
            | MediaPlayerState::Idle
            | MediaPlayerState::Loading => PlayerAction::Hold {
                at: self.advance_stamp(),
            },
            MediaPlayerState::Ended => PlayerAction::Ended,
        }
    }

    /// Produce the heartbeat action for a tick on which no new frame was
    /// decoded (paused, still, or EOF-held): a [`PlayerAction::Hold`] with an
    /// advancing stamp, or [`PlayerAction::Ended`] once ended.
    #[must_use]
    pub fn on_heartbeat(&mut self) -> PlayerAction {
        match self.state {
            MediaPlayerState::Ended => PlayerAction::Ended,
            _ => PlayerAction::Hold {
                at: self.advance_stamp(),
            },
        }
    }

    // ---- internals ------------------------------------------------------

    /// Re-anchor the output-anchored timeline at `anchor` (a fresh play/vamp).
    /// Resets the per-anchor frame counter but **never the `emitted_any` /
    /// `last_at` monotone floor**: if frames were already emitted, the next
    /// stamp continues strictly forward from `last_at` rather than rewinding to
    /// `anchor` (the published timeline is strictly monotone across any command
    /// sequence — the data-plane never-rewind guarantee, inv #1/#3).
    fn reanchor(&mut self, anchor: MediaTime) {
        self.anchor = anchor;
        self.emitted = 0;
    }

    /// The output-anchored stamp for the `emitted`-th frame of the current
    /// anchor epoch: `anchor + emitted × frame_period` (media-playout §7.2).
    /// Exact integer arithmetic — never float, never wall-clock.
    fn anchored_stamp(&self) -> MediaTime {
        // `emitted` is a frame counter; it fits i64 for any realistic run. Clamp
        // (never panic) rather than `as`-cast (lint policy / safety rule 3).
        let k = i64::try_from(self.emitted).unwrap_or(i64::MAX);
        let offset = k.saturating_mul(self.geometry.frame_period_ns);
        self.anchor.saturating_add(MediaTime::from_nanos(offset))
    }

    /// Compute the stamp for the next frame to emit and advance the cursors,
    /// **strictly monotone by construction**: the very first frame stamps at
    /// `anchor`; every later frame is `max(anchored_stamp, last_at +
    /// frame_period)` so a re-anchor can never rewind or stall the published
    /// timeline (the store's monotonic guard would otherwise clamp a rewind into
    /// a frozen tile).
    fn next_at(&mut self) -> MediaTime {
        let anchored = self.anchored_stamp();
        let at = if self.emitted_any {
            let floor = self
                .last_at
                .saturating_add(MediaTime::from_nanos(self.geometry.frame_period_ns));
            if anchored.as_nanos() > floor.as_nanos() {
                anchored
            } else {
                floor
            }
        } else {
            anchored
        };
        self.emitted = self.emitted.saturating_add(1);
        self.emitted_any = true;
        self.last_at = at;
        at
    }

    /// Publish the current frame at the next (strictly-monotone, output-anchored)
    /// stamp and advance the emitted-frame cursor.
    fn publish(&mut self) -> PlayerAction {
        PlayerAction::Publish { at: self.next_at() }
    }

    /// Wrap a loop: seek the open container back to `frame`, discard the
    /// boundary frame, and keep the emitted cursor advancing (monotonic stamps
    /// across the seam). The executor performs the container seek + the decoder
    /// `flush` (`avcodec_flush_buffers`) on this action. The emitted cursor is
    /// *not* reset — stamps stay monotonic across the lap seam (the executor's
    /// next decoded frame publishes at the next `k`). No `PtsNormalizer` is
    /// involved: the player synthesizes its own monotone timeline (ADR-0097 §5
    /// "Implementation refinement").
    const fn wrap_to(frame: u64) -> PlayerAction {
        PlayerAction::SeekFlushTo { frame }
    }

    /// Settle a non-looping end-of-segment per the EOF policy: `auto_off` ends
    /// (the switcher applies the bus/keyer consequence); the others hold the
    /// terminal frame.
    fn terminate(&mut self) -> PlayerAction {
        match self.eof_policy {
            EofPolicy::AutoOff => {
                self.state = MediaPlayerState::Ended;
                PlayerAction::Ended
            }
            EofPolicy::Black => {
                self.state = MediaPlayerState::Black;
                PlayerAction::Hold {
                    at: self.advance_stamp(),
                }
            }
            EofPolicy::HoldLastFrame | EofPolicy::Loop => {
                // (Loop reaches here only on the armed-exit path, where the exit
                // overrides the loop and we hold the last frame.)
                self.state = MediaPlayerState::Holding;
                PlayerAction::Hold {
                    at: self.advance_stamp(),
                }
            }
        }
    }

    /// The stamp for a heartbeat republish: advance via the same strictly-
    /// monotone path so the republished frame reads LIVE (strictly-increasing
    /// stamps), never aged or rewound.
    fn advance_stamp(&mut self) -> MediaTime {
        self.next_at()
    }
}
