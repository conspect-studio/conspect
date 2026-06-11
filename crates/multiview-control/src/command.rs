//! The engine **command bus** (ADR-W008): control's inbound channel to the
//! engine.
//!
//! Multiview's engine (`multiview-engine`) does not yet expose an inbound command
//! intake (hot-reconfiguration is a later milestone), so the command-bus
//! contract is defined here. It is the *one* direction in which the control
//! plane talks **to** the engine, and it is built so it can **never**
//! back-pressure the engine (invariant #10):
//!
//! * It is a **bounded** `tokio::sync::mpsc` channel. Control is the producer;
//!   the engine side holds the drainable [`CommandReceiver`].
//! * Control submits with [`CommandSender::try_submit`], which **never blocks
//!   or awaits**: when the queue is full it returns [`SubmitError::Full`]
//!   immediately (the HTTP layer maps that to `503` /
//!   [`ControlError::EngineBusy`](crate::error::ControlError::EngineBusy)). A
//!   slow or stopped engine drain therefore sheds control load rather than
//!   blocking control, and a flood of control commands can never grow memory
//!   without bound or stall the engine.
//! * The engine drains **at its leisure** with [`CommandReceiver::try_drain`]
//!   (non-blocking, e.g. once per tick) or [`CommandReceiver::recv`] (awaited on
//!   its own task) — its choice, never forced by control.
//!
//! Every submitted command carries an [`OperationId`]: long-running commands
//! return `202 Accepted` + that id immediately, and the engine reports the
//! eventual outcome on the realtime event stream correlated by the same id
//! (ADR-W008). The HTTP response never waits for the engine to apply the change.
use std::fmt;

use multiview_config::routing::StreamRef;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A correlation id for an asynchronously-applied control command.
///
/// Returned to the client in a `202 Accepted` response and echoed by the engine
/// on the realtime stream (`corr`) when the command's outcome is known.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(transparent)]
pub struct OperationId(String);

