//! # multiview-overlay
//!
//! The overlay **model** and **layout math** for Multiview: a serializable,
//! ordered stack of overlay layers (text labels, clocks, audio meters, alert
//! cards, logos, lower-thirds, burned-in subtitles) plus the pure geometry that
//! resolves each layer into a backend-agnostic *draw list* the compositor can
//! consume.
//!
//! This crate is **pure Rust with no native dependency in the default build**.
//! Native text/subtitle rasterization (libass, `HarfBuzz`, `FriBidi` — ADR-R007)
//! lives behind the off-by-default [`libass`](#features) feature and in the
//! compositor crate; nothing here touches the GPU or a C library.
//!
//! ## What lives here
//!
//! - [`geometry`] — pure box/anchor layout math: a normalized rectangle plus a
//!   9-point [`Anchor`](geometry::Anchor) and edge
//!   [`Padding`](geometry::Padding) resolve to exact pixel coordinates.
//! - [`layer`] — the OBS-style serializable layer stack (ADR-R008): each
//!   [`OverlayLayer`] carries its
//!   [`LayerKind`](layer::LayerKind) + style, [`Target`](layer::Target) surface,
//!   anchored [`Placement`](layer::Placement), `z`, opacity,
//!   [`BlendMode`](layer::BlendMode), and visibility.
//! - [`alert`] — the must-never-fail [`AlertCard`](alert::AlertCard) state
//!   machine, driven only by the media clock and operator actions, never by a
//!   live input frame (overlays are input-decoupled, ADR-R008).
//! - [`resolve`] — turns an [`OverlayStack`] into a
//!   back-to-front [`DrawList`] of
//!   [`DrawQuad`]s: the portable premultiplied-RGBA quad list
//!   any compositor backend (wgpu / Metal / CUDA-NPP) consumes (ADR-R008).
//! - [`subtitle`] — pure SRT/WebVTT timed-text ingest into a time-indexed
//!   [`CueTrack`]; the compositor's text engine rasterizes the active cue.
//! - [`subtitle_route`] — the per-layer subtitle/caption **cue re-point**
//!   (RT-10a / ADR-0019): a [`SubtitleLayer`](subtitle_route::SubtitleLayer)
//!   holds an `Arc<dyn `[`CueSource`](subtitle_route::CueSource)`>` sampled per
//!   tick by `active_at(now)` and can be atomically re-pointed to a different
//!   source's cue stream (subtitle breakaway), with a hard-cut CLEAR at the seam.
//! - [`libass`] — the ASS/SSA capability gate: native libass styling behind the
//!   off-by-default `libass` feature, gracefully falling back to the SRT/VTT
//!   text path when it is not compiled in (ADR-R007).
//!
//! ## Operator surface (broadcast multiviewer, brief §2/§5)
//!
//! Pure render models for the professional-multiviewer operator surface, each
//! input-decoupled and driven only by the engine media clock or the tally
//! arbiter — never a live decoded frame:
//!
//! - [`tally`] — multi-region tally border (LH / RH / Text strips) resolved from
//!   [`multiview_core::tally::TallyState`], with brightness-scaled fills and a text
//!   label per lit region so the state reads beyond colour alone.
//! - [`umd`] — Under-Monitor Display labels whose text updates live (a revision
//!   bump that fires only on a visible change) without a layout reload.
//! - [`timer`] — count-up / count-down / down-then-up timers and round-robin
//!   page cycling, pure value-machines over an injected `MediaTime`.
//! - [`identify`] — the flash-a-tile IDENTIFY "find this tile" overlay, a square
//!   wave over the media clock with an accessibility text badge.
//!
//! ## Design invariants honoured
//!
//! - **Input-decoupled (ADR-R008):** the model and the alert state machine
//!   depend only on local state and a media clock — never on a decoded input
//!   frame — so the alert path is drawable when every input and the GPU are
//!   gone.
//! - **Premultiplied "over" is the default blend** (ADR-R008): a
//!   straight/premultiplied mismatch halos every antialiased edge.
//! - **Tagged serde unions** ([`LayerKind`](layer::LayerKind),
//!   [`Target`](layer::Target)) — never `untagged`.
//!
//! ## Features
//!
//! - `libass` *(off by default)* — opt-in native libass burn-in rasterizer
//!   wiring; adds the C toolchain. The default build needs no native library.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod alert;
pub mod caption_probe;
pub mod clock;
pub mod error;
pub mod geometry;
pub mod identify;
pub mod layer;
pub mod libass;
pub mod resolve;
pub mod safearea;
pub mod scopes;
pub mod subtitle;
pub mod subtitle_route;
pub mod tally;
pub mod timecode;
pub mod timer;
pub mod umd;

pub use error::{Error, Result};
pub use layer::{OverlayLayer, OverlayStack};
pub use libass::{AssCapability, SubtitleFallback};
pub use resolve::{CanvasSize, DrawList, DrawQuad};
pub use subtitle::{Cue, CueTrack, SubtitleFormat};
pub use subtitle_route::{CueSource, EmptyCueSource, SubtitleLayer};
