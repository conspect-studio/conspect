//! The ordered degradation ladder with hysteresis (no flapping).
//!
//! Invariant #9: a closed control loop sheds load **cheapest-impact-first**,
//! tile-by-tile, *before* the composited program output is ever touched. The
//! ladder here is the fixed, documented ordering from
//! [efficiency §3.3](../../../docs/research/efficiency.md): earlier rungs
//! degrade low-priority tiles and shared resources; later rungs touch the
//! program everyone sees; the last rung sheds tiles entirely.
//!
//! Degradation is monotone in *level*: level 0 is full quality, higher levels
//! have applied progressively more (cumulative) load-shed actions. The
//! [`Hysteresis`] controller decides when to step down (apply the next rung)
//! and up (recover), with a dwell/cooldown so a noisy pressure signal cannot
//! oscillate the plan (the OBS "naive recovery oscillates" lesson).

use crate::error::{Error, Result};

/// One rung of the degradation ladder.
///
/// Ordered cheapest-impact-first; [`DegradationAction::LADDER`] lists them in
/// the order they are applied. Each action's position in the ladder is its
/// rung index, exposed via [`DegradationAction::rung`] (we resolve the index by
/// matching rather than casting the discriminant, to stay within the no-`as`
/// guardrail).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum DegradationAction {
    // --- Preview rungs (topmost, cheapest to shed) -----------------------
    // Per [preview-subsystem §8](../../../docs/research/preview-subsystem.md)
    // and ADR-P001, preview is the FIRST thing shed under sustained overload —
    // ALL preview rungs are applied before any tile/program lever moves
    // (invariant #9: cheapest-impact-first; preview loses every resource fight
    // against the program). These rungs touch only the best-effort, isolated
    // preview side-channel (invariant #10), never the protected output path.
    /// 1. Shed click-to-focus WHEP sessions: suspend existing focus encodes and
    ///    refuse new focus (the `503 fallback: ws-jpeg` shape). The single most
    ///    expensive preview transport, shed first.
    ShedFocusWhep,
    /// 2. Drop the preview grid/thumbnail fps (fewer JPEG/MJPEG ticks).
    DropPreviewGridFps,
    /// 3. Drop the preview grid/thumbnail resolution.
    DropPreviewGridRes,
    /// 4. Stop off-air cue/pre-warm decoders (ADR-P004 Tier B workers).
    DropOffAirCueDecoders,
    /// 5. Suspend the preview subsystem entirely (no taps, no encodes).
    SuspendPreviewEntirely,

    // --- Tile / shared-resource rungs (pre-program) ----------------------
    /// 6. Drop per-tile decode resolution (lowest-priority tiles first).
    DropTileResolution,
    /// 7. Drop per-tile fps (`skip_frame` noref -> nokey).
    DropTileFps,
    /// 8. Switch to a simpler/faster scaler.
    SimplerScaler,

    // --- Program-affecting rungs (the output everyone sees) --------------
    /// 9. Step the output encoder preset faster (cheap, large swing).
    FasterEncoderPreset,
    /// 10. Lower output bitrate.
    LowerOutputBitrate,
    /// 11. Lower output fps.
    LowerOutputFps,
    /// 12. Lower output resolution.
    LowerOutputResolution,
    /// 13. Shed/freeze lowest-priority tiles entirely (`AVDISCARD_ALL`).
    ShedTiles,
}

impl DegradationAction {
    /// The ladder in cheapest-impact-first order (the order actions are
    /// applied as pressure rises; recovery reverses it). The five preview rungs
    /// lead, then the tile/shared-resource rungs, then the program-affecting
    /// rungs — so preview is fully shed before the program is ever touched.
    pub const LADDER: [DegradationAction; 13] = [
        DegradationAction::ShedFocusWhep,
        DegradationAction::DropPreviewGridFps,
        DegradationAction::DropPreviewGridRes,
        DegradationAction::DropOffAirCueDecoders,
        DegradationAction::SuspendPreviewEntirely,
        DegradationAction::DropTileResolution,
        DegradationAction::DropTileFps,
        DegradationAction::SimplerScaler,
        DegradationAction::FasterEncoderPreset,
        DegradationAction::LowerOutputBitrate,
        DegradationAction::LowerOutputFps,
        DegradationAction::LowerOutputResolution,
        DegradationAction::ShedTiles,
    ];