impl OperationId {
    /// Mint a fresh, random operation id.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    /// Wrap an existing id string (e.g. one parsed from a request).
    #[must_use]
    pub fn from_string(id: String) -> Self {
        Self(id)
    }

    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for OperationId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for OperationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A stored named layout **resolved + solved at the route** (off the engine
/// hot path), carried by [`Command::ApplyLayout`] (ADR-W019).
///
/// The HTTP handler reads the body from the layouts repository at request
/// time, parses it as a typed [`multiview_config::LayoutDocument`], and solves
/// it to a validated [`multiview_core::layout::Layout`] named after the stored
/// id — failing with `422` **before** any `202` when the id is unknown or the
/// body does not parse/solve. The engine's frame-boundary drain therefore does
/// **no I/O and no solving**: applying is one `set_layout` pointer swap plus
/// O(cells) id/slate rebinds (invariants #1/#10).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ResolvedLayout {
    /// The solved core layout (canvas + normalized cells + source bindings),
    /// named after the stored layout id.
    pub solved: multiview_core::layout::Layout,
    /// The typed stored-layout document (placement strategy + schema cells),
    /// mirrored into the engine's working config on apply so export/salvo/
    /// fallback surfaces stay coherent with the active layout.
    pub document: multiview_config::LayoutDocument,
}

impl ResolvedLayout {
    /// Bundle a solved layout with its source document.
    #[must_use]
    pub const fn new(
        solved: multiview_core::layout::Layout,
        document: multiview_config::LayoutDocument,
    ) -> Self {
        Self { solved, document }
    }
}

/// A control-plane command destined for the engine.
///
/// These are the management mutations that must be applied on the data plane
/// (at a frame boundary, per the Class-1/Class-2 model, invariant #11). The
/// control plane validates and enqueues them; the engine applies them when it
/// drains. Each is correlated by an [`OperationId`].
///
/// Not `Eq`/`Hash`: [`Command::RouteAudio`] carries a floating-point `gain_db`
/// (a level), so the enum is `PartialEq` only — commands are matched and routed,
/// never used as a hash key.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Command {
    /// Start program output.
    Start {
        /// Correlation id for the async outcome.
        op: OperationId,
    },
    /// Stop program output.
    Stop {
        /// Correlation id for the async outcome.
        op: OperationId,
    },
    /// Swap the source bound to a tile (make-before-break).
    ///
    /// This is the **desugared alias** of [`Command::RouteVideo`] with a
    /// `StreamRef{ source, Video, Best }` selector (ADR-0034 / RT-11): a legacy
    /// `SwapSource{tile,source}` and the equivalent `RouteVideo` apply to the
    /// **same** O(1) `CompositorDrive::rebind_cell` crosspoint. Kept as a distinct
    /// variant for back-compat; new callers should prefer [`Command::RouteVideo`].
    SwapSource {
        /// Correlation id for the async outcome.
        op: OperationId,
        /// The tile/cell id whose binding changes.
        tile: String,
        /// The new source/input id to bind.
        source: String,
    },
    /// Re-point a layout **cell** to a video [`StreamRef`] — the per-stream VIDEO
    /// crosspoint (ADR-0034 / RT-11, RT-6 `rebind_cell`). Class-1 (hot, seamless):
    /// the next frame draws the new source, no encoder/session reset.
    RouteVideo {
        /// Correlation id for the async outcome.
        op: OperationId,
        /// The destination layout cell id.
        cell: String,
        /// The source elementary stream feeding the cell.
        source: StreamRef,
    },
    /// Re-point a program-bus channel / discrete **track** to an audio
    /// [`StreamRef`] — the per-stream AUDIO crosspoint / breakaway (ADR-0034 /
    /// RT-11, RT-8a/RT-9 `ProgramBus::repoint_crossfade`). Class-1 onto the
    /// program bus (mixer re-route, pop-free cross-fade); a breakaway onto a
    /// discrete track whose pinned layout differs is Class-2 (see
    /// [`crate::routing::classify`]).
    RouteAudio {
        /// Correlation id for the async outcome.
        op: OperationId,
        /// The destination program-bus channel / discrete-track name.
        target: String,
        /// The source elementary stream feeding the target.
        source: StreamRef,
        /// Program-bus contribution gain in dB (`0.0` ⇒ unity).
        gain_db: f32,
        /// Whether the source contributes silence (still routed).
        mute: bool,
    },
    /// Re-point a subtitle **layer** to a subtitle [`StreamRef`] — the per-stream
    /// SUBTITLE crosspoint / breakaway (ADR-0034 / RT-11, RT-10a
    /// `SubtitleLayer::repoint`). Class-1 onto an existing layer (hard cut,
    /// CLEAR-on-switch).
    RouteSubtitle {
        /// Correlation id for the async outcome.
        op: OperationId,
        /// The destination subtitle layer id.
        layer: String,
        /// The source elementary stream feeding the layer.
        source: StreamRef,
    },
    /// Apply a new layout to the running multiview at the next frame boundary
    /// (invariant #11 Class-1; ADR-R004 atomic scene-graph swap).
    ApplyLayout {
        /// Correlation id for the async outcome.
        op: OperationId,
        /// The layout id to make active.
        layout: String,
        /// The stored layout, resolved + solved **at the route** (ADR-W019), so
        /// the frame-boundary drain never reads the repository or re-solves on
        /// the render thread. [`None`] is the back-compat form: the engine then
        /// falls back to re-solving its working config iff `layout` matches the
        /// solved working layout's name. Boxed to keep the command small on the
        /// bounded bus.
        document: Option<Box<ResolvedLayout>>,
    },
    /// Arm (stage) a salvo so it is ready for an atomic take. Arming never
    /// changes program output; it only stages the recall (broadcast-multiviewer
    /// brief §8).
    ArmSalvo {
        /// Correlation id for the async outcome.
        op: OperationId,
        /// The salvo id to stage.
        salvo: String,
        /// The output head this recall targets, if scoped to one head.
        head: Option<String>,
    },
    /// Take (atomically apply) a salvo. If `salvo` is `None` the engine takes the
    /// currently-armed salvo; otherwise it takes (arm-then-take) the named salvo.
    TakeSalvo {
        /// Correlation id for the async outcome.
        op: OperationId,
        /// The salvo id to take, or `None` to take the armed salvo.
        salvo: Option<String>,
        /// The output head this recall targets, if scoped to one head.
        head: Option<String>,
    },
    /// Cancel a previously-armed salvo before it is taken.
    CancelSalvo {
        /// Correlation id for the async outcome.
        op: OperationId,
        /// The salvo id to cancel, or `None` to cancel the armed salvo.
        salvo: Option<String>,
        /// The output head this recall targets, if scoped to one head.
        head: Option<String>,
    },
    /// Create **or replace** a managed source on the **running** engine
    /// (ADR-W018 live apply, invariant #11). Carries the full, already-validated
    /// (ADR-W015) config document; the engine drain registers the source's frame
    /// store + route key at a frame boundary and hands the heavy producer
    /// spawn/teardown to an off-thread hub. An upsert under an existing id is a
    /// live **edit**: the registered `TileStore` is reused so the bound tile
    /// holds last-good through the producer swap.
    UpsertSource {
        /// Correlation id for the async outcome.
        op: OperationId,
        /// The validated source document to apply (boxed: the source document
        /// is much larger than the other command variants).
        source: Box<multiview_config::Source>,
    },
    /// Remove a managed source from the **running** engine (ADR-W018): the
    /// frame store unregisters at a frame boundary (bound cells composite their
    /// `on_loss` slate from the next tick) and the producer is torn down off the
    /// clock thread (bounded).
    RemoveSource {
        /// Correlation id for the async outcome.
        op: OperationId,
        /// The source id to remove.
        id: String,
    },
    /// Force (or clear) a manual tally override on a tile/element, taking
    /// precedence over the arbitrated bus state until released. A `color` of
    /// [`None`] clears the override and returns the element to arbitration.
    SetTallyOverride {
        /// Correlation id for the async outcome.
        op: OperationId,
        /// The tally target the override applies to.
        target: multiview_events::TallyTarget,
        /// The colour to force, or `None` to clear the override.
        color: Option<multiview_core::tally::TallyColor>,
    },
}

