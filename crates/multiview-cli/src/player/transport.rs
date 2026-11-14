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

/// What a [`MediaPlayer::rebind`] lands on after a clip commit.
///
/// `Cue` is the API `Load` contract ("does not start playout"); `Resume` is the
/// internal refresh caller: a playing or vamping player keeps doing so on the
/// new clip (design 2.2, decision 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LoadThen {
    /// Park on the new in-point, `Cued`.
    Cue,
    /// Continue `Playing` / `Vamping` on the new clip; anything else lands `Cued`.
    Resume,
}

/// What the ingest thread is doing this frame period (design 2.3).
///
/// Every mode drains the mailbox and polls the staged slot once per frame
/// period; `stop` is the only exit. No mode returns an error, so the thread
/// cannot die on a decode or read failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PlayerMode {
    /// No clip has ever committed: the NA heartbeat. `LIVE` by construction.
    NoClip,
    /// Reading and decoding the current clip.
    Decoding,
    /// Republishing the last good frame each period. `LIVE` by construction.
    Held,
    /// `auto_off`: nothing is published; the store ages by design.
    Released,
}

impl PlayerMode {
    /// Every mode drains the mailbox (a verb is never stranded).
    #[must_use]
    pub const fn drains_mailbox(self) -> bool {
        true
    }

    /// Every mode polls the staged slot, so a load can commit from any mode.
    #[must_use]
    pub const fn polls_staged_slot(self) -> bool {
        true
    }

    /// Whether the mode publishes a frame each period (`Released` does not).
    #[must_use]
    pub const fn publishes(self) -> bool {
        !matches!(self, Self::Released)
    }

    /// Whether the tile is `LIVE` by construction (no IO on the thread).
    #[must_use]
    pub const fn live_by_construction(self) -> bool {
        matches!(self, Self::NoClip | Self::Held)
    }

    /// The mode after a decode or read error: hold the last good frame, or
    /// fall back to the NA heartbeat when no frame was ever published. Never
    /// an exit.
    #[must_use]
    pub const fn after_error(has_frame: bool) -> Self {
        if has_frame {
            Self::Held
        } else {
            Self::NoClip
        }
    }

    /// The mode after a [`PlayerAction`] from a decoding player.
    #[must_use]
    pub const fn after_action(self, action: PlayerAction) -> Self {
        match action {
            PlayerAction::Ended => Self::Released,
            PlayerAction::Hold { .. } => match self {
                Self::Decoding => Self::Held,
                other => other,
            },
            PlayerAction::Publish { .. } | PlayerAction::SeekFlushTo { .. } => self,
        }
    }

    /// The mode after a commit: a freshly committed clip decodes again.
    #[must_use]
    pub const fn after_commit() -> Self {
        Self::Decoding
    }
}

/// The pure media-player transport state machine.
///
/// Construct with [`MediaPlayer::new`] (a clip bound) or [`MediaPlayer::idle`]
/// (no clip yet); drive transport with the verb methods; feed each decoded
/// source frame to [`MediaPlayer::on_decoded`] and perform the returned
/// [`PlayerAction`]. Heartbeat ticks go through [`MediaPlayer::on_heartbeat`]
/// / [`MediaPlayer::on_heartbeat_n`].
///
/// **Stamps are output tick indices** (design 4.1): the cursor is an integer
/// tick `k`, and the stamp is `MediaTime::from_tick(k, cadence)`, so a step of
/// `n` ticks advances the stamp by exactly `from_tick(k+n) - from_tick(k)` with
/// no per-step rounding. One private path (`advance`) owns the cursor.
#[derive(Debug, Clone)]
pub struct MediaPlayer {
    state: MediaPlayerState,
    geometry: PlayoutGeometry,
    eof_policy: EofPolicy,
    /// The output tick index the current anchor epoch starts at.
    anchor_tick: i64,
    /// Ticks consumed since the current anchor.
    emitted: u64,
    /// The tick most recently stamped. `None` until the first stamp. A re-anchor
    /// never rewinds it: every stamp is at least `last_tick + n`.
    last_tick: Option<i64>,
    /// How many times the cursor jumped forward because the thread fell behind.
    stamp_skips: u64,
}

impl MediaPlayer {
    /// Construct a player parked at the in-point (`Cued`), with the given
    /// geometry and EOF policy, anchored at output media time `anchor`.
    #[must_use]
    pub fn new(geometry: PlayoutGeometry, eof_policy: EofPolicy, anchor: MediaTime) -> Self {
        Self {
            state: MediaPlayerState::Cued,
            anchor_tick: anchor.to_tick(geometry.cadence()),
            geometry,
            eof_policy,
            emitted: 0,
            last_tick: None,
            stamp_skips: 0,
        }
    }

