//! Bounded, nonblocking control-to-tick handoff for the first M/E.
//!
//! The data plane is the sole [`SwitcherTickDriver`] owner. No REST task can
//! lock its state or block its clock; admitted intents use `try_send`, and the
//! driver performs a fixed maximum of `try_recv` calls on each output tick.

use multiview_core::layout::Layout;
use multiview_core::switcher::{FrameCount, SourceRef, TransitionDescriptor};
use std::sync::Arc;

use crate::switcher_plan::validate_mix;
use tokio::sync::mpsc;

use crate::clock::Tick;
use crate::isolation::{event_stream, EventStream, EventSubscription, LatestState};
use crate::switcher::{SwitcherProjection, SwitcherState};

/// Maximum command applications in one output frame.
pub const MAX_COMMANDS_PER_TICK: usize = 16;
/// Maximum queued commands, including unresponsive data-plane scenarios.
pub const COMMAND_CAPACITY: usize = 64;

/// An operator verb, already authorized and reference-validated on admission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitcherVerb {
    /// Arm a new preview source, without changing on-air output.
    SelectPreview(SourceRef),
    /// Punch a source directly onto on-air PGM at the next frame boundary.
    SelectProgram(SourceRef),
    /// Atomically take PVW to PGM at the next frame boundary.
    Cut,
    /// Begin an exact-duration automatic take on the next frame boundary.
    Auto(FrameCount),
}

/// One correlated command; `operation_id` is opaque and never interpreted by
/// the data plane. The caller owns idempotency before enqueue.
#[derive(Debug, Clone)]
pub struct SwitcherCommand {
    /// Stable operation identifier returned by the 202 response.
    pub operation_id: String,
    /// The scene generation validated at API admission. Rechecked at the tick.
    pub scene_version: u64,
    /// The operator action.
    pub verb: SwitcherVerb,
}

/// Engine-owned bus selection projected at a real output frame boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitcherSnapshot {
    /// Output tick on which this snapshot was published.
    pub tick: u64,
    /// Current PGM source id.
    pub program: SourceRef,
    /// Current PVW source id.
    pub preview: SourceRef,
    /// Whether an automatic transition is in progress.
    pub in_flight: bool,
    /// Number of completed takes, monotonic within one runtime.
    pub completion_count: u64,
}

/// Definitive command result after the engine has processed it at a tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitcherOutcome {
    /// Operation identifier correlated with the accepted 202.
    pub operation_id: String,
    /// Exact tick on which the engine applied or rejected the command.
    pub tick: u64,
    /// `true` means the operation reached its terminal applied frame, not merely armed.
    pub completed: bool,
    /// `None` means applied; otherwise typed machine or geometry refusal.
    pub error: Option<String>,
}

/// A cloneable control-plane handle; it cannot mutate the machine directly.
#[derive(Debug, Clone)]
pub struct SwitcherControl {
    tx: mpsc::Sender<SwitcherCommand>,
    snapshots: LatestState<SwitcherSnapshot>,
    outcomes: EventStream<SwitcherOutcome>,
    scenes: LatestState<ScenePair>,
}

/// Versioned, prevalidated render scenes. A command never references mutable
/// control-side scene geometry; the engine resolves this immutable pair at arm.
#[derive(Debug, Clone)]
pub struct ScenePair {
    /// Monotonic authored-scene generation.
    pub version: u64,
    /// Current on-air scene geometry.
    pub program: Arc<Layout>,
    /// Armed preview scene geometry.
    pub preview: Arc<Layout>,
}

impl SwitcherControl {
    /// Construct a bounded mailbox and engine-owned driver; initial state is not
    /// reported as applied until the first output tick calls `drain_at_tick`.
    #[must_use]
    pub fn new(machine: SwitcherState) -> (Self, SwitcherTickDriver) {
        let (tx, rx) = mpsc::channel(COMMAND_CAPACITY);
        let snapshots = LatestState::new();
        let (outcomes, _) = event_stream(256);
        let handle = Self {
            tx,
            snapshots,
            outcomes,
            scenes: LatestState::new(),
        };
        let driver = SwitcherTickDriver {
            rx,
            machine,
            snapshots: handle.snapshots.clone(),
            outcomes: handle.outcomes.clone(),
            scenes: handle.scenes.clone(),
            active_auto: None,
            active_auto_scenes: None,
        };
        (handle, driver)
    }

    /// Admit without waiting; a full/closed queue is an explicit refusal and
    /// MUST NOT receive a 202 response.
    ///
    /// # Errors
    /// Returns the command unchanged if the bounded mailbox is full or closed.
    pub fn try_send(&self, command: SwitcherCommand) -> Result<(), SwitcherCommand> {
        self.tx.try_send(command).map_err(|err| err.into_inner())
    }

    /// Publish an immutable pair after validating its layout geometry. This is
    /// control-side only; a later edit cannot mutate a previously published pair.
    ///
    /// # Errors
    /// Refuses invalid source scenes; an AUTO additionally validates the exact
    /// pair captured at the engine tick before arming.
    pub fn set_scenes(&self, pair: ScenePair) -> Result<(), String> {
        pair.program.validate().map_err(|e| e.to_string())?;
        pair.preview.validate().map_err(|e| e.to_string())?;
        self.scenes.publish(pair);
        Ok(())
    }

    /// Most recently applied frame-boundary state, never a predicted state.
    #[must_use]
    pub fn snapshot(&self) -> Option<std::sync::Arc<SwitcherSnapshot>> {
        self.snapshots.latest()
    }

    /// Scene generation presently available for command admission. This is
    /// descriptive only: the caller must verify a real bus frame provider is
    /// attached before returning 202; a scene snapshot alone is not capability.
    #[must_use]
    pub fn scene_version(&self) -> Option<u64> {
        self.scenes.latest().map(|pair| pair.version)
    }

