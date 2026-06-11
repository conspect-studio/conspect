//! RT-11 / ADR-0034 §10 — the **#11 classifier** for per-stream crosspoint takes.
//!
//! Every management change is **Class-1** (hot/seamless at a frame boundary) or
//! **Class-2** (controlled reset via make-before-break), surfaced **before**
//! applying (invariant #11). The decoupled-routing brief §8 adds a third tier,
//! **Reset-lite** (a single IDR / discontinuity within the pre-allocated max), and
//! a **coerced-degradation** flag (an operator-confirmed down/up-mix that turns a
//! would-be Class-2 into a Class-1-with-degradation).
//!
//! The classifier is **honest at the edges** — it is NOT a universal "all
//! in-program re-points are Class-1". It inspects the **destination's pinned
//! params** ([`DestinationProfile`], the control-plane mirror of the output
//! `sink.rs` `StreamCodecParameters` snapshot) against the source's
//! [`StreamKind`] payload (here, an audio source's channel count):
//!
//! | Crosspoint | Class | Why (capability-matrix) |
//! |---|---|---|
//! | VIDEO re-point onto an existing/primed cell | **Class-1** | atomic scene-graph swap. |
//! | VIDEO re-point onto a **cold** target (spin-up) | **Reset-lite** | single IDR within the pre-allocated max. |
//! | AUDIO re-point onto the **program bus** | **Class-1** | the bus resamples to its working layout; the source layout is absorbed. |
//! | AUDIO breakaway onto a **discrete track** whose pinned layout matches | **Class-1** | hot mixer re-route. |
//! | AUDIO breakaway onto a discrete track whose pinned layout **differs** | **Class-2** | the mux pinned the layout for the session (track-set CRUD). |
//! | … same, with operator-confirmed coercion | **Class-1** (coerced) | down/up-mix to the pinned layout. |
//! | SUBTITLE re-point onto an existing layer | **Class-1** | hot re-route. |
//! | SUBTITLE breakaway requiring a **new track set** | **Class-2** | passthrough set CRUD. |
//!
//! The property the brief mandates and a test enforces: a breakaway whose source
//! layout ≠ the pinned discrete-track layout is **not** reported as plain Class-1.

use multiview_config::routing::StreamRef;
use multiview_core::stream::{StreamInventory, StreamKind};
use serde::{Deserialize, Serialize};

/// The #11 class of a crosspoint take — the three tiers the capability matrix
/// defines, surfaced **before** the take applies.
///
/// Serialised in `snake_case` (`"class1"` / `"reset_lite"` / `"class2"`) and
/// **never `untagged`**. `#[non_exhaustive]` for forward compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RouteClass {
    /// **Hot / seamless** at a frame boundary — no encoder/session reset.
    Class1,
    /// A single IDR / container discontinuity within the pre-allocated maximum
    /// (a cold-target spin-up); in-program but not strictly seamless.
    ResetLite,
    /// A **controlled reset** via make-before-break parallel-output migration
    /// (a pinned-param mismatch the session cannot absorb hot).
    Class2,
}

impl RouteClass {
    /// The stable `snake_case` label (matches the serde encoding).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Class1 => "class1",
            Self::ResetLite => "reset_lite",
            Self::Class2 => "class2",
        }
    }
}

/// The classified plan for a crosspoint take: the #11 class plus a
/// coerced-degradation flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct RoutePlan {
    /// The #11 class of the take.
    pub class: RouteClass,
    /// Whether the (would-be Class-2) take was **coerced** to Class-1 by an
    /// operator-confirmed down/up-mix to the destination's pinned layout — a
    /// Class-1 **with degradation**, surfaced so the operator sees the trade.
    pub coerced: bool,
}

/// The destination a crosspoint take targets — the TIER-2 side of the matrix.
///
/// Internally tagged by `kind` (never `untagged`, ADR-0010). Each variant carries
/// the destination id plus the bits the classifier needs (a discrete track's
/// pinned channel count). `#[non_exhaustive]` so output ← program (RT-12) can be
/// added later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum RouteTarget {
    /// A layout video cell.
    VideoCell {
        /// The destination cell id.
        cell: String,
    },
    /// The mixed **program bus** channel (the bus absorbs the source layout).
    AudioProgramBus {
        /// The program-bus channel name.
        channel: String,
    },
    /// A named **discrete output track** (its layout is pinned for the session).
    AudioDiscreteTrack {
        /// The discrete-track name.
        track: String,
        /// The channel count the mux pinned this track to for the session, if
        /// known (absent ⇒ the classifier cannot prove a mismatch and treats it
        /// as the program-bus-equivalent absorbing case).
        #[serde(default)]
        pinned_channels: Option<u16>,
    },
    /// A subtitle / caption layer.
    SubtitleLayer {
        /// The destination layer id.
        layer: String,
    },
}

