//! Pure, frame-boundary M/E switcher value machine (ADR-0054/0055).
//!
//! The caller drains intents at the tick boundary, passes the *same* output
//! [`Tick`] used for composition, and applies the returned projection. No time
//! source, I/O, shared lock or asynchronous work is involved here.
use multiview_core::switcher::{ArmedSet, FrameCount, Progress, SourceRef, TransitionDescriptor};
use thiserror::Error;

use crate::clock::Tick;

/// Invalid M/E configuration or take request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum SwitcherError {
    /// There is no element selected for the next transition.
    #[error("at least one background or key element must be armed")]
    NothingArmed,
    /// An armed key does not exist on this M/E or is selected twice.
    #[error("armed keys must be distinct and within the M/E's 1-based key indices")]
    InvalidKey,
    /// The descriptor has an invalid timing or switch point.
    #[error("invalid transition descriptor")]
    InvalidTransition,
}

/// One immutable frame-boundary projection for the render-plan resolver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwitcherProjection {
    /// Exact progress, including the terminal `1` projection on completion.
    pub progress: Progress,
    /// Whether this command started a previously idle transition.
    pub started: bool,
    /// True exactly once per completed take, regardless of verb.
    pub completed: bool,
}

impl SwitcherProjection {
    fn idle() -> Self {
        Self {
            progress: Progress::ZERO,
            started: false,
            completed: false,
        }
    }
}

#[derive(Debug, Clone)]
enum Flight {
    Auto { start: u64, rate: FrameCount },
    Manual { position: u16 },
}

/// Owns the program/preview scenes, USK on-air state and next-transition setup.
///
/// Instances are per M/E; `flip_flop` defaults to true independently for each.
/// All commands mutate only this value and return a projection to apply at the
/// calling frame boundary. In-flight descriptors and arming are frozen by
/// rejecting configuration changes until completion or abort.
#[derive(Debug, Clone)]
pub struct SwitcherState {
    program: SourceRef,
    preview: SourceRef,
    keys_on_air: Vec<bool>,
    armed: ArmedSet,
    transition: TransitionDescriptor,
    flip_flop: bool,
    flight: Option<Flight>,
    completion_count: u64,
    tbar_at_end: bool,
}

impl SwitcherState {
    /// Build one M/E with a background take armed and flip-flop enabled.
    #[must_use]
    pub fn new(program: SourceRef, preview: SourceRef, keys_on_air: Vec<bool>) -> Self {
        Self {
            program,
            preview,
            keys_on_air,
            armed: ArmedSet::default().with_background(),
            transition: TransitionDescriptor::Cut,
            flip_flop: true,
            flight: None,
            completion_count: 0,
            tbar_at_end: false,
        }
    }

    /// Current program background scene.
    #[must_use]
    pub fn program(&self) -> &SourceRef {
        &self.program
    }
    /// Current preview background scene.
    #[must_use]
    pub fn preview(&self) -> &SourceRef {
        &self.preview
    }
    /// Key on-air states in one-based key order.
    #[must_use]
    pub fn keys_on_air(&self) -> &[bool] {
        &self.keys_on_air
    }
    /// The selected next-transition elements.
    #[must_use]
    pub fn armed(&self) -> &ArmedSet {
        &self.armed
    }
    /// The selected transition descriptor.
    #[must_use]
    pub fn transition(&self) -> &TransitionDescriptor {
        &self.transition
    }
    /// Whether completed background takes swap program and preview.
    #[must_use]
    pub fn flip_flop(&self) -> bool {
        self.flip_flop
    }
    /// Whether a manual or automatic take is active.
    #[must_use]
    pub fn is_in_flight(&self) -> bool {
        self.flight.is_some()
    }
    /// Total completed takes, useful to correlate completion edges.
    #[must_use]
    pub fn completion_count(&self) -> u64 {
        self.completion_count
    }

    /// Set per-M/E flip-flop behavior for subsequent completed takes.
    pub fn set_flip_flop(&mut self, enabled: bool) {
        self.flip_flop = enabled;
    }

    /// Stage the preview scene; an active take keeps its already-selected target.
    pub fn set_preview(&mut self, scene: SourceRef) {
        if self.flight.is_none() {
            self.preview = scene;
            self.tbar_at_end = false;
        }
    }

    /// Select the elements that move on the next take. No duplicates or zero keys.
    ///
    /// # Errors
    /// Returns [`SwitcherError::NothingArmed`] for an empty set or
    /// [`SwitcherError::InvalidKey`] for duplicate/out-of-range indices.
    pub fn arm(&mut self, armed: ArmedSet) -> Result<(), SwitcherError> {
        if armed.is_empty() {
            return Err(SwitcherError::NothingArmed);
        }
        let mut seen = vec![false; self.keys_on_air.len()];
        for &index in &armed.keys {
            let Some(slot) = index
                .checked_sub(1)
                .and_then(|i| usize::try_from(i).ok())
                .and_then(|i| seen.get_mut(i))
            else {
                return Err(SwitcherError::InvalidKey);
            };
            if *slot {
                return Err(SwitcherError::InvalidKey);
            }
            *slot = true;
        }
        if self.flight.is_none() {
            self.armed = armed;
            self.tbar_at_end = false;
        }
        Ok(())
    }