impl Command {
    /// The operation id correlating this command's eventual outcome.
    #[must_use]
    pub fn operation_id(&self) -> &OperationId {
        match self {
            Self::Start { op }
            | Self::Stop { op }
            | Self::SwapSource { op, .. }
            | Self::RouteVideo { op, .. }
            | Self::RouteAudio { op, .. }
            | Self::RouteSubtitle { op, .. }
            | Self::ApplyLayout { op, .. }
            | Self::ArmSalvo { op, .. }
            | Self::TakeSalvo { op, .. }
            | Self::CancelSalvo { op, .. }
            | Self::UpsertSource { op, .. }
            | Self::RemoveSource { op, .. }
            | Self::SetTallyOverride { op, .. } => op,
        }
    }

    /// A stable machine-readable label for the command kind.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Start { .. } => "start",
            Self::Stop { .. } => "stop",
            Self::SwapSource { .. } => "swap_source",
            Self::RouteVideo { .. } => "route_video",
            Self::RouteAudio { .. } => "route_audio",
            Self::RouteSubtitle { .. } => "route_subtitle",
            Self::ApplyLayout { .. } => "apply_layout",
            Self::ArmSalvo { .. } => "arm_salvo",
            Self::TakeSalvo { .. } => "take_salvo",
            Self::CancelSalvo { .. } => "cancel_salvo",
            Self::UpsertSource { .. } => "upsert_source",
            Self::RemoveSource { .. } => "remove_source",
            Self::SetTallyOverride { .. } => "set_tally_override",
        }
    }

    /// Desugar this command into the engine-native [`RouteIntent`] it applies, if
    /// it is a routing command (`SwapSource` / `Route*`), else [`None`].
    ///
    /// This is the bridge the engine's command drain uses: `multiview-control`
    /// depends on `multiview-engine` (not the reverse), so the control plane
    /// translates its `Command` into the engine's intent type. `SwapSource`
    /// desugars to a `RouteIntent::Video { …, StreamRef{source, Video, Best} }`
    /// (ADR-0034 / RT-11 — the alias), so a legacy swap and the equivalent
    /// `RouteVideo` apply identically.
    #[must_use]
    pub fn route_intent(&self) -> Option<multiview_engine::RouteIntent> {
        use multiview_engine::RouteIntent;
        match self {
            Self::SwapSource { tile, source, .. } => {
                Some(RouteIntent::swap_source(tile.clone(), source.clone()))
            }
            Self::RouteVideo { cell, source, .. } => Some(RouteIntent::Video {
                cell: cell.clone(),
                source: source.clone(),
            }),
            Self::RouteAudio {
                target,
                source,
                gain_db,
                mute,
                ..
            } => Some(RouteIntent::Audio {
                target: target.clone(),
                source: source.clone(),
                gain_db: *gain_db,
                mute: *mute,
            }),
            Self::RouteSubtitle { layer, source, .. } => Some(RouteIntent::Subtitle {
                layer: layer.clone(),
                source: source.clone(),
            }),
            _ => None,
        }
    }
}

/// Why a non-blocking command submission failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SubmitError {
    /// The bounded queue is full. The submission is shed (never blocks the
    /// engine); the caller should retry later. Maps to `503`.
    #[error("command bus is at capacity")]
    Full,
    /// The engine side (the receiver) has been dropped; the engine is gone.
    #[error("command bus receiver is closed")]
    Closed,
}

/// The control-side producer half of the command bus.
///
/// Cloneable so every request handler shares one bus. Submission is **always
/// non-blocking** ([`CommandSender::try_submit`]); there is deliberately no
/// awaiting/back-pressuring send, so control can never make the engine wait and
/// the engine can never make control wait.
#[derive(Debug, Clone)]
pub struct CommandSender {
    tx: tokio::sync::mpsc::Sender<Command>,
    capacity: usize,
}