/// One crosspoint take request: a destination + a source stream (+ optional
/// classifier hints carried for destinations whose pinned params are not yet in
/// the engine snapshot).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteRequest {
    /// The destination the take re-points.
    pub target: RouteTarget,
    /// The source elementary stream feeding the destination.
    pub source: StreamRef,
}

/// The destination's **pinned params** the classifier compares against — the
/// control-plane mirror of the output `sink.rs` `StreamCodecParameters` snapshot
/// (ADR-0034 §10). Carries exactly what each crosspoint kind needs to decide its
/// class:
///
/// * video → whether the target source is **primed** (warm) or **cold** (needs a
///   single IDR);
/// * audio → the **pinned channel layout** of a discrete track (the program bus
///   has none — it absorbs any source);
/// * subtitle → whether the layer **exists** (hot re-route) or needs a new track
///   set.
///
/// Plus a `coerce` flag: when set, a would-be Class-2 audio layout mismatch is
/// reported as Class-1-with-degradation (an operator-confirmed down/up-mix).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DestinationProfile {
    /// Video: whether the cold/warm state is known and warm.
    video_primed: Option<bool>,
    /// Audio discrete track: the pinned channel count (program bus = `None`).
    pinned_channels: Option<u16>,
    /// Audio: whether this destination is the layout-absorbing program bus.
    is_program_bus: bool,
    /// Subtitle: whether the destination layer already exists.
    layer_exists: Option<bool>,
    /// Whether an operator-confirmed coercion to the pinned layout is permitted.
    coerce: bool,
}

impl DestinationProfile {
    /// A video-cell destination whose target source is primed (`true`) or cold
    /// (`false`).
    #[must_use]
    pub const fn video_cell(primed: bool) -> Self {
        Self {
            video_primed: Some(primed),
            ..Self::new()
        }
    }

    /// The layout-absorbing program bus (no pinned layout — any source is
    /// Class-1).
    #[must_use]
    pub const fn audio_program_bus() -> Self {
        Self {
            is_program_bus: true,
            ..Self::new()
        }
    }

    /// A discrete output track pinned to `channels` channels for the session.
    #[must_use]
    pub const fn audio_discrete_track(channels: u16) -> Self {
        Self {
            pinned_channels: Some(channels),
            ..Self::new()
        }
    }

    /// A subtitle layer that already exists (`true`) or needs a new track set
    /// (`false`).
    #[must_use]
    pub const fn subtitle_layer(exists: bool) -> Self {
        Self {
            layer_exists: Some(exists),
            ..Self::new()
        }
    }

    /// Mark this destination as permitting an operator-confirmed coercion (a
    /// down/up-mix to the pinned layout), turning a would-be Class-2 audio
    /// mismatch into a Class-1-with-degradation.
    #[must_use]
    pub const fn coerce_to_pinned(mut self) -> Self {
        self.coerce = true;
        self
    }

    const fn new() -> Self {
        Self {
            video_primed: None,
            pinned_channels: None,
            is_program_bus: false,
            layer_exists: None,
            coerce: false,
        }
    }
}

/// Classify a crosspoint take's #11 class by inspecting the **destination's
/// pinned params** against the **source's** resolved stream (ADR-0034 §10).
///
/// This is honest at the edges — see the module table. The `inventory` is the
/// source input's [`StreamInventory`] (used to read the source audio layout for
/// the discrete-track comparison); `dest` is the destination's pinned profile.
#[must_use]
pub fn classify(
    request: &RouteRequest,
    inventory: &StreamInventory,
    dest: &DestinationProfile,
) -> RoutePlan {
    match &request.target {
        RouteTarget::VideoCell { .. } => RoutePlan {
            // An existing/primed cell is a hot scene-graph swap (Class-1); a cold
            // target needs a single IDR to spin up (Reset-lite).
            class: if dest.video_primed == Some(false) {
                RouteClass::ResetLite
            } else {
                RouteClass::Class1
            },
            coerced: false,
        },
        RouteTarget::AudioProgramBus { .. } => RoutePlan {
            // The program bus resamples to its working layout: any source layout
            // is absorbed — always Class-1.
            class: RouteClass::Class1,
            coerced: false,
        },
        RouteTarget::AudioDiscreteTrack { .. } => {
            classify_audio_discrete(request, inventory, *dest)
        }
        RouteTarget::SubtitleLayer { .. } => RoutePlan {
            // An existing layer is a hot re-route (Class-1); a destination that
            // needs a new passthrough track set is Class-2 (set CRUD).
            class: if dest.layer_exists == Some(false) {
                RouteClass::Class2
            } else {
                RouteClass::Class1
            },
            coerced: false,
        },
    }
}