    /// Configure the next take; invalid dip/stinger parameters are rejected.
    ///
    /// # Errors
    /// Returns [`SwitcherError::InvalidTransition`] on invalid timing.
    pub fn set_transition(
        &mut self,
        descriptor: TransitionDescriptor,
    ) -> Result<(), SwitcherError> {
        let valid = match &descriptor {
            TransitionDescriptor::Dip {
                rate, switch_point, ..
            } => {
                rate.get() >= 2
                    && switch_point.num > 0
                    && switch_point.den > 0
                    && switch_point.num < switch_point.den
            }
            TransitionDescriptor::Stinger {
                trigger_point,
                mix_rate,
                clip_duration,
                ..
            } => trigger_point
                .get()
                .checked_add(mix_rate.get())
                .is_some_and(|sum| sum <= clip_duration.get()),
            _ => true,
        };
        if !valid {
            return Err(SwitcherError::InvalidTransition);
        }
        if self.flight.is_none() {
            self.transition = descriptor;
            self.tbar_at_end = false;
        }
        Ok(())
    }

    /// Start AUTO on this tick; repeated AUTO during a flight cannot stack.
    ///
    /// # Errors
    /// Returns [`SwitcherError::NothingArmed`] if no elements are armed.
    pub fn auto(&mut self, tick: Tick) -> Result<SwitcherProjection, SwitcherError> {
        self.require_armed()?;
        if self.flight.is_some() {
            return Ok(self.sample(tick));
        }
        if matches!(self.transition, TransitionDescriptor::Cut) {
            return self.cut();
        }
        let rate = self.rate();
        self.tbar_at_end = false;
        self.flight = Some(Flight::Auto {
            start: tick.index,
            rate,
        });
        Ok(SwitcherProjection {
            progress: Progress::ZERO,
            started: true,
            completed: false,
        })
    }

    /// Set the absolute u16 T-bar position; duplicates do not advance progress.
    /// Manual input overrides any AUTO ramp until a take completes or aborts.
    ///
    /// # Errors
    /// Returns [`SwitcherError::NothingArmed`] if no elements are armed.
    pub fn tbar(
        &mut self,
        position: u16,
        _tick: Tick,
    ) -> Result<SwitcherProjection, SwitcherError> {
        self.require_armed()?;
        if self.flight.is_none() && self.tbar_at_end {
            return Ok(SwitcherProjection::idle());
        }
        let started = self.flight.is_none();
        if position == u16::MAX {
            let mut projection = self.finish();
            self.tbar_at_end = true;
            projection.started = started;
            return Ok(projection);
        }
        self.tbar_at_end = false;
        self.flight = Some(Flight::Manual { position });
        Ok(SwitcherProjection {
            progress: Progress::new(
                u32::from(position),
                FrameCount::try_from(u32::from(u16::MAX)).unwrap_or(FrameCount::ONE),
            ),
            started,
            completed: false,
        })
    }

    /// Instant take: complete the active flight or start and finish one cut.
    ///
    /// # Errors
    /// Returns [`SwitcherError::NothingArmed`] if no elements are armed.
    pub fn cut(&mut self) -> Result<SwitcherProjection, SwitcherError> {
        self.require_armed()?;
        self.tbar_at_end = false;
        let started = self.flight.is_none();
        let mut projection = self.finish();
        projection.started = started;
        Ok(projection)
    }

    /// Cancel the active flight, keeping PGM/PVW, key states and arming intact.
    pub fn abort(&mut self) -> SwitcherProjection {
        self.flight = None;
        self.tbar_at_end = false;
        SwitcherProjection::idle()
    }

    /// Punch a scene directly onto program without a preview swap or key change.
    pub fn punch(&mut self, scene: SourceRef) -> SwitcherProjection {
        self.flight = None;
        self.tbar_at_end = false;
        self.program = scene;
        self.completion_count = self.completion_count.saturating_add(1);
        SwitcherProjection {
            progress: Progress::COMPLETE,
            started: true,
            completed: true,
        }
    }

    /// Project the current flight onto `tick`, completing at the terminal tick.
    pub fn sample(&mut self, tick: Tick) -> SwitcherProjection {
        match self.flight {
            None => SwitcherProjection::idle(),
            Some(Flight::Manual { position }) => SwitcherProjection {
                progress: Progress::new(
                    u32::from(position),
                    FrameCount::try_from(u32::from(u16::MAX)).unwrap_or(FrameCount::ONE),
                ),
                started: false,
                completed: false,
            },
            Some(Flight::Auto { start, rate }) => {
                let elapsed = tick.index.saturating_sub(start);
                if elapsed >= u64::from(rate.get()) {
                    self.finish()
                } else {
                    SwitcherProjection {
                        progress: Progress::new(u32::try_from(elapsed).unwrap_or(rate.get()), rate),
                        started: false,
                        completed: false,
                    }
                }
            }
        }
    }

    fn require_armed(&self) -> Result<(), SwitcherError> {
        if self.armed.is_empty() {
            Err(SwitcherError::NothingArmed)
        } else {
            Ok(())
        }
    }

    fn rate(&self) -> FrameCount {
        match &self.transition {
            TransitionDescriptor::Mix { rate }
            | TransitionDescriptor::Dip { rate, .. }
            | TransitionDescriptor::Wipe { rate, .. }
            | TransitionDescriptor::DvePush { rate, .. }
            | TransitionDescriptor::DveSqueeze { rate, .. } => *rate,
            TransitionDescriptor::Stinger { clip_duration, .. } => *clip_duration,
            _ => FrameCount::ONE,
        }
    }

    fn finish(&mut self) -> SwitcherProjection {
        self.flight = None;
        if self.armed.background {
            if self.flip_flop {
                std::mem::swap(&mut self.program, &mut self.preview);
            } else {
                self.program = self.preview.clone();
            }
        }
        for &index in &self.armed.keys {
            if let Some(key) = usize::try_from(index - 1)
                .ok()
                .and_then(|i| self.keys_on_air.get_mut(i))
            {
                *key = !*key;
            }
        }
        self.completion_count = self.completion_count.saturating_add(1);
        SwitcherProjection {
            progress: Progress::COMPLETE,
            started: false,
            completed: true,
        }
    }
}