    /// Subscribe to correlated, drop-oldest applied outcomes. A lagged reader
    /// must re-fetch state and operation status; it never stalls the output tick.
    #[must_use]
    pub fn outcomes(&self) -> EventSubscription<SwitcherOutcome> {
        self.outcomes.subscribe()
    }
}

/// Sole output-clock owner of the M/E state; do not share with REST handlers.
#[derive(Debug)]
pub struct SwitcherTickDriver {
    rx: mpsc::Receiver<SwitcherCommand>,
    machine: SwitcherState,
    snapshots: LatestState<SwitcherSnapshot>,
    outcomes: EventStream<SwitcherOutcome>,
    scenes: LatestState<ScenePair>,
    active_auto: Option<String>,
    active_auto_scenes: Option<Arc<ScenePair>>,
}

impl SwitcherTickDriver {
    /// Immutable scene pair pinned for the duration of the active AUTO, if any.
    /// The compositor must render this pair rather than a newer authored pair
    /// until the take completes or is interrupted.
    #[must_use]
    pub fn scene_pair(&self) -> Option<Arc<ScenePair>> {
        self.active_auto_scenes
            .clone()
            .or_else(|| self.scenes.latest())
    }

    /// Process at most 16 queued commands, then advance the exact-tick machine;
    /// always produces one snapshot even if a client wedges or a command fails.
    /// The caller composes the resulting PGM/PVW scene on this same `tick`.
    pub fn drain_at_tick(&mut self, tick: Tick) -> SwitcherProjection {
        let mut projection = self.machine.sample(tick);
        if projection.completed {
            if let Some(operation_id) = self.active_auto.take() {
                self.active_auto_scenes = None;
                self.publish_outcome(operation_id, tick.index, true, None);
            }
        }
        for _ in 0..MAX_COMMANDS_PER_TICK {
            let Ok(command) = self.rx.try_recv() else {
                break;
            };
            let scenes = self.scenes.latest();
            let result: Result<SwitcherProjection, String> = if scenes
                .as_ref()
                .is_some_and(|pair| pair.version != command.scene_version)
            {
                Err("scene version changed before the frame boundary; re-plan the operation".into())
            } else {
                match &command.verb {
                    SwitcherVerb::SelectPreview(source) => {
                        if let Some(pair) = &scenes {
                            if !pair
                                .preview
                                .cells
                                .iter()
                                .any(|cell| cell.source.as_deref() == Some(source.as_str()))
                            {
                                Err("preview source is absent from the active scene".into())
                            } else {
                                self.machine
                                    .set_preview(source.clone())
                                    .map(|()| self.machine.sample(tick))
                                    .map_err(|e| e.to_string())
                            }
                        } else {
                            Err("no active PGM/PVW scene provider".into())
                        }
                    }
                    SwitcherVerb::SelectProgram(source) => {
                        if let Some(pair) = &scenes {
                            if !pair
                                .program
                                .cells
                                .iter()
                                .any(|cell| cell.source.as_deref() == Some(source.as_str()))
                            {
                                Err("program source is absent from the active scene".into())
                            } else {
                                Ok(self.machine.punch(source.clone()))
                            }
                        } else {
                            Err("no active PGM/PVW scene provider".into())
                        }
                    }
                    SwitcherVerb::Cut => {
                        if scenes.is_some() {
                            self.machine.cut().map_err(|e| e.to_string())
                        } else {
                            Err("no active PGM/PVW scene provider".into())
                        }
                    }
                    SwitcherVerb::Auto(rate) => {
                        if self.machine.is_in_flight() {
                            Err("cannot start a second AUTO during an active take".into())
                        } else if let Some(pair) = &scenes {
                            validate_mix(&pair.program, &pair.preview)
                                .map_err(|e| e.to_string())
                                .and_then(|()| {
                                    self.machine
                                        .set_transition(TransitionDescriptor::Mix { rate: *rate })
                                        .map_err(|e| e.to_string())
                                })
                                .and_then(|()| self.machine.auto(tick).map_err(|e| e.to_string()))
                        } else {
                            Err("no active PGM/PVW scene provider".into())
                        }
                    }
                }
            };
            match result {
                Ok(next) => {
                    projection = next;
                    if matches!(command.verb, SwitcherVerb::Auto(_)) && !next.completed {
                        self.active_auto_scenes = scenes;
                        self.active_auto = Some(command.operation_id);
                    } else {
                        if matches!(
                            command.verb,
                            SwitcherVerb::Cut | SwitcherVerb::SelectProgram(_)
                        ) {
                            if let Some(old) = self.active_auto.take() {
                                self.active_auto_scenes = None;
                                self.publish_outcome(
                                    old,
                                    tick.index,
                                    false,
                                    Some("AUTO interrupted by a new on-air operation".into()),
                                );
                            }
                        }
                        self.publish_outcome(command.operation_id, tick.index, true, None);
                    }
                }
                Err(error) => {
                    self.publish_outcome(command.operation_id, tick.index, false, Some(error))
                }
            }
        }
        self.snapshots.publish(SwitcherSnapshot {
            tick: tick.index,
            program: self.machine.program().clone(),
            preview: self.machine.preview().clone(),
            in_flight: self.machine.is_in_flight(),
            completion_count: self.machine.completion_count(),
        });
        projection
    }

    fn publish_outcome(
        &self,
        operation_id: String,
        tick: u64,
        completed: bool,
        error: Option<String>,
    ) {
        self.outcomes.publish(SwitcherOutcome {
            operation_id,
            tick,
            completed,
            error,
        });
    }
}