/// Classify an audio re-point onto a **discrete output track**: Class-1 when the
/// source layout matches the pinned layout (or the pin is unknown / it is the
/// program bus), Class-2 on a mismatch — or Class-1-with-degradation when the
/// operator confirms a coerced down/up-mix.
fn classify_audio_discrete(
    request: &RouteRequest,
    inventory: &StreamInventory,
    dest: DestinationProfile,
) -> RoutePlan {
    // The pinned channel layout the mux fixed for the session. Prefer the
    // destination profile; fall back to the channel count carried on the target
    // (for a destination not yet in the engine snapshot).
    let pinned = dest.pinned_channels.or(match &request.target {
        RouteTarget::AudioDiscreteTrack {
            pinned_channels, ..
        } => *pinned_channels,
        _ => None,
    });
    // The program-bus-equivalent absorbing case, or no pinned layout to compare:
    // cannot prove a mismatch, so treat as a hot re-route.
    let Some(pinned) = pinned else {
        return RoutePlan {
            class: RouteClass::Class1,
            coerced: false,
        };
    };
    if dest.is_program_bus {
        return RoutePlan {
            class: RouteClass::Class1,
            coerced: false,
        };
    }
    // Read the source's channel layout from the resolved stream.
    let source_channels = source_audio_channels(&request.source, inventory);
    match source_channels {
        // A known mismatch: the mux pinned the track layout for the session, so a
        // re-point that would change it is Class-2 — unless the operator confirmed
        // a coerced down/up-mix to the pinned layout (Class-1-with-degradation).
        Some(src) if src != pinned => {
            if dest.coerce {
                RoutePlan {
                    class: RouteClass::Class1,
                    coerced: true,
                }
            } else {
                RoutePlan {
                    class: RouteClass::Class2,
                    coerced: false,
                }
            }
        }
        // Layouts match (or the source layout is unknown): hot re-route.
        _ => RoutePlan {
            class: RouteClass::Class1,
            coerced: false,
        },
    }
}

/// The source audio stream's channel count, resolved against the inventory by the
/// `StreamRef`'s selector, or [`None`] when it does not resolve / is not audio.
fn source_audio_channels(source: &StreamRef, inventory: &StreamInventory) -> Option<u16> {
    if source.kind != StreamKind::Audio {
        return None;
    }
    let descriptor = multiview_engine::resolve_selector(inventory, source.kind, &source.selector)?;
    descriptor
        .detail
        .audio_layout()
        .map(|(channels, _)| channels)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;
    use multiview_core::stream::{StableStreamId, StreamDescriptor, StreamDetail};

    fn audio_inv(channels: u16) -> StreamInventory {
        StreamInventory::from_streams(vec![StreamDescriptor::new(
            StableStreamId::from_ts_pid(StreamKind::Audio, 300),
            StreamKind::Audio,
            "aac",
            StreamDetail::Audio {
                channels,
                sample_rate: 48_000,
            },
        )])
        .with_input_id("cam-b")
    }

    #[test]
    fn discrete_track_layout_mismatch_is_class2_not_class1() {
        let req = RouteRequest {
            target: RouteTarget::AudioDiscreteTrack {
                track: "t".to_owned(),
                pinned_channels: None,
            },
            source: StreamRef::best("cam-b", StreamKind::Audio),
        };
        let plan = classify(
            &req,
            &audio_inv(2),
            &DestinationProfile::audio_discrete_track(6),
        );
        assert_eq!(plan.class, RouteClass::Class2);
        assert!(!plan.coerced);
    }

    #[test]
    fn class_str_round_trips() {
        for (c, s) in [
            (RouteClass::Class1, "class1"),
            (RouteClass::ResetLite, "reset_lite"),
            (RouteClass::Class2, "class2"),
        ] {
            assert_eq!(c.as_str(), s);
            assert_eq!(serde_json::to_value(c).unwrap(), serde_json::json!(s));
        }
    }
}