    /// This action's rung index (`0`-based) within [`Self::LADDER`].
    #[must_use]
    pub const fn rung(self) -> usize {
        match self {
            DegradationAction::ShedFocusWhep => 0,
            DegradationAction::DropPreviewGridFps => 1,
            DegradationAction::DropPreviewGridRes => 2,
            DegradationAction::DropOffAirCueDecoders => 3,
            DegradationAction::SuspendPreviewEntirely => 4,
            DegradationAction::DropTileResolution => 5,
            DegradationAction::DropTileFps => 6,
            DegradationAction::SimplerScaler => 7,
            DegradationAction::FasterEncoderPreset => 8,
            DegradationAction::LowerOutputBitrate => 9,
            DegradationAction::LowerOutputFps => 10,
            DegradationAction::LowerOutputResolution => 11,
            DegradationAction::ShedTiles => 12,
        }
    }

    /// Whether this rung sheds the best-effort, isolated **preview**
    /// side-channel (the five topmost rungs), as opposed to a tile or program
    /// lever. The degradation driver maps these onto
    /// [`crate::degradation`]'s preview hooks (e.g. `FocusGate::suspend`) so a
    /// preview rung is shed BEFORE any tile/program rung (ADR-P001, invariant
    /// #9). A preview rung never affects the program ([`Self::affects_program`]
    /// is always `false` for it).
    #[must_use]
    pub const fn affects_preview(self) -> bool {
        self.rung() < DegradationAction::first_non_preview_level()
    }

    /// Whether this rung degrades the **program output** everyone sees (the
    /// `FasterEncoderPreset` rung onward), as opposed to the preview
    /// side-channel or individual low-priority tiles / shared resources.
    ///
    /// The planner can surface this so an operator (or a policy) treats
    /// crossing into program-affecting territory differently.
    #[must_use]
    pub const fn affects_program(self) -> bool {
        self.rung() >= DegradationAction::first_program_level()
    }

    /// The level at which the **first non-preview** (tile) rung begins: every
    /// rung strictly below this is a preview rung. Equals the number of preview
    /// rungs, so all of them are shed before any tile/program lever moves.
    #[must_use]
    pub const fn first_non_preview_level() -> usize {
        DegradationAction::DropTileResolution.rung()
    }

    /// The level at which the **first program-affecting** rung begins: every
    /// rung strictly below this is preview-or-tile (pre-program). This is the
    /// boundary invariant #9 protects — nothing the program shows is touched
    /// until pressure has climbed past every cheaper rung.
    #[must_use]
    pub const fn first_program_level() -> usize {
        DegradationAction::FasterEncoderPreset.rung()
    }
}

/// The maximum degradation level: full quality (`0`) plus every ladder rung.
pub const MAX_LEVEL: usize = DegradationAction::LADDER.len();

/// The set of degradation actions applied at a given `level`.
///
/// At level `n`, the first `n` rungs of [`DegradationAction::LADDER`] are
/// active (cumulative). `level == 0` is full quality; `level == MAX_LEVEL` has
/// the whole ladder applied.
#[must_use]
pub fn actions_at_level(level: usize) -> &'static [DegradationAction] {
    let clamped = level.min(MAX_LEVEL);
    DegradationAction::LADDER
        .get(..clamped)
        .unwrap_or(&DegradationAction::LADDER)
}

/// Direction the controller wants to move the degradation level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LadderMove {
    /// Apply the next rung (shed more load): level increases by one.
    Down,
    /// Recover one rung (restore quality): level decreases by one.
    Up,
    /// Hold the current level (inside the hysteresis band or in cooldown).
    Hold,
}

/// Tuning for the hysteresis controller.
///
/// `low`/`high` form the hysteresis band on a normalized `0.0..=1.0` pressure
/// signal (e.g. `0.7`/`0.9`): below `low` the controller wants to recover,
/// above `high` it wants to shed, in between it holds. `recover_cooldown_ticks`
/// is the dwell the controller waits **after any change** before it will move
/// *up* (recover) — the asymmetric cooldown that stops the "naive recovery
/// oscillates" flapping. Stepping *down* under sustained high pressure is not
/// cooled down (shedding load is the safety net and must be prompt).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HysteresisConfig {
    /// Lower threshold of the hysteresis band (`0.0..=1.0`); below it, recover.
    pub low: f64,
    /// Upper threshold of the hysteresis band (`0.0..=1.0`); above it, shed.
    pub high: f64,
    /// Ticks to dwell after any change before a recovery (up) move is allowed.
    pub recover_cooldown_ticks: u32,
}

impl HysteresisConfig {
    /// A sensible default: shed above `0.9`, recover below `0.7`, dwell `5`
    /// ticks before recovering (at a 1-2 s control tick, ~5-10 s — matching the
    /// brief's cooldown guidance).
    #[must_use]
    pub const fn new_default() -> Self {
        Self {
            low: 0.7,
            high: 0.9,
            recover_cooldown_ticks: 5,
        }
    }