    /// Construct a player with no clip: state `Idle`, a one-frame sentinel
    /// geometry (a [`PlayoutGeometry`] cannot be empty). Its stamps run from
    /// tick 0 until [`MediaPlayer::start_at`] seeds the output-anchored tick.
    ///
    /// # Errors
    ///
    /// [`PlayoutGeometryError::Cadence`] if `cadence` is not a positive rational.
    pub fn idle(cadence: Rational, eof_policy: EofPolicy) -> Result<Self, PlayoutGeometryError> {
        let geometry = PlayoutGeometry::new(0, 1, 0, 1, cadence)?;
        let mut player = Self::new(geometry, eof_policy, MediaTime::ZERO);
        player.state = MediaPlayerState::Idle;
        Ok(player)
    }

    /// Anchor the first publish at output tick `tick` (the origin anchor,
    /// design 4.1). Call before the first stamp; it never lowers the monotone
    /// floor once a frame has been stamped.
    pub const fn start_at(&mut self, tick: i64) {
        self.anchor_tick = tick;
        self.emitted = 0;
    }

    /// Commit a new clip: swap the geometry, land per `then`, clear any armed
    /// exit, and re-anchor at `max(commit_tick, last_tick + 1)`. Keeps
    /// `last_tick`, so there is no gap and no rewind. Returns the tick the next
    /// stamp is anchored at (the audio `start_anchor` uses the same tick).
    pub fn rebind(&mut self, geometry: PlayoutGeometry, then: LoadThen, commit_tick: i64) -> i64 {
        self.state = match (then, self.state) {
            (LoadThen::Resume, MediaPlayerState::Playing) => MediaPlayerState::Playing,
            (LoadThen::Resume, MediaPlayerState::Vamping { .. }) => {
                MediaPlayerState::Vamping { exit_armed: false }
            }
            _ => MediaPlayerState::Cued,
        };
        self.geometry = geometry;
        let anchor = match self.last_tick {
            Some(last) => commit_tick.max(last.saturating_add(1)),
            None => commit_tick,
        };
        self.anchor_tick = anchor;
        self.emitted = 0;
        anchor
    }

    /// If the next stamp would be earlier than `min_tick` (the thread fell
    /// behind output time), jump the cursor forward to `min_tick` and count it.
    /// A `min_tick` at or behind the cursor is a no-op: stamps never go back.
    pub fn catch_up(&mut self, min_tick: i64) {
        if self.next_tick(1) < min_tick {
            self.anchor_tick = min_tick;
            self.emitted = 0;
            self.stamp_skips = self.stamp_skips.saturating_add(1);
        }
    }

    /// The tick most recently stamped, if any.
    #[must_use]
    pub const fn last_tick(&self) -> Option<i64> {
        self.last_tick
    }

    /// The tick the next single-step stamp would take.
    #[must_use]
    pub fn next_stamp_tick(&self) -> i64 {
        self.next_tick(1)
    }

    /// How many forward jumps [`MediaPlayer::catch_up`] has made.
    #[must_use]
    pub const fn stamp_skips(&self) -> u64 {
        self.stamp_skips
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

    /// As [`MediaPlayer::on_heartbeat`], advancing `n` ticks at once (the NA
    /// cadence of design 2.1): the stamp moves by exactly
    /// `from_tick(k + n) - from_tick(k)`. `n == 0` is treated as 1.
    #[must_use]
    pub fn on_heartbeat_n(&mut self, n: u32) -> PlayerAction {
        match self.state {
            MediaPlayerState::Ended => PlayerAction::Ended,
            _ => PlayerAction::Hold {
                at: self.advance(n),
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
        self.anchor_tick = anchor.to_tick(self.geometry.cadence());
        self.emitted = 0;
    }

    /// The tick an `n`-step advance would stamp: the first stamp of an epoch
    /// lands on the anchor; later ones on `max(anchor + emitted + n - 1,
    /// last + n)`, so a re-anchor can never rewind or stall the timeline.
    fn next_tick(&self, n: u32) -> i64 {
        let step = i64::from(n.max(1));
        let emitted = i64::try_from(self.emitted).unwrap_or(i64::MAX);
        let anchored = self
            .anchor_tick
            .saturating_add(emitted)
            .saturating_add(step - 1);
        match self.last_tick {
            Some(last) => anchored.max(last.saturating_add(step)),
            None => self.anchor_tick.saturating_add(emitted),
        }
    }

    /// The single path that owns the stamp (L1): decoded, held and heartbeat
    /// stamps all come from here. Advances `n` ticks and returns the exact
    /// `from_tick` of the new cursor. Integer arithmetic only, no allocation.
    fn advance(&mut self, n: u32) -> MediaTime {
        let tick = self.next_tick(n);
        let consumed = if self.last_tick.is_some() {
            u64::from(n.max(1))
        } else {
            1
        };
        self.emitted = self.emitted.saturating_add(consumed);
        self.last_tick = Some(tick);
        MediaTime::from_tick(tick, self.geometry.cadence())
    }

    /// Publish the current frame at the next (strictly-monotone, output-anchored)
    /// stamp and advance the emitted-frame cursor.
    fn publish(&mut self) -> PlayerAction {
        PlayerAction::Publish {
            at: self.advance(1),
        }
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
        self.advance(1)
    }
}