/// The engine-side drainable consumer half of the command bus.
///
/// The engine owns this and drains it on its own schedule — never forced by
/// control. Draining is non-blocking ([`CommandReceiver::try_drain`]) or an
/// awaited [`CommandReceiver::recv`] on a dedicated task; either way control
/// cannot block the engine's drain and the engine's drain cannot block control.
#[derive(Debug)]
pub struct CommandReceiver {
    rx: tokio::sync::mpsc::Receiver<Command>,
}

/// Create a bounded command bus with room for `capacity` queued commands,
/// returning the control-side sender and the engine-side receiver.
///
/// A `capacity` of `0` is promoted to `1` (`tokio::sync::mpsc` requires a
/// positive buffer).
#[must_use]
pub fn command_bus(capacity: usize) -> (CommandSender, CommandReceiver) {
    let capacity = capacity.max(1);
    let (tx, rx) = tokio::sync::mpsc::channel(capacity);
    (CommandSender { tx, capacity }, CommandReceiver { rx })
}

impl CommandSender {
    /// Submit a command **without blocking or awaiting**.
    ///
    /// Returns the command's [`OperationId`] on success. On a full queue returns
    /// [`SubmitError::Full`] (shed the load — the engine is never blocked); if
    /// the engine side has gone away returns [`SubmitError::Closed`].
    ///
    /// # Errors
    ///
    /// [`SubmitError::Full`] when the bounded queue is saturated, or
    /// [`SubmitError::Closed`] when the receiver was dropped.
    pub fn try_submit(&self, command: Command) -> Result<OperationId, SubmitError> {
        let op = command.operation_id().clone();
        match self.tx.try_send(command) {
            Ok(()) => Ok(op),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Err(SubmitError::Full),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => Err(SubmitError::Closed),
        }
    }

    /// The bounded queue depth (the maximum number of un-drained commands).
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

impl CommandReceiver {
    /// Drain every currently-queued command without awaiting.
    ///
    /// Returns the commands in FIFO order. The engine calls this on its own
    /// cadence (e.g. once per tick). It never blocks waiting for more.
    #[must_use]
    pub fn try_drain(&mut self) -> Vec<Command> {
        let mut drained = Vec::new();
        while let Ok(command) = self.rx.try_recv() {
            drained.push(command);
        }
        drained
    }

    /// Await the next command (for an engine that drains on a dedicated task).
    ///
    /// Returns [`None`] once every [`CommandSender`] has been dropped.
    pub async fn recv(&mut self) -> Option<Command> {
        self.rx.recv().await
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::float_cmp)]

    use super::*;
    use multiview_config::routing::{StreamRef, StreamSelector};
    use multiview_core::stream::StreamKind;
    use multiview_engine::RouteIntent;

    #[test]
    fn swap_source_desugars_to_the_route_video_best_alias() {
        // ADR-0034 / RT-11 back-compat: a legacy `SwapSource{tile,source}` and the
        // equivalent `RouteVideo{cell, StreamRef{source, Video, Best}}` desugar to
        // the SAME engine route intent — so the alias keeps working.
        let swap = Command::SwapSource {
            op: OperationId::new(),
            tile: "c0".to_owned(),
            source: "cam-b".to_owned(),
        };
        let route = Command::RouteVideo {
            op: OperationId::new(),
            cell: "c0".to_owned(),
            source: StreamRef::best("cam-b", StreamKind::Video),
        };
        let swap_intent = swap.route_intent().expect("SwapSource desugars");
        let route_intent = route.route_intent().expect("RouteVideo desugars");
        assert_eq!(
            swap_intent, route_intent,
            "SwapSource is the desugared alias of RouteVideo{{Video, Best}}"
        );
        assert_eq!(
            swap_intent,
            RouteIntent::Video {
                cell: "c0".to_owned(),
                source: StreamRef::best("cam-b", StreamKind::Video),
            }
        );
    }

    #[test]
    fn route_audio_carries_gain_and_mute_into_the_intent() {
        let mut source = StreamRef::best("cam-b", StreamKind::Audio);
        source.selector = StreamSelector::language("eng".to_owned());
        let cmd = Command::RouteAudio {
            op: OperationId::new(),
            target: "prog".to_owned(),
            source,
            gain_db: -3.0,
            mute: true,
        };
        match cmd.route_intent().expect("RouteAudio desugars") {
            RouteIntent::Audio {
                target,
                gain_db,
                mute,
                ..
            } => {
                assert_eq!(target, "prog");
                assert_eq!(gain_db, -3.0);
                assert!(mute);
            }
            other => panic!("expected an Audio intent, got {other:?}"),
        }
        assert_eq!(cmd.kind(), "route_audio");
    }

    #[test]
    fn non_routing_commands_have_no_route_intent() {
        let start = Command::Start {
            op: OperationId::new(),
        };
        assert!(start.route_intent().is_none());
    }
}