    /// Construct and validate a config.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidCapability`] if the thresholds are not finite,
    /// not within `0.0..=1.0`, or `low >= high` (a non-positive-width band
    /// would defeat the anti-flap guarantee).
    pub fn try_new(low: f64, high: f64, recover_cooldown_ticks: u32) -> Result<Self> {
        let config = Self {
            low,
            high,
            recover_cooldown_ticks,
        };
        config.validate()?;
        Ok(config)
    }

    /// Validate the threshold band.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidCapability`] if thresholds are non-finite,
    /// outside `0.0..=1.0`, or `low >= high`.
    pub fn validate(&self) -> Result<()> {
        if !self.low.is_finite() || !self.high.is_finite() {
            return Err(Error::InvalidCapability(
                "hysteresis thresholds must be finite",
            ));
        }
        if !(0.0..=1.0).contains(&self.low) || !(0.0..=1.0).contains(&self.high) {
            return Err(Error::InvalidCapability(
                "hysteresis thresholds must lie within 0.0..=1.0",
            ));
        }
        if self.low >= self.high {
            return Err(Error::InvalidCapability(
                "hysteresis requires low < high (a positive-width band)",
            ));
        }
        Ok(())
    }
}

impl Default for HysteresisConfig {
    fn default() -> Self {
        Self::new_default()
    }
}

/// The hysteresis controller: turns a stream of normalized pressure readings
/// into degradation-level moves that never flap.
///
/// Drive it once per control tick with [`Hysteresis::observe`]. It owns the
/// current degradation level and a cooldown counter; the asymmetric rule —
/// shed promptly, recover only after a dwell — is the anti-flap guarantee
/// (invariant #9's hysteresis requirement).
#[derive(Debug, Clone)]
pub struct Hysteresis {
    config: HysteresisConfig,
    level: usize,
    /// Ticks remaining before a recovery (up) move is permitted.
    cooldown_remaining: u32,
}

impl Hysteresis {
    /// Construct a controller at full quality (level 0).
    #[must_use]
    pub fn new(config: HysteresisConfig) -> Self {
        Self {
            config,
            level: 0,
            cooldown_remaining: 0,
        }
    }

    /// The current degradation level (`0..=MAX_LEVEL`).
    #[must_use]
    pub fn level(&self) -> usize {
        self.level
    }

    /// The set of actions currently applied at the controller's level.
    #[must_use]
    pub fn active_actions(&self) -> &'static [DegradationAction] {
        actions_at_level(self.level)
    }

    /// Ticks remaining before a recovery move is permitted.
    #[must_use]
    pub fn cooldown_remaining(&self) -> u32 {
        self.cooldown_remaining
    }

    /// Observe one normalized `pressure` reading (`0.0` = idle, `1.0` =
    /// saturated) and update the level, returning the move that was applied.
    ///
    /// Rules:
    /// - `pressure > high` and not already at `MAX_LEVEL` -> [`LadderMove::Down`]
    ///   (apply the next rung immediately; resets the recovery cooldown).
    /// - `pressure < low`, level `> 0`, and the cooldown has elapsed ->
    ///   [`LadderMove::Up`] (recover one rung; resets the cooldown).
    /// - otherwise -> [`LadderMove::Hold`] (inside the band, in cooldown, or at
    ///   a level bound). Each held tick decrements the cooldown.
    ///
    /// A non-finite `pressure` is treated as `Hold` (a bad sensor reading must
    /// never move the plan).
    pub fn observe(&mut self, pressure: f64) -> LadderMove {
        if !pressure.is_finite() {
            self.tick_cooldown();
            return LadderMove::Hold;
        }

        if pressure > self.config.high && self.level < MAX_LEVEL {
            self.level += 1;
            // Stepping down (more shedding) restarts the dwell so we do not
            // immediately bounce back up on the next sub-`low` reading.
            self.cooldown_remaining = self.config.recover_cooldown_ticks;
            return LadderMove::Down;
        }

        if pressure < self.config.low && self.level > 0 {
            if self.cooldown_remaining > 0 {
                self.tick_cooldown();
                return LadderMove::Hold;
            }
            self.level -= 1;
            self.cooldown_remaining = self.config.recover_cooldown_ticks;
            return LadderMove::Up;
        }

        // Inside the band, or at a bound: hold and let any cooldown elapse.
        self.tick_cooldown();
        LadderMove::Hold
    }

    /// Decrement the recovery cooldown by one tick (saturating at zero).
    fn tick_cooldown(&mut self) {
        self.cooldown_remaining = self.cooldown_remaining.saturating_sub(1);
    }
}
