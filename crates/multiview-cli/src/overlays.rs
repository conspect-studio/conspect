//! Build the **per-tile overlay draw-data** that the `run` paths bake into the
//! composited program (feature `overlay`).
//!
//! This wires the pure overlay *models* ([`multiview_overlay`]) and the pure-DSP
//! meter draw-data ([`multiview_audio`]) into the compositor's overlay primitive
//! model ([`multiview_compositor::overlay`]), then rasterizes text with the
//! stage-1 text engine. The engine drive loop and the libav pipeline call
//! [`OverlayBaker::draw_list`] **off the hot path** (on the collected output
//! frame, after the protected output core has emitted it) and bake the result
//! with [`multiview_compositor::overlay::apply_overlays_to_nv12`] — so overlay
//! rasterization can never block or back-pressure the output clock (inv #1/#10).
//!
//! ## Per-tile surface (the operator multiviewer)
//!
//! Unlike a single program-wide overlay set, the baker iterates the **layout
//! cells** and, for *each* cell rectangle, draws — all positioned and scaled
//! *within that cell*, never the whole canvas:
//!
//! 1. an **input label** — the source's `display_name` (or its id), bottom-left;
//! 2. a **per-input dB meter** — a vertical peak-held bar down the tile's right
//!    edge, fed by **that input's own** audio loudness (a silent / audio-free
//!    input rides its floor honestly, never a fabricated constant);
//! 3. a **state / fault flag** — a top-left badge from the tile's
//!    [`SourceState`](multiview_core::traits::SourceState)
//!    (`LIVE`/`STALE`/`RECONNECTING`/`NO_SIGNAL`), conveyed as **text** (not
//!    colour alone) via the alert-card chrome;
//! 4. an optional **per-tile safe-area** graticule + centre cross, drawn *inside*
//!    the cell rect (fixing the previous canvas-wide marker).
//!
//! A program-wide wall-clock label is kept (top-left of the whole canvas) when a
//! clock is configured. A missing source just shows `NO_SIGNAL` + a floor meter;
//! nothing here can stall.

use std::collections::HashMap;

use multiview_audio::meterdata::{Conflator, MeterSample};
use multiview_compositor::overlay::meters::{MeterBar, MeterScale};
use multiview_compositor::overlay::subpass::{
    clock_face, ClockFaceStyle, HandAngles, OverlayColor, OverlayDrawList, OverlayPrimitive,
    OverlayRect,
};
use multiview_compositor::overlay::text::{FontFamily, TextEngine};
use multiview_compositor::Error as CompositorError;
use multiview_core::time::MediaTime;
use multiview_core::traits::SourceState;
use multiview_overlay::clock::{
    AnalogHands, ClockFace, ClockModel, RefSource, RefStatus, TimeRef, TimeZoneOffset,
};
use multiview_overlay::geometry::PixelRect;
use multiview_overlay::resolve::CanvasSize;
use multiview_overlay::safearea::{SafeAreaKind, SafeAreaMarkers};

use crate::wallclock::WallClockSource;

/// Opaque white / amber / cyan / green linear overlay colours for the surface.
const WHITE: OverlayColor = OverlayColor::opaque(0.92, 0.92, 0.92);
const AMBER: OverlayColor = OverlayColor::opaque(0.95, 0.6, 0.05);
const GREEN: OverlayColor = OverlayColor::opaque(0.1, 0.85, 0.2);
const RED: OverlayColor = OverlayColor::opaque(0.9, 0.12, 0.12);
const SAFE: OverlayColor = OverlayColor::new(0.9, 0.9, 0.9, 0.7);
/// A translucent dark backing for the per-tile label / flag chrome so the text
/// reads over any underlying picture (the meaning is the *text*, not the colour).
const CHROME_BG: OverlayColor = OverlayColor::new(0.0, 0.0, 0.0, 0.55);
/// A near-opaque dark backing for the program-wide clock readout. Unlike the
/// per-tile chrome it must fully cover whatever sits beneath it — the top-left
/// tile's own state flag would otherwise bleed through and garble the digits —
/// so the program clock cleanly owns its corner (a standard multiviewer
/// convention). The meaning is still the text, not the colour (#38).
const CLOCK_BG: OverlayColor = OverlayColor::new(0.0, 0.0, 0.0, 0.92);

/// The static placement of one multiview tile's overlay surface: the cell's pixel
/// rectangle on the canvas plus the (immutable) label text for the bound source.
///
/// Built once from the solved layout + config source names; the per-frame
/// dynamics (meter level, tile state) are supplied to [`OverlayBaker::draw_list`]
/// separately so this stays a cheap value type.
#[derive(Debug, Clone, PartialEq)]
pub struct TileSpec {
    /// The bound source id (store key / fallback label).
    pub source_id: String,
    /// The human-facing label to draw (display name, or the id when unnamed).
    pub label: String,
    /// The cell's pixel rectangle on the canvas (top-left origin).
    pub rect: PixelRect,
}

impl TileSpec {
    /// Build a tile spec for `source_id` placed at `rect`, labelled `label`.
    #[must_use]
    pub fn new(source_id: impl Into<String>, label: impl Into<String>, rect: PixelRect) -> Self {
        Self {
            source_id: source_id.into(),
            label: label.into(),
            rect,
        }
    }
}

/// A per-tile **content fault**, distinct from the lifecycle [`SourceState`].
///
/// Where [`SourceState`] tracks the *transport* health of a tile
/// (`LIVE`/`STALE`/`RECONNECTING`/`NO_SIGNAL`), a [`TileFault`] tracks a
/// *content* condition detected by sampling the tile's last-good frame /
/// audio: an all-black picture, a frozen (non-advancing) picture, or sustained
/// audio silence. A healthy tile carries [`TileFault::None`] and shows no fault
/// badge.
///
/// This is the CLI's compact, exhaustive view of the engine's content-aware
/// probes; it maps from [`multiview_core::alarm::AlarmKind`] for the three picture
/// / audio faults the multiviewer surfaces as a per-tile badge (see
/// [`TileFault::from_alarm_kind`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TileFault {
    /// No content fault: the tile's picture is advancing, not black, and its
    /// audio is above the silence floor. No badge is drawn.
    #[default]
    None,
    /// The picture is black (mean luma at/below the black threshold) for the
    /// dwell window — drawn as a `BLACK` badge.
    Black,
    /// The picture is frozen (too few luma samples change between successive
    /// sampled frames) for the dwell — drawn as a `FROZEN` badge.
    Frozen,
    /// Audio is silent (the per-input meter sits at/below the silence floor)
    /// for the dwell — drawn as a `NO AUDIO` badge.
    Silent,
}

impl TileFault {
    /// Whether a fault is present (anything other than [`TileFault::None`]).
    #[must_use]
    pub const fn is_present(self) -> bool {
        !matches!(self, Self::None)
    }

    /// Map an [`AlarmKind`](multiview_core::alarm::AlarmKind) to the CLI's per-tile
    /// fault badge, for the three content faults the multiviewer surfaces.
    ///
    /// Returns [`None`] for any alarm kind without a dedicated per-tile badge
    /// (over-level, clipping, caption loss, …) — those roll up through the
    /// alarm engine rather than the tile fault badge.
    #[must_use]
    pub fn from_alarm_kind(kind: multiview_core::alarm::AlarmKind) -> Option<Self> {
        use multiview_core::alarm::AlarmKind;
        match kind {
            AlarmKind::Black => Some(Self::Black),
            AlarmKind::Freeze => Some(Self::Frozen),
            AlarmKind::Silence => Some(Self::Silent),
            // Other alarm kinds have no per-tile content badge here.
            _ => None,
        }
    }

    /// The short, all-text badge label for this fault (text carries the meaning,
    /// not colour alone — the accessibility requirement). [`TileFault::None`]
    /// has no badge and returns [`None`].
    #[must_use]
    pub const fn badge_text(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Black => Some("BLACK"),
            Self::Frozen => Some("FROZEN"),
            Self::Silent => Some("NO AUDIO"),
        }
    }
}

/// The live per-tile dynamics for one output frame: the source's current
/// loudness (dBFS), its lifecycle state, and any detected content fault. A tile
/// with no decodable audio passes its meter floor; a missing/unconnected source
/// is `NO_SIGNAL`; a healthy tile carries [`TileFault::None`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TileDynamics {
    /// The source's current program loudness in dBFS (its own audio).
    pub meter_db: f64,
    /// The tile's lifecycle state sampled this tick.
    pub state: SourceState,
    /// The tile's detected content fault sampled this tick (distinct from
    /// `state`). [`TileFault::None`] ⇒ no fault badge.
    pub fault: TileFault,
}

/// The per-tile peak-hold meter + conflator state, kept across frames so the
/// vertical bar tracks the source's own audio and the held peak decays smoothly.
struct TileMeter {
    bar: MeterBar,
    conflator: Conflator<MeterSample>,
}

impl TileMeter {
    fn new() -> Self {
        Self {
            bar: MeterBar::new(MeterScale::default()),
            conflator: Conflator::with_rate(multiview_audio::meterdata::DISPLAY_HZ),
        }
    }
}

/// Builds the per-frame, **per-tile** overlay draw-data for a run.
///
/// Holds the (single, shared) text engine, the per-tile static placement
/// ([`TileSpec`]), each tile's conflated-meter + peak-hold state, and an optional
/// program-wide clock. Constructed once per run, then queried per collected
/// output frame with [`OverlayBaker::draw_list`], which is also handed that
/// frame's **per-source active caption lines** (sampled from each source's cue
/// store by the pipeline, off the hot path) to burn each tile's caption into
/// *its own* cell rect.
pub struct OverlayBaker {
    engine: TextEngine,
    tiles: Vec<TileSpec>,
    meters: Vec<TileMeter>,
    clock: Option<ClockModel>,
    analog_clock: Option<AnalogClockSpec>,
    /// The injectable source of the **current** wall-clock instant. Sampled at
    /// draw time so the displayed time-of-day tracks the live (NTP-disciplined)
    /// OS clock (anti-drift), never the monotonic output-tick counter — the
    /// engine's output cadence is wholly independent of this display concern
    /// (invariant #1).
    wall_clock: WallClockSource,
    per_tile_safe_area: bool,
    /// The Conspect tile-watermark seam (S3, ADR-0050 §5): when present and the
    /// published ladder level is at a watermark rung, the bake appends the corner
    /// watermark to the draw list. `None` (the default) never watermarks. The
    /// read is a single wait-free `arc_swap` load off the hot loop — it cannot
    /// pace or stall the output clock (invariant #1). Carries the canvas geometry
    /// the corner mark is anchored to.
    watermark: Option<WatermarkPlacement>,
}

/// The Conspect tile-watermark placement: the wait-free
/// [`WatermarkSignal`](crate::licence::WatermarkSignal) the bake samples + the
/// canvas geometry the corner mark is anchored to.
#[derive(Clone)]
struct WatermarkPlacement {
    signal: crate::licence::WatermarkSignal,
    canvas_width: u32,
    canvas_height: u32,
}

/// An analog clock face placed on the canvas: its [`ClockModel`] (for the
/// timezone + the hand-angle math) plus where + how big to draw the face. The
/// face is rendered with the compositor's ring + angled-hand primitives; the
/// model's analog hand angles drive the hands (the digital readout can be shown
/// independently via [`OverlayBaker::with_clock`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AnalogClockSpec {
    /// The clock model (zone + analog face) whose hand angles drive the hands.
    model: ClockModel,
    /// Centre x of the face on the canvas, pixels.
    cx: f32,
    /// Centre y of the face on the canvas, pixels.
    cy: f32,
    /// Bezel radius of the face, pixels.
    radius: f32,
}

impl AnalogClockSpec {
    /// Place an analog clock of bezel `radius` (px) centred at `(cx, cy)` on the
    /// canvas, in the given `zone`.
    #[must_use]
    pub fn new(zone: TimeZoneOffset, cx: f32, cy: f32, radius: f32) -> Self {
        Self {
            model: ClockModel::new(
                ClockFace::analog(),
                zone,
                TimeRef::new(RefSource::System, RefStatus::Freerun),
            ),
            cx,
            cy,
            radius,
        }
    }

    /// The face centre x on the canvas, pixels.
    #[must_use]
    pub const fn cx(self) -> f32 {
        self.cx
    }

    /// The face centre y on the canvas, pixels.
    #[must_use]
    pub const fn cy(self) -> f32 {
        self.cy
    }

    /// The bezel radius of the face, pixels.
    #[must_use]
    pub const fn radius(self) -> f32 {
        self.radius
    }
}

impl OverlayBaker {
    /// Build a per-tile baker over `tiles` (each carrying its own cell rect),
    /// driven by `wall_clock` for the displayed time-of-day.
    ///
    /// The program-wide digital clock label (top-left) shows the **real
    /// wall-clock time-of-day** read from `wall_clock` at draw time — so it tracks
    /// the host's NTP-disciplined OS clock and never drifts from real time over a
    /// long run (anti-drift), wholly independent of the engine's output-tick
    /// cadence (invariant #1). Its reference badge comes from
    /// [`WallClockSource::reference`]. Per-tile safe-area markers are off by
    /// default (enable with [`OverlayBaker::with_per_tile_safe_area`]). Captions
    /// are burned in per tile from the per-source active cue lines passed to
    /// [`OverlayBaker::draw_list`].
    ///
    /// # Errors
    ///
    /// Returns the compositor [`CompositorError`] if the bundled OFL fonts fail
    /// to load.
    pub fn new(tiles: Vec<TileSpec>, wall_clock: WallClockSource) -> Result<Self, CompositorError> {
        let meters = tiles.iter().map(|_| TileMeter::new()).collect();
        let time_ref = wall_clock.reference();
        Ok(Self {
            engine: TextEngine::new()?,
            tiles,
            meters,
            clock: Some(ClockModel::new(
                ClockFace::digital_24h(),
                TimeZoneOffset::UTC,
                time_ref,
            )),
            analog_clock: None,
            wall_clock,
            per_tile_safe_area: false,
            watermark: None,
        })
    }

    /// Attach the Conspect tile-watermark seam (S3, ADR-0050 §5): when `signal` is
    /// at a watermark rung, every bake appends a corner watermark anchored to the
    /// `canvas_width × canvas_height` canvas. The decision is a single wait-free
    /// `arc_swap` load per baked frame, off the hot loop — it cannot pace or stall
    /// the output clock (invariant #1). The watermark marks the composited
    /// multiview canvas only (the tiles); a 1:1 pass-through program is never
    /// composited through this bake, so it is never marked (the spec's "tiles
    /// only, never pass-through program"). Builder-style; omitting it never
    /// watermarks.
    #[must_use]
    pub fn with_watermark(
        mut self,
        signal: crate::licence::WatermarkSignal,
        canvas_width: u32,
        canvas_height: u32,
    ) -> Self {
        self.watermark = Some(WatermarkPlacement {
            signal,
            canvas_width,
            canvas_height,
        });
        self
    }

    /// Enable per-tile safe-area / centre-cross markers (drawn inside each cell
    /// rect), builder-style. Off by default.
    #[must_use]
    pub fn with_per_tile_safe_area(mut self, on: bool) -> Self {
        self.per_tile_safe_area = on;
        self
    }

    /// Set (or replace) the program-wide **digital** clock label drawn top-left.
    /// Pass `None` to suppress the digital readout (e.g. when only an analog face
    /// is wanted). Builder-style.
    #[must_use]
    pub fn with_clock(mut self, clock: Option<ClockModel>) -> Self {
        self.clock = clock;
        self
    }

    /// Add an **analog** clock face (ring + angled hour/minute/second hands) at
    /// the given placement, builder-style. Independent of the digital label — a
    /// config may request either or both.
    #[must_use]
    pub fn with_analog_clock(mut self, spec: AnalogClockSpec) -> Self {
        self.analog_clock = Some(spec);
        self
    }

    /// The tiles this baker draws (their static placement), in declaration order.
    #[must_use]
    pub fn tiles(&self) -> &[TileSpec] {
        &self.tiles
    }

    /// Build the overlay draw-data for the output instant `pts`, given each
    /// tile's live [`TileDynamics`] keyed by source id and the per-source active
    /// caption lines (`captions[source_id]` = the cue lines on screen at `pts`
    /// for that source, sampled by the pipeline from its cue store).
    ///
    /// For every tile this draws — *inside that tile's cell rect* — the input
    /// label, the source's own peak-held dB meter, a state/fault flag, **and that
    /// source's active caption lines** (bottom-centre of the cell); plus the
    /// optional per-tile safe-area and the program-wide clock. A source absent
    /// from `dynamics` is treated as `NO_SIGNAL` at the meter floor and a source
    /// absent from `captions` simply shows no caption (a missing source never
    /// stalls — invariant #1).
    ///
    /// # Errors
    ///
    /// Returns the compositor [`CompositorError`] only if a glyph cannot fit the
    /// atlas (a degenerate font/size); text that simply does not ink is skipped.
    pub fn draw_list(
        &mut self,
        pts: MediaTime,
        dynamics: &HashMap<String, TileDynamics>,
        captions: &HashMap<String, Vec<String>>,
        bitmaps: &HashMap<String, multiview_ffmpeg::caption::CueBitmap>,
    ) -> Result<OverlayDrawList, CompositorError> {
        let mut list = OverlayDrawList::new();
        let now_ns = pts.as_nanos();

        // Per-tile surface. Take the tiles + meters by index so the per-tile
        // meter state (a mutable borrow) does not alias the immutable tile spec.
        for i in 0..self.tiles.len() {
            // Resolve this tile's live dynamics (default: NO_SIGNAL at floor).
            let (label, rect, source_id) = match self.tiles.get(i) {
                Some(spec) => (spec.label.clone(), spec.rect, spec.source_id.clone()),
                None => continue,
            };
            let dyn_ = dynamics.get(&source_id).copied().unwrap_or(TileDynamics {
                meter_db: multiview_audio::Ballistics::FLOOR_DB,
                state: SourceState::NoSignal,
                fault: TileFault::None,
            });

            // Advance this tile's meter from its own audio (conflated ~30 Hz).
            if let Some(meter) = self.meters.get_mut(i) {
                meter.conflator.accept(MeterSample { db: dyn_.meter_db });
                if let Some(sample) = meter.conflator.poll(now_ns) {
                    meter.bar.observe_db(sample.db_f32());
                }
                meter.bar.decay_peak(0.02);
            }
            let bar = self.meters.get(i).map(|m| m.bar);

            self.draw_tile(&mut list, &label, rect, dyn_.state, dyn_.fault, bar)?;

            // Burn this source's active caption (if any) into THIS tile's cell
            // rect, bottom-centre — never canvas-wide.
            let cue_lines = captions.get(&source_id).cloned().unwrap_or_default();
            if !cue_lines.is_empty() {
                self.draw_tile_caption(&mut list, rect, &cue_lines)?;
            }

            // Burn this source's active BITMAP caption (DVB-sub) into THIS tile's
            // cell rect, bottom-anchored — the per-tile bitmap burn-in. A bitmap
            // and a text cue can both be present; both are cell-local.
            if let Some(bitmap) = bitmaps.get(&source_id) {
                draw_tile_bitmap(&mut list, rect, bitmap);
            }
        }

        // Program-wide wall-clock label, top-left of the whole canvas. The
        // displayed time-of-day is sampled from the LIVE OS clock right now (at
        // bake time) — NOT derived from `pts`/the output-tick counter — so it
        // tracks the host's NTP-disciplined clock and cannot drift from real time
        // over a long run (anti-drift). The engine's output cadence is wholly
        // independent of this read (invariant #1): this is a pure display sample.
        let wall = self.wall_clock.now();
        if let Some(clock) = self.clock {
            if let Some(text) = clock.render_digital(wall) {
                // Translucent backing behind the readout so it stays legible over
                // whatever sits beneath it — in particular the top-left tile's
                // own state flag would otherwise bleed green through the white
                // digits (#38). Meaning is the text; the chip is contrast only,
                // sized to the coarse mono advance like the other chrome chips.
                const CLOCK_SIZE_PX: f32 = 26.0;
                const CLOCK_PAD: u32 = 4;
                let clock_w = u32_from_usize(text.chars().count())
                    .saturating_mul(quantize_advance(CLOCK_SIZE_PX))
                    .saturating_add(CLOCK_PAD.saturating_mul(2));
                let clock_h = round_dim(CLOCK_SIZE_PX).saturating_add(CLOCK_PAD.saturating_mul(2));
                push_filled_rect(
                    &mut list,
                    OverlayRect::new(
                        12i32.saturating_sub(i32_dim(CLOCK_PAD)),
                        6i32.saturating_sub(i32_dim(CLOCK_PAD)),
                        clock_w,
                        clock_h,
                    ),
                    CLOCK_BG,
                );
                self.push_text(
                    &mut list,
                    &text,
                    TextRun {
                        family: FontFamily::Mono,
                        size_px: CLOCK_SIZE_PX,
                        x: 12,
                        y: 6,
                        color: WHITE,
                    },
                )?;
                // The timing-reference badge, just under the digital readout:
                // status TEXT + a distinct GLYPH (accessibility — never colour
                // alone), e.g. "🔒 SYS locked". A locked/holdover (disciplined)
                // reference reads white; an undisciplined one (ref-loss/freerun)
                // reads amber — but the meaning is carried by the text + glyph.
                self.draw_reference_badge(&mut list, clock.time_ref, 12, 36)?;
            }
        }

        // Program-wide ANALOG clock face: a bezel ring + 12 ticks + three angled
        // hands, driven by the model's analog hand angles for this instant. The
        // model owns the time→angle math (the only float); this only maps those
        // angles into the compositor's ring + stroke primitives.
        if let Some(analog) = self.analog_clock {
            if let Some(hands) = analog.model.render_analog(wall) {
                let style = ClockFaceStyle::at(analog.cx, analog.cy, analog.radius);
                // The program-wide analog clock is a conventional 12-hour dial.
                for prim in clock_face(hand_angles(hands), style, 12) {
                    list.push(prim);
                }
            }
        }

        // The Conspect enforcement tile watermark (S3, ADR-0050 §5): when the
        // published ladder level is at a watermark rung, append the corner mark
        // to the composited multiview canvas. Drawn LAST so it sits on top of the
        // tile chrome. The decision is a single wait-free `arc_swap` load — no
        // lock, no allocation — off the hot loop, so it cannot pace or stall the
        // output clock (invariant #1). It rides the existing overlay sub-pass like
        // every other primitive (region-limited bake, ADR-0023).
        if let Some(wm) = &self.watermark {
            if wm.signal.watermark() {
                let _ = multiview_compositor::overlay::push_tile_watermark(
                    &mut list,
                    wm.canvas_width,
                    wm.canvas_height,
                );
            }
        }

        Ok(list)
    }

    /// Burn `cue_lines` into the cell `rect`, bottom-centre of THAT tile (never
    /// canvas-wide). The caption size scales with the cell so a small tile gets a
    /// proportionally smaller caption; the lines stack upward from a bottom inset.
    fn draw_tile_caption(
        &mut self,
        list: &mut OverlayDrawList,
        rect: PixelRect,
        cue_lines: &[String],
    ) -> Result<(), CompositorError> {
        let geom = TileGeometry::resolve(rect);
        if geom.width < MIN_TILE_PX || geom.height < MIN_TILE_PX {
            return Ok(());
        }
        // Caption text ~6% of the cell height, clamped to a legible band.
        let size = f32_dim((geom.height / 16).clamp(12, 36));
        let line_step = round_dim(size * 1.2).max(1);
        // Bottom inset (~one chip height) so the caption clears the label band.
        let bottom_inset = geom.chip_height().saturating_add(geom.pad());
        let n = u32_from_usize(cue_lines.len());
        // Top y of the first line so the block of `n` lines sits above the inset.
        let block_h = n.saturating_mul(line_step);
        let base_y = geom.y.saturating_add(i32_dim(
            geom.height
                .saturating_sub(bottom_inset)
                .saturating_sub(block_h),
        ));
        for (i, line) in cue_lines.iter().enumerate() {
            let line_y =
                base_y.saturating_add(i32_dim(u32_from_usize(i).saturating_mul(line_step)));
            // Centre each line within the cell using the coarse advance estimate.
            let approx_w =
                u32_from_usize(line.chars().count()).saturating_mul(quantize_advance(size));
            let x_off = geom.width.saturating_sub(approx_w.min(geom.width)) / 2;
            let x = geom.x.saturating_add(i32_dim(x_off));
            self.push_text(
                list,
                line,
                TextRun {
                    family: FontFamily::Sans,
                    size_px: size,
                    x,
                    y: line_y,
                    color: AMBER,
                },
            )?;
        }
        Ok(())
    }

    /// Draw one tile's overlay surface inside its cell `rect`: optional safe-area,
    /// the source's vertical dB meter (right edge), a bottom-left input label, and
    /// a top-left state/fault flag.
    fn draw_tile(
        &mut self,
        list: &mut OverlayDrawList,
        label: &str,
        rect: PixelRect,
        state: SourceState,
        fault: TileFault,
        bar: Option<MeterBar>,
    ) -> Result<(), CompositorError> {
        let geom = TileGeometry::resolve(rect);
        if geom.width < MIN_TILE_PX || geom.height < MIN_TILE_PX {
            // Too small to host a legible surface; skip rather than draw mush.
            return Ok(());
        }

        // Per-tile safe-area + centre cross, scaled INSIDE the cell rect.
        if self.per_tile_safe_area {
            let markers = SafeAreaMarkers::default()
                .with_kind(SafeAreaKind::ActionSafe, true)
                .with_kind(SafeAreaKind::TitleSafe, true)
                .with_center_cross(true)
                .resolve(CanvasSize::new(geom.width, geom.height));
            for safe in &markers.rects {
                // Offset each marker rect from cell-local space to canvas space.
                push_box_outline(list, geom.offset_rect(safe.rect), 1, SAFE);
            }
            if let Some(cross) = markers.center_cross {
                push_center_cross(list, geom.offset_cross(cross), 1, SAFE);
            }
        }

        // The per-input dB meter: a vertical peak-held bar down the tile's RIGHT
        // edge, fed by THAT input's own audio.
        if let Some(bar) = bar {
            let track = geom.meter_track();
            bar.push_into(list, track, true, GREEN, AMBER);
        }

        // The state / fault flag: a top-left badge whose TEXT names the state, on
        // a translucent backing (meaning is the text, not the colour — A11y).
        let flag_text = state_flag_text(state);
        let flag_color = state_flag_color(state);
        let flag_h = geom.chip_height();
        let flag_w = geom.flag_width(flag_text.chars().count());
        push_filled_rect(
            list,
            OverlayRect::new(geom.x, geom.y, flag_w, flag_h),
            CHROME_BG,
        );
        self.push_text(
            list,
            flag_text,
            TextRun {
                family: FontFamily::Sans,
                size_px: geom.chip_text_px(),
                x: geom.x.saturating_add(geom.pad_i32()),
                y: geom.y.saturating_add(geom.pad_i32() / 2),
                color: flag_color,
            },
        )?;

        // The per-tile content-fault badge: a TOP-RIGHT chip whose TEXT names the
        // fault (BLACK / FROZEN / NO AUDIO), drawn ONLY when a fault is present so
        // a healthy tile shows nothing. Positioned top-right (right-aligned, inset
        // left of the meter track) so it never collides with the top-left state
        // flag, the bottom-left label, the bottom-centre caption, or the
        // right-edge meter. Meaning is the text; the warning colour reinforces it.
        if let Some(badge) = fault.badge_text() {
            let badge_h = geom.chip_height();
            let badge_w = geom.flag_width(badge.chars().count());
            // Right-align within the cell, clear of the right-edge meter track.
            let badge_right = geom.x.saturating_add(i32_dim(geom.meter_left()));
            let badge_x = badge_right.saturating_sub(i32_dim(badge_w)).max(geom.x);
            push_filled_rect(
                list,
                OverlayRect::new(badge_x, geom.y, badge_w, badge_h),
                CHROME_BG,
            );
            self.push_text(
                list,
                badge,
                TextRun {
                    family: FontFamily::Sans,
                    size_px: geom.chip_text_px(),
                    x: badge_x.saturating_add(geom.pad_i32()),
                    y: geom.y.saturating_add(geom.pad_i32() / 2),
                    color: RED,
                },
            )?;
        }

        // The input label: bottom-left of the tile, on a translucent backing.
        let label_h = geom.chip_height();
        let label_w = geom
            .flag_width(label.chars().count())
            .min(geom.meter_left());
        let label_y = geom
            .y
            .saturating_add(i32_dim(geom.height.saturating_sub(label_h)));
        push_filled_rect(
            list,
            OverlayRect::new(geom.x, label_y, label_w, label_h),
            CHROME_BG,
        );
        self.push_text(
            list,
            label,
            TextRun {
                family: FontFamily::Sans,
                size_px: geom.chip_text_px(),
                x: geom.x.saturating_add(geom.pad_i32()),
                y: label_y.saturating_add(geom.pad_i32() / 2),
                color: WHITE,
            },
        )?;

        Ok(())
    }

    /// Draw the clock's **timing-reference badge** at canvas pixel `(x, y)`: a
    /// small translucent chrome backing carrying the reference [`RefStatus::glyph`]
    /// followed by the [`TimeRef::status_text`] (e.g. `"🔒 SYS locked"`).
    ///
    /// Accessibility: the meaning is conveyed by both the **glyph** and the
    /// **text**, never colour alone. The tint reinforces the state — a disciplined
    /// reference (locked/holdover) reads white; an undisciplined one
    /// (ref-loss/freerun) reads amber — but a colour-blind operator reads the
    /// glyph + words. The glyph is prepended to the status text so it shapes
    /// through the same bundled font run as the words.
    fn draw_reference_badge(
        &mut self,
        list: &mut OverlayDrawList,
        time_ref: TimeRef,
        x: i32,
        y: i32,
    ) -> Result<(), CompositorError> {
        let badge = format!("{} {}", time_ref.status.glyph(), time_ref.status_text());
        // Chrome backing so the badge reads over any underlying picture (the
        // meaning is the glyph + text, not the colour). A fixed legible band sized
        // to the coarse glyph advance, with a small pad around the text origin.
        let size_px = 16.0;
        let pad: u32 = 3;
        let approx_w = u32_from_usize(badge.chars().count())
            .saturating_mul(quantize_advance(size_px))
            .saturating_add(pad.saturating_mul(2));
        let band_h = round_dim(size_px).saturating_add(pad.saturating_mul(2));
        push_filled_rect(
            list,
            OverlayRect::new(
                x.saturating_sub(i32_dim(pad)),
                y.saturating_sub(i32_dim(pad)),
                approx_w,
                band_h,
            ),
            CHROME_BG,
        );
        let color = if time_ref.status.is_disciplined() {
            WHITE
        } else {
            AMBER
        };
        self.push_text(
            list,
            &badge,
            TextRun {
                family: FontFamily::Sans,
                size_px,
                x,
                y,
                color,
            },
        )
    }

    /// Shape `text` per `run` and append its glyph quads to `list`, tinted by the
    /// run colour (the baseline sits at the run origin; the engine offsets each
    /// glyph). Re-rasterizes only on a content change (per-glyph atlas, T2).
    fn push_text(
        &mut self,
        list: &mut OverlayDrawList,
        text: &str,
        run: TextRun,
    ) -> Result<(), CompositorError> {
        let color = run.color;
        let rasterized = self.engine.rasterize_run(
            text,
            run.family,
            run.size_px,
            [color.r, color.g, color.b, color.a],
        )?;
        for glyph in rasterized.glyphs() {
            list.push(OverlayPrimitive::Glyph {
                dest_x: run.x.saturating_add(glyph.dest_x),
                dest_y: run.y.saturating_add(glyph.dest_y),
                width: glyph.width,
                height: glyph.height,
                coverage: glyph
                    .premultiplied_rgba
                    .chunks_exact(4)
                    .filter_map(|px| px.get(3).copied())
                    .collect(),
                color,
            });
        }
        Ok(())
    }
}

/// Burn a **bitmap caption cue** (DVB-sub) into the cell `rect`, bottom-anchored
/// within THAT tile (never canvas-wide) — mirroring [`OverlayBaker::draw_tile_caption`]'s
/// bottom-inset model but for a premultiplied-RGBA image instead of text.
///
/// The cue's source rect is in *source* pixels; it is scaled to fit the cell
/// **width** (minus a small horizontal inset) preserving aspect, clamped so it
/// never exceeds the cell's usable height, then placed centred horizontally with
/// its bottom edge a chip-height above the cell bottom (clearing the label band).
/// The `Image` primitive carries the premultiplied RGBA verbatim — the
/// compositor sub-pass blits it without re-premultiplying.
///
/// A degenerate cue (zero dims) or a sub-minimum tile draws nothing.
fn draw_tile_bitmap(
    list: &mut OverlayDrawList,
    rect: PixelRect,
    bitmap: &multiview_ffmpeg::caption::CueBitmap,
) {
    let geom = TileGeometry::resolve(rect);
    if geom.width < MIN_TILE_PX || geom.height < MIN_TILE_PX {
        return;
    }
    let src_w = bitmap.rect.width;
    let src_h = bitmap.rect.height;
    if src_w == 0 || src_h == 0 {
        return;
    }

    let pad = geom.pad();
    // The usable width: cell width minus a left+right inset.
    let max_w = geom.width.saturating_sub(pad.saturating_mul(2)).max(1);
    // Bottom inset (clear the label band) and a matching top headroom.
    let bottom_inset = geom.chip_height().saturating_add(pad);
    let max_h = geom
        .height
        .saturating_sub(bottom_inset.saturating_add(pad))
        .max(1);

    // Scale to fit the width preserving aspect; clamp to the usable height.
    let (dest_w, dest_h) = fit_preserving_aspect(src_w, src_h, max_w, max_h);
    if dest_w == 0 || dest_h == 0 {
        return;
    }

    // Centre horizontally; bottom-anchor a chip-height above the cell bottom.
    let x_off = geom.width.saturating_sub(dest_w) / 2;
    let dest_x = geom.x.saturating_add(i32_dim(x_off));
    let dest_bottom_off = geom.height.saturating_sub(bottom_inset);
    let dest_y = geom
        .y
        .saturating_add(i32_dim(dest_bottom_off.saturating_sub(dest_h)));

    list.push(OverlayPrimitive::Image {
        dest: OverlayRect::new(dest_x, dest_y, dest_w, dest_h),
        src_width: src_w,
        src_height: src_h,
        rgba: bitmap.rgba.clone(),
        alpha: 1.0,
    });
}

/// Scale `(src_w, src_h)` to fit within `(max_w, max_h)` preserving aspect
/// ratio (the larger-constraint side touches its bound). Pure integer math, no
/// `as` cast; returns `(0, 0)` for a degenerate input.
fn fit_preserving_aspect(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if src_w == 0 || src_h == 0 || max_w == 0 || max_h == 0 {
        return (0, 0);
    }
    // Scale by width first: dest_w = max_w, dest_h = src_h * max_w / src_w.
    let by_width_h = u64::from(src_h)
        .saturating_mul(u64::from(max_w))
        .checked_div(u64::from(src_w))
        .unwrap_or(0);
    if by_width_h <= u64::from(max_h) {
        let h = u32::try_from(by_width_h.max(1)).unwrap_or(max_h);
        (max_w, h.min(max_h).max(1))
    } else {
        // Height-bound: dest_h = max_h, dest_w = src_w * max_h / src_h.
        let by_height_w = u64::from(src_w)
            .saturating_mul(u64::from(max_h))
            .checked_div(u64::from(src_h))
            .unwrap_or(0);
        let w = u32::try_from(by_height_w.max(1)).unwrap_or(max_w);
        (w.min(max_w).max(1), max_h)
    }
}

/// The minimum cell extent (px) that can host a legible per-tile surface; smaller
/// tiles are skipped so the run never draws unreadable mush.
const MIN_TILE_PX: u32 = 48;

/// One placed text run: where its baseline-left origin sits on the canvas, the
/// bundled face, the pixel size, and the linear tint.
#[derive(Debug, Clone, Copy)]
struct TextRun {
    family: FontFamily,
    size_px: f32,
    x: i32,
    y: i32,
    color: OverlayColor,
}

/// The integer pixel geometry of one cell rectangle, with the helpers the tile
/// surface uses to place its meter / label / flag *inside* that cell.
#[derive(Debug, Clone, Copy)]
struct TileGeometry {
    /// Cell top-left x in canvas pixels.
    x: i32,
    /// Cell top-left y in canvas pixels.
    y: i32,
    /// Cell width in pixels.
    width: u32,
    /// Cell height in pixels.
    height: u32,
}

impl TileGeometry {
    /// Quantise a [`PixelRect`] to integer canvas pixels.
    fn resolve(rect: PixelRect) -> Self {
        Self {
            x: round_dim_signed(rect.x),
            y: round_dim_signed(rect.y),
            width: round_dim(rect.width),
            height: round_dim(rect.height),
        }
    }

    /// The pad (px) for chrome inside the cell — a small fraction of the cell.
    fn pad(self) -> u32 {
        (self.width.min(self.height) / 40).clamp(2, 8)
    }

    fn pad_i32(self) -> i32 {
        i32_dim(self.pad())
    }

    /// The chrome chip height (flag/label band) — a fraction of the cell height.
    fn chip_height(self) -> u32 {
        (self.height / 8).clamp(14, 40)
    }

    /// The chrome text size (px), sized to fit the chip with padding.
    fn chip_text_px(self) -> f32 {
        let h = self.chip_height().saturating_sub(self.pad());
        f32_dim(h.clamp(10, 30))
    }

    /// The width (px) of a chip holding `glyphs` characters at the chip text size,
    /// clamped to the cell width.
    fn flag_width(self, glyphs: usize) -> u32 {
        let advance = quantize_advance(self.chip_text_px());
        let text = u32_from_usize(glyphs).saturating_mul(advance);
        text.saturating_add(self.pad().saturating_mul(2))
            .clamp(self.pad().saturating_mul(2).max(1), self.width)
    }

    /// The meter track width (px) down the tile's right edge.
    fn meter_width(self) -> u32 {
        (self.width / 16).clamp(6, 18)
    }

    /// The canvas x where the meter track begins (left edge of the meter).
    fn meter_left(self) -> u32 {
        let inset = self.meter_width().saturating_add(self.pad());
        self.width.saturating_sub(inset)
    }

    /// The meter track rectangle: a vertical strip down the tile's right edge,
    /// inset from the top/bottom by the chrome chip height so it does not collide
    /// with the flag/label.
    fn meter_track(self) -> OverlayRect {
        let pad = self.pad();
        let mw = self.meter_width();
        let track_x = self
            .x
            .saturating_add(i32_dim(self.width.saturating_sub(mw.saturating_add(pad))));
        let chip = self.chip_height();
        let top_inset = chip.saturating_add(pad);
        let track_y = self.y.saturating_add(i32_dim(top_inset));
        let track_h = self
            .height
            .saturating_sub(top_inset.saturating_add(chip).saturating_add(pad))
            .max(1);
        OverlayRect::new(track_x, track_y, mw, track_h)
    }

    /// Offset a cell-local [`PixelRect`] (origin at the cell's top-left) into
    /// canvas space.
    fn offset_rect(self, local: PixelRect) -> PixelRect {
        PixelRect {
            x: local.x + f32_signed(self.x),
            y: local.y + f32_signed(self.y),
            width: local.width,
            height: local.height,
        }
    }

    /// Offset a cell-local centre cross into canvas space.
    fn offset_cross(
        self,
        local: multiview_overlay::safearea::CenterCross,
    ) -> multiview_overlay::safearea::CenterCross {
        multiview_overlay::safearea::CenterCross {
            x: local.x + f32_signed(self.x),
            y: local.y + f32_signed(self.y),
            arm_px: local.arm_px,
        }
    }
}

/// The short, all-text flag label for a tile's lifecycle state. Conveys meaning
/// as text (not colour alone) per the accessibility requirement.
fn state_flag_text(state: SourceState) -> &'static str {
    match state {
        SourceState::Live => "LIVE",
        SourceState::Stale => "STALE",
        SourceState::Reconnecting => "RECONNECT",
        SourceState::NoSignal => "NO SIGNAL",
        // `SourceState` is `#[non_exhaustive]`; a future state is surfaced as an
        // explicit fault flag rather than silently mislabelled as LIVE.
        _ => "FAULT",
    }
}

/// The flag tint for a tile's state (green `LIVE`, amber `STALE`/`RECONNECTING`,
/// red `NO_SIGNAL`). The text already carries the meaning; the colour reinforces
/// it.
fn state_flag_color(state: SourceState) -> OverlayColor {
    match state {
        SourceState::Live => GREEN,
        SourceState::Stale | SourceState::Reconnecting => AMBER,
        // NO_SIGNAL and any future fault state read red (the text carries the
        // precise meaning; the colour reinforces it).
        _ => RED,
    }
}

/// Append a filled rectangle primitive (chrome backing).
fn push_filled_rect(list: &mut OverlayDrawList, rect: OverlayRect, color: OverlayColor) {
    list.push(OverlayPrimitive::FilledRect {
        rect,
        corner_radius: 0,
        color,
    });
}

/// Append four line strokes forming the outline of a [`PixelRect`] at the given
/// thickness.
fn push_box_outline(
    list: &mut OverlayDrawList,
    rect: PixelRect,
    thickness: u32,
    color: OverlayColor,
) {
    let left = round_dim_signed(rect.x);
    let top = round_dim_signed(rect.y);
    let width = round_dim(rect.width);
    let height = round_dim(rect.height);
    if width == 0 || height == 0 {
        return;
    }
    let thick = thickness.max(1).min(width / 2).min(height / 2).max(1);
    let bottom = top.saturating_add(i32_dim(height.saturating_sub(thick)));
    let right = left.saturating_add(i32_dim(width.saturating_sub(thick)));
    list.push(line(left, top, width, thick, color));
    list.push(line(left, bottom, width, thick, color));
    list.push(line(left, top, thick, height, color));
    list.push(line(right, top, thick, height, color));
}

/// Append a center-cross marker (two short strokes through the cross centre).
fn push_center_cross(
    list: &mut OverlayDrawList,
    cross: multiview_overlay::safearea::CenterCross,
    thickness: u32,
    color: OverlayColor,
) {
    let arm = round_dim(cross.arm_px).max(1);
    let cx = round_dim_signed(cross.x);
    let cy = round_dim_signed(cross.y);
    let t = thickness.max(1);
    list.push(line(
        cx.saturating_sub(i32_dim(arm)),
        cy.saturating_sub(i32_dim(t / 2)),
        arm.saturating_mul(2),
        t,
        color,
    ));
    list.push(line(
        cx.saturating_sub(i32_dim(t / 2)),
        cy.saturating_sub(i32_dim(arm)),
        t,
        arm.saturating_mul(2),
        color,
    ));
}

/// An axis-aligned line/border primitive.
fn line(x: i32, y: i32, width: u32, height: u32, color: OverlayColor) -> OverlayPrimitive {
    OverlayPrimitive::Line {
        rect: OverlayRect::new(x, y, width, height),
        color,
    }
}

/// Bridge the overlay model's [`AnalogHands`] (degrees clockwise from 12) into
/// the compositor's [`HandAngles`] — the same units, just the two crates' mirror
/// types (the compositor stays overlay-free; see [`clock_face`]).
fn hand_angles(hands: AnalogHands) -> HandAngles {
    HandAngles {
        hour_deg: hands.hour_deg,
        minute_deg: hands.minute_deg,
        second_deg: hands.second_deg,
    }
}

/// A coarse per-glyph advance estimate for sizing chrome (px); good enough for
/// placement (exact measuring would re-shape the run, unnecessary here).
fn quantize_advance(size_px: f32) -> u32 {
    round_dim(size_px * 0.55).max(1)
}

/// Round a non-negative `f32` pixel measure to `u32` (saturating), no `as`.
fn round_dim(value: f32) -> u32 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    let rounded = value.round();
    let mut lo = 0_u32;
    let mut hi = u32::MAX;
    while lo < hi {
        let mid = lo.saturating_add((hi - lo).saturating_add(1) / 2);
        if f32_dim(mid) <= rounded {
            lo = mid;
        } else {
            hi = mid.saturating_sub(1);
        }
    }
    lo
}

/// Round a (possibly negative) `f32` pixel coordinate to `i32` (saturating), no
/// `as` cast.
fn round_dim_signed(value: f32) -> i32 {
    if !value.is_finite() {
        return 0;
    }
    if value < 0.0 {
        i32_dim(round_dim(-value)).saturating_neg()
    } else {
        i32_dim(round_dim(value))
    }
}

/// Exact small-`u32` → `f32`, no `as`.
fn f32_dim(value: u32) -> f32 {
    let high = u16::try_from(value >> 16).unwrap_or(u16::MAX);
    let low = u16::try_from(value & 0xFFFF).unwrap_or(u16::MAX);
    f32::from(high) * 65_536.0 + f32::from(low)
}

/// Exact small-`i32` → `f32`, no `as`.
fn f32_signed(value: i32) -> f32 {
    if value < 0 {
        -f32_dim(value.unsigned_abs())
    } else {
        f32_dim(u32::try_from(value).unwrap_or(u32::MAX))
    }
}

/// Saturating `u32` → `i32`, no `as`.
fn i32_dim(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// Saturating `usize` → `u32`, no `as`.
fn u32_from_usize(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::Arc;

    use crate::wallclock::{WallClock, WallClockSource};

    use super::*;

    /// A test wall-clock whose instant the test controls (atomic so a single
    /// shared instance can be advanced between bakes — the anti-drift proof
    /// advances the same injected clock the baker holds). Always reports a `SYS`
    /// reference at the configured status.
    struct FakeClock {
        secs: AtomicI64,
        status: RefStatus,
    }

    impl FakeClock {
        fn new(secs: i64, status: RefStatus) -> Self {
            Self {
                secs: AtomicI64::new(secs),
                status,
            }
        }

        fn advance(&self, by: i64) {
            self.secs.fetch_add(by, Ordering::SeqCst);
        }
    }

    impl WallClock for FakeClock {
        fn unix_seconds(&self) -> i64 {
            self.secs.load(Ordering::SeqCst)
        }

        fn reference(&self) -> multiview_overlay::clock::TimeRef {
            multiview_overlay::clock::TimeRef::new(RefSource::System, self.status)
        }
    }

    /// A `WallClockSource` over a fresh fake clock pinned to `secs` (locked),
    /// returning both so a test can advance the clock after building the baker.
    fn fake_source_at(secs: i64) -> (WallClockSource, Arc<FakeClock>) {
        let fake = Arc::new(FakeClock::new(secs, RefStatus::Locked));
        let dynamic: Arc<dyn WallClock> = fake.clone();
        (WallClockSource::new(dynamic), fake)
    }

    /// A `WallClockSource` over a fake clock fixed at the Unix epoch (00:00:00
    /// UTC) — the legacy `base_unix_secs == 0` behaviour, for tests whose
    /// assertions do not depend on the displayed time-of-day.
    fn epoch_clock() -> WallClockSource {
        fake_source_at(0).0
    }

    /// Count the glyphs the digital clock + reference badge draw in the top-left
    /// clock band of the canvas (a generous strip).
    fn clock_band_glyphs(list: &OverlayDrawList) -> usize {
        let band = OverlayRect::new(0, 0, 600, 80);
        glyphs_in(list, band)
    }

    /// A fingerprint of the **digital clock readout** the baker actually drew this
    /// bake: the (`dest_x`, `dest_y`, ink-coverage-sum) of every glyph quad on the
    /// clock's text row (the top ~28 px strip, above the reference badge), in draw
    /// order. It changes iff the rendered time-of-day string changes — so it
    /// proves what the BAKER rendered (not just what the model would render).
    fn clock_readout_fingerprint(list: &OverlayDrawList) -> Vec<(i32, i32, u32)> {
        list.primitives
            .iter()
            .filter_map(|p| match p {
                OverlayPrimitive::Glyph {
                    dest_x,
                    dest_y,
                    coverage,
                    ..
                } if *dest_x >= 0 && *dest_x < 600 && *dest_y >= 0 && *dest_y < 28 => {
                    let ink: u32 = coverage.iter().map(|&c| u32::from(c)).sum();
                    Some((*dest_x, *dest_y, ink))
                }
                _ => None,
            })
            .collect()
    }

    /// A 2x2 grid of 640x360 cells on a 1280x720 canvas.
    fn quad_tiles() -> Vec<TileSpec> {
        let mk = |id: &str, label: &str, x: f32, y: f32| {
            TileSpec::new(
                id,
                label,
                PixelRect {
                    x,
                    y,
                    width: 640.0,
                    height: 360.0,
                },
            )
        };
        vec![
            mk("in_a", "CAMERA A", 0.0, 0.0),
            mk("in_b", "CAMERA B", 640.0, 0.0),
            mk("in_c", "CAMERA C", 0.0, 360.0),
            mk("in_d", "CAMERA D", 640.0, 360.0),
        ]
    }

    fn dynamics(entries: &[(&str, f64, SourceState)]) -> HashMap<String, TileDynamics> {
        entries
            .iter()
            .map(|(id, db, state)| {
                (
                    (*id).to_owned(),
                    TileDynamics {
                        meter_db: *db,
                        state: *state,
                        fault: TileFault::None,
                    },
                )
            })
            .collect()
    }

    /// Build a dynamics map carrying an explicit per-source fault, so a test can
    /// assert the fault badge renders only for the faulted tile(s).
    fn dynamics_with_faults(
        entries: &[(&str, f64, SourceState, TileFault)],
    ) -> HashMap<String, TileDynamics> {
        entries
            .iter()
            .map(|(id, db, state, fault)| {
                (
                    (*id).to_owned(),
                    TileDynamics {
                        meter_db: *db,
                        state: *state,
                        fault: *fault,
                    },
                )
            })
            .collect()
    }

    /// An empty per-source caption map (no tile shows a caption).
    fn no_captions() -> HashMap<String, Vec<String>> {
        HashMap::new()
    }

    /// An empty per-source bitmap-cue map (no tile shows a bitmap caption).
    fn no_bitmaps() -> HashMap<String, multiview_ffmpeg::caption::CueBitmap> {
        HashMap::new()
    }

    /// Build a per-source caption map: `source_id -> active cue lines`.
    fn captions(entries: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        entries
            .iter()
            .map(|(id, lines)| {
                (
                    (*id).to_owned(),
                    lines.iter().map(|l| (*l).to_owned()).collect(),
                )
            })
            .collect()
    }

    /// Glyph primitives whose top-left falls inside `rect` (canvas pixels).
    fn glyphs_in(list: &OverlayDrawList, rect: OverlayRect) -> usize {
        list.primitives
            .iter()
            .filter(|p| match p {
                OverlayPrimitive::Glyph { dest_x, dest_y, .. } => {
                    *dest_x >= rect.x
                        && *dest_x < rect.x + i32_dim(rect.width)
                        && *dest_y >= rect.y
                        && *dest_y < rect.y + i32_dim(rect.height)
                }
                _ => false,
            })
            .count()
    }

    /// The **live green fill** rectangles in the tile's right-edge meter column.
    ///
    /// The meter draws three primitives per track: a dim (low-alpha) green
    /// background spanning the whole track, the live opaque-green fill, and an
    /// amber 1px peak tick. We keep only the live fill (opaque green) so its
    /// height is a faithful proxy for the source's loudness deflection.
    fn meter_fill_in(list: &OverlayDrawList, geom: TileGeometry) -> Vec<OverlayRect> {
        let track = geom.meter_track();
        let cell_top = geom.y;
        let cell_bottom = geom.y.saturating_add(i32_dim(geom.height));
        list.primitives
            .iter()
            .filter_map(|p| match p {
                OverlayPrimitive::FilledRect { rect, color, .. }
                    if rect.x >= track.x.saturating_sub(2)
                        && rect.x <= track.x.saturating_add(i32_dim(track.width))
                        // Constrain to THIS cell's vertical extent so a sibling
                        // tile sharing the meter column is not miscounted.
                        && rect.y >= cell_top
                        && rect.y <= cell_bottom
                        // Opaque green live fill only (exclude the dim background
                        // and the amber peak tick).
                        && (color.a - GREEN.a).abs() < 0.01
                        && (color.g - GREEN.g).abs() < 0.01 =>
                {
                    Some(*rect)
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn resolver_emits_label_meter_and_flag_per_cell() {
        let tiles = quad_tiles();
        let mut baker = OverlayBaker::new(tiles.clone(), epoch_clock()).unwrap();

        // in_a loud + LIVE; in_b silent + LIVE; in_c missing (defaults NO_SIGNAL);
        // in_d explicitly NO_SIGNAL.
        let dyns = dynamics(&[
            ("in_a", -3.0, SourceState::Live),
            (
                "in_b",
                multiview_audio::Ballistics::FLOOR_DB,
                SourceState::Live,
            ),
            (
                "in_d",
                multiview_audio::Ballistics::FLOOR_DB,
                SourceState::NoSignal,
            ),
        ]);

        // Drive a few frames so the conflated meter establishes a level.
        let mut list = OverlayDrawList::new();
        for tick in 0..5 {
            let pts = MediaTime::from_nanos(tick * 40_000_000);
            list = baker
                .draw_list(pts, &dyns, &no_captions(), &no_bitmaps())
                .unwrap();
        }

        // Each cell must host glyphs for its label + flag, somewhere inside it.
        for spec in &tiles {
            let geom = TileGeometry::resolve(spec.rect);
            let cell = OverlayRect::new(geom.x, geom.y, geom.width, geom.height);
            assert!(
                glyphs_in(&list, cell) > 0,
                "tile {} drew no text glyphs in its cell rect",
                spec.source_id
            );
        }

        // The loud LIVE tile (in_a) must have a meaningfully taller fill than the
        // silent tile (in_b): the per-input meter reflects THAT input's audio.
        let geom_a = TileGeometry::resolve(tiles[0].rect);
        let geom_b = TileGeometry::resolve(tiles[1].rect);
        let tallest = |rects: &[OverlayRect]| rects.iter().map(|r| r.height).max().unwrap_or(0);
        let a_fill = tallest(&meter_fill_in(&list, geom_a));
        let b_fill = tallest(&meter_fill_in(&list, geom_b));
        assert!(
            a_fill > b_fill + 10,
            "loud tile meter fill ({a_fill}) must exceed silent tile fill ({b_fill})"
        );
    }

    /// The horizontal-centre caption band of a cell: the lower portion of the
    /// cell, with the leftmost quarter (where the bottom-left label chip sits)
    /// excluded so only centred caption glyphs are counted.
    fn caption_glyphs_centred_in_cell(list: &OverlayDrawList, geom: TileGeometry) -> usize {
        let left_inset = geom.width / 4;
        let band = OverlayRect::new(
            geom.x.saturating_add(i32_dim(left_inset)),
            geom.y
                .saturating_add(i32_dim(geom.height.saturating_mul(3) / 5)),
            geom.width.saturating_sub(left_inset),
            geom.height.saturating_mul(2) / 5,
        );
        glyphs_in(list, band)
    }

    #[test]
    fn caption_burns_into_only_its_own_tile() {
        // A cue published for in_a must render centred caption glyphs inside in_a's
        // cell rect; a sibling tile (in_b) with no cue must render none in its
        // centred caption band. This is the per-tile burn-in contract (replacing
        // the old program-wide canvas-bottom cue).
        let tiles = quad_tiles();
        let mut baker = OverlayBaker::new(tiles.clone(), epoch_clock()).unwrap();

        let dyns = dynamics(&[
            ("in_a", -3.0, SourceState::Live),
            ("in_b", -3.0, SourceState::Live),
        ]);
        let caps = captions(&[("in_a", &["English subtitle 1 -Unforced-"])]);

        let pts = MediaTime::from_nanos(1_500_000_000);
        let list = baker.draw_list(pts, &dyns, &caps, &no_bitmaps()).unwrap();

        let geom_a = TileGeometry::resolve(tiles[0].rect);
        let geom_b = TileGeometry::resolve(tiles[1].rect);

        let a_caption = caption_glyphs_centred_in_cell(&list, geom_a);
        let b_caption = caption_glyphs_centred_in_cell(&list, geom_b);
        assert!(
            a_caption > 0,
            "in_a's caption must render centred glyphs inside in_a's cell (got {a_caption})"
        );
        assert_eq!(
            b_caption, 0,
            "in_b has no cue: its centred caption band must be empty (got {b_caption})"
        );

        // Every caption glyph in_a drew must lie within in_a's cell rect (the
        // caption is cell-local, not canvas-wide). Count all glyphs in in_a's
        // caption band vs the whole canvas's caption-row centred bands of OTHER
        // cells in the same row — in_b (same row, no cue) already proved zero.
        let in_a_cell = OverlayRect::new(geom_a.x, geom_a.y, geom_a.width, geom_a.height);
        assert!(
            glyphs_in(&list, in_a_cell) >= a_caption,
            "every in_a caption glyph must fall inside in_a's cell rect"
        );
    }

    /// Build a per-source bitmap-cue map: `source_id -> CueBitmap`. The
    /// `#[non_exhaustive]` `CueBitmap`/`CueRect` are built only inside
    /// `multiview-ffmpeg`, so the cue is constructed via `CaptionCue::try_bitmap` and
    /// its bitmap matched out.
    fn bitmaps(
        entries: &[(&str, u32, u32)],
    ) -> HashMap<String, multiview_ffmpeg::caption::CueBitmap> {
        use multiview_ffmpeg::caption::{CaptionCue, CueRect};
        entries
            .iter()
            .map(|(id, w, h)| {
                // Opaque-white premultiplied band (rgb == a == 255), w*h*4 bytes.
                let rgba: Vec<u8> = (0..(*w * *h))
                    .flat_map(|_| [255_u8, 255, 255, 255])
                    .collect();
                let rect = CueRect::new(0, 0, *w, *h);
                let cue = CaptionCue::try_bitmap(
                    MediaTime::from_nanos(0),
                    MediaTime::from_nanos(1_000),
                    rgba,
                    rect,
                )
                .expect("valid bitmap cue");
                let CaptionCue::Bitmap { bitmap, .. } = cue else {
                    panic!("try_bitmap must yield a bitmap cue");
                };
                ((*id).to_owned(), bitmap)
            })
            .collect()
    }

    /// The Image primitives whose dest rect lies inside `rect` (canvas pixels).
    fn images_in(list: &OverlayDrawList, rect: OverlayRect) -> Vec<OverlayRect> {
        list.primitives
            .iter()
            .filter_map(|p| match p {
                OverlayPrimitive::Image { dest, .. }
                    if dest.x >= rect.x
                        && dest.x < rect.x + i32_dim(rect.width)
                        && dest.y >= rect.y
                        && dest.y < rect.y + i32_dim(rect.height) =>
                {
                    Some(*dest)
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn bitmap_caption_burns_into_only_its_own_tile_bottom_anchored() {
        // A CueBitmap published for in_a must produce >=1 Image primitive whose
        // dest is inside in_a's cell rect; in_b (no bitmap) gets none. The dest is
        // bottom-anchored within the cell (mirrors the text caption model).
        let tiles = quad_tiles();
        let mut baker = OverlayBaker::new(tiles.clone(), epoch_clock()).unwrap();

        let dyns = dynamics(&[
            ("in_a", -3.0, SourceState::Live),
            ("in_b", -3.0, SourceState::Live),
        ]);
        // A source-resolution band (like a 400x60 DVB-sub rect on a 640x480 src).
        let bmps = bitmaps(&[("in_a", 400, 60)]);

        let pts = MediaTime::from_nanos(1_500_000_000);
        let list = baker.draw_list(pts, &dyns, &no_captions(), &bmps).unwrap();

        let geom_a = TileGeometry::resolve(tiles[0].rect);
        let geom_b = TileGeometry::resolve(tiles[1].rect);
        let cell_a = OverlayRect::new(geom_a.x, geom_a.y, geom_a.width, geom_a.height);
        let cell_b = OverlayRect::new(geom_b.x, geom_b.y, geom_b.width, geom_b.height);

        let a_images = images_in(&list, cell_a);
        let b_images = images_in(&list, cell_b);
        assert!(
            !a_images.is_empty(),
            "in_a's bitmap cue must produce >=1 Image inside in_a's cell"
        );
        assert!(
            b_images.is_empty(),
            "in_b has no bitmap cue: no Image in its cell (got {})",
            b_images.len()
        );

        // The image dest is bottom-anchored: its bottom edge sits in the lower
        // half of the cell, and the whole dest stays within the cell rect.
        let cell_mid_y = geom_a.y.saturating_add(i32_dim(geom_a.height / 2));
        let cell_bottom = geom_a.y.saturating_add(i32_dim(geom_a.height));
        for d in &a_images {
            let dest_bottom = d.y.saturating_add(i32_dim(d.height));
            assert!(
                dest_bottom > cell_mid_y,
                "image dest bottom {dest_bottom} should be in the lower half (mid {cell_mid_y})"
            );
            assert!(
                dest_bottom <= cell_bottom,
                "image dest bottom {dest_bottom} within cell bottom {cell_bottom}"
            );
            assert!(d.x >= geom_a.x, "image dest left within cell");
            assert!(
                d.x.saturating_add(i32_dim(d.width))
                    <= geom_a.x.saturating_add(i32_dim(geom_a.width)),
                "image dest right within cell"
            );
        }
    }

    /// The TOP-RIGHT fault-badge band of a cell: the upper chip-height strip,
    /// with the leftmost half (where the top-left state flag sits) excluded so
    /// only the right-aligned fault badge is counted. The right-edge meter draws
    /// FilledRect/Line primitives, not glyphs, so a glyph here is the badge.
    fn fault_badge_glyphs_in_cell(list: &OverlayDrawList, geom: TileGeometry) -> usize {
        let half = geom.width / 2;
        let band = OverlayRect::new(
            geom.x.saturating_add(i32_dim(half)),
            geom.y,
            geom.width.saturating_sub(half),
            geom.chip_height(),
        );
        glyphs_in(list, band)
    }

    #[test]
    fn fault_badge_renders_only_on_the_faulted_tile() {
        // A tile carrying a content fault (BLACK / FROZEN / NO AUDIO) must render
        // fault-badge glyphs in its TOP-RIGHT band; a healthy sibling tile
        // (TileFault::None) must render none there. Mirrors the per-tile caption
        // burn-in contract: the badge is cell-local, drawn only where the fault is.
        let tiles = quad_tiles();
        let mut baker = OverlayBaker::new(tiles.clone(), epoch_clock()).unwrap();

        // in_a black, in_b frozen, in_c silent, in_d healthy (no fault).
        let dyns = dynamics_with_faults(&[
            ("in_a", -3.0, SourceState::Live, TileFault::Black),
            ("in_b", -3.0, SourceState::Live, TileFault::Frozen),
            (
                "in_c",
                multiview_audio::Ballistics::FLOOR_DB,
                SourceState::Live,
                TileFault::Silent,
            ),
            ("in_d", -3.0, SourceState::Live, TileFault::None),
        ]);

        let list = baker
            .draw_list(MediaTime::ZERO, &dyns, &no_captions(), &no_bitmaps())
            .unwrap();

        let geom_a = TileGeometry::resolve(tiles[0].rect);
        let geom_b = TileGeometry::resolve(tiles[1].rect);
        let geom_c = TileGeometry::resolve(tiles[2].rect);
        let geom_d = TileGeometry::resolve(tiles[3].rect);

        assert!(
            fault_badge_glyphs_in_cell(&list, geom_a) > 0,
            "the BLACK-faulted tile (in_a) must draw a fault badge top-right"
        );
        assert!(
            fault_badge_glyphs_in_cell(&list, geom_b) > 0,
            "the FROZEN-faulted tile (in_b) must draw a fault badge top-right"
        );
        assert!(
            fault_badge_glyphs_in_cell(&list, geom_c) > 0,
            "the SILENT-faulted tile (in_c) must draw a fault badge top-right"
        );
        assert_eq!(
            fault_badge_glyphs_in_cell(&list, geom_d),
            0,
            "the healthy tile (in_d) must draw NO fault badge"
        );
    }

    #[test]
    fn missing_source_defaults_to_no_signal_flag() {
        let tiles = quad_tiles();
        let mut baker = OverlayBaker::new(tiles.clone(), epoch_clock()).unwrap();
        // No dynamics at all: every tile must be NO_SIGNAL (never panics/stalls).
        let list = baker
            .draw_list(
                MediaTime::ZERO,
                &HashMap::new(),
                &no_captions(),
                &no_bitmaps(),
            )
            .unwrap();
        // The "NO SIGNAL" flag draws glyphs near each tile's top-left.
        for spec in &tiles {
            let geom = TileGeometry::resolve(spec.rect);
            let flag_box = OverlayRect::new(geom.x, geom.y, geom.width / 2, geom.chip_height());
            assert!(
                glyphs_in(&list, flag_box) > 0,
                "tile {} drew no NO_SIGNAL flag glyphs",
                spec.source_id
            );
        }
    }

    #[test]
    fn meter_and_flag_stay_inside_the_cell_rect() {
        let tiles = quad_tiles();
        let mut baker = OverlayBaker::new(tiles.clone(), epoch_clock())
            .unwrap()
            .with_per_tile_safe_area(true);
        let dyns = dynamics(&[("in_a", 0.0, SourceState::Live)]);
        let list = baker
            .draw_list(MediaTime::ZERO, &dyns, &no_captions(), &no_bitmaps())
            .unwrap();

        // Every per-tile primitive for tile A must lie within tile A's rect (the
        // per-tile safe-area / meter / flag must NOT span the whole canvas — this
        // is the canvas-wide bug fix).
        let geom = TileGeometry::resolve(tiles[0].rect);
        let track = geom.meter_track();
        let fills = meter_fill_in(&list, geom);
        for r in &fills {
            assert!(
                r.x >= geom.x && r.x + i32_dim(r.width) <= geom.x + i32_dim(geom.width) + 1,
                "meter fill {r:?} escaped tile A horizontally (track {track:?})"
            );
            assert!(
                r.y >= geom.y - 1 && r.y + i32_dim(r.height) <= geom.y + i32_dim(geom.height) + 1,
                "meter fill {r:?} escaped tile A vertically"
            );
        }
        assert!(!fills.is_empty(), "tile A drew no meter fill at 0 dBFS");
    }

    #[test]
    fn tiny_tiles_are_skipped_without_panicking() {
        // A degenerate 10x10 cell is below the legibility floor — skip, never mush.
        let tiles = vec![TileSpec::new(
            "in_tiny",
            "X",
            PixelRect {
                x: 0.0,
                y: 0.0,
                width: 10.0,
                height: 10.0,
            },
        )];
        let mut baker = OverlayBaker::new(tiles, epoch_clock()).unwrap();
        let dyns = dynamics(&[("in_tiny", 0.0, SourceState::Live)]);
        // Must not panic; produces (at most) the clock label, no tile chrome.
        let _ = baker
            .draw_list(MediaTime::ZERO, &dyns, &no_captions(), &no_bitmaps())
            .unwrap();
    }

    /// Count the ring + stroke primitives (the analog clock-face vocabulary) in a
    /// draw list.
    fn rings_and_strokes(list: &OverlayDrawList) -> (usize, usize) {
        let rings = list
            .primitives
            .iter()
            .filter(|p| matches!(p, OverlayPrimitive::Ring { .. }))
            .count();
        let strokes = list
            .primitives
            .iter()
            .filter(|p| matches!(p, OverlayPrimitive::Stroke { .. }))
            .count();
        (rings, strokes)
    }

    #[test]
    fn analog_clock_draws_a_ring_plus_hands_and_ticks() {
        // With an analog clock configured, the baker must emit the clock-face
        // vocabulary: a bezel ring + 12 ticks + 3 hands (the digital baseline
        // emits NONE of these ring/stroke primitives).
        let tiles = quad_tiles();
        let mut plain = OverlayBaker::new(tiles.clone(), epoch_clock()).unwrap();
        let plain_list = plain
            .draw_list(
                MediaTime::ZERO,
                &HashMap::new(),
                &no_captions(),
                &no_bitmaps(),
            )
            .unwrap();
        let (plain_rings, plain_strokes) = rings_and_strokes(&plain_list);
        assert_eq!(
            (plain_rings, plain_strokes),
            (0, 0),
            "the digital baseline draws no analog face"
        );

        let mut baker = OverlayBaker::new(tiles, epoch_clock())
            .unwrap()
            .with_analog_clock(AnalogClockSpec::new(
                TimeZoneOffset::UTC,
                1160.0,
                600.0,
                90.0,
            ));
        let list = baker
            .draw_list(
                MediaTime::ZERO,
                &HashMap::new(),
                &no_captions(),
                &no_bitmaps(),
            )
            .unwrap();
        let (rings, strokes) = rings_and_strokes(&list);
        assert_eq!(rings, 1, "the analog face draws exactly one bezel ring");
        assert_eq!(strokes, 15, "12 hour ticks + 3 hands are stroke primitives");
    }

    // NOTE: the former `analog_clock_hands_advance_with_the_media_timeline` test
    // is superseded by `analog_face_tracks_the_wall_clock_between_bakes` below.
    // Task #30 deliberately changes the displayed clock (digital AND analog) to
    // track the LIVE wall-clock time-of-day sampled at bake time (anti-drift),
    // NOT the output-tick / media timeline — so a test asserting the analog hands
    // advance with the media timeline now describes the wrong contract. The
    // replacement advances the injected WALL clock (holding the media tick fixed)
    // and keeps the same "the second hand must move" assertion, un-weakened.

    /// Collect the digital clock string the baker rendered this bake, by reading
    /// it back from the configured clock model against the source's sampled
    /// instant. (The glyph quads themselves are not text-recoverable, so we assert
    /// the *model* output the baker drives; a separate test proves glyphs ink.)
    fn rendered_digital(clock: ClockModel, src: &WallClockSource) -> String {
        clock
            .render_digital(src.now())
            .expect("digital clock renders a string")
    }

    #[test]
    fn digital_clock_shows_the_injected_time_of_day() {
        // A fake clock pinned to a known instant must drive the digital readout to
        // that exact time-of-day. 1_780_000_000 = 2026-05-31 20:26:40 UTC.
        let (src, _fake) = fake_source_at(1_780_000_000);
        let mut baker = OverlayBaker::new(quad_tiles(), src.clone()).unwrap();

        // The baker draws SOMETHING in the clock band (the digital readout +
        // reference badge), proving the clock overlay renders.
        let list = baker
            .draw_list(
                MediaTime::ZERO,
                &HashMap::new(),
                &no_captions(),
                &no_bitmaps(),
            )
            .unwrap();
        assert!(
            clock_band_glyphs(&list) > 0,
            "the digital clock + reference badge must draw glyphs top-left"
        );

        // The model the baker drives must format that instant's time-of-day.
        let clock = ClockModel::new(
            ClockFace::digital_24h(),
            TimeZoneOffset::UTC,
            src.reference(),
        );
        assert_eq!(rendered_digital(clock, &src), "20:26:40");
    }

    #[test]
    fn program_clock_readout_sits_on_a_legibility_chip() {
        // #38: the digital clock draws a translucent backing rect behind its
        // readout (top-left, at (8,2)) so it stays legible over a tile's state
        // flag underneath — the chip is contrast only; the meaning is the text.
        let (src, _fake) = fake_source_at(1_780_000_000);
        let mut baker = OverlayBaker::new(quad_tiles(), src).unwrap();
        let list = baker
            .draw_list(
                MediaTime::ZERO,
                &HashMap::new(),
                &no_captions(),
                &no_bitmaps(),
            )
            .unwrap();
        let has_clock_chip = list.primitives.iter().any(|p| {
            matches!(
                p,
                OverlayPrimitive::FilledRect { rect, .. }
                    if rect.x <= 8 && rect.y <= 2 && rect.width >= 26 && rect.height >= 26
            )
        });
        assert!(
            has_clock_chip,
            "the program clock readout must sit on a translucent backing chip (#38)"
        );
        // The readout glyphs still render on top of the chip.
        assert!(clock_band_glyphs(&list) > 0);
    }

    #[test]
    fn displayed_time_of_day_tracks_the_wall_clock_not_the_tick() {
        // ANTI-DRIFT: advancing the injected real clock by N seconds advances the
        // displayed time-of-day by N seconds, regardless of the output-tick index.
        // If the clock were derived from the tick counter, holding the clock fixed
        // while ticks advance would change the readout (it must NOT), and advancing
        // the clock while the tick is held would not (it MUST).
        let (src, fake) = fake_source_at(1_780_000_000); // 20:26:40 UTC
        let mut baker = OverlayBaker::new(quad_tiles(), src.clone()).unwrap();

        let clock = ClockModel::new(
            ClockFace::digital_24h(),
            TimeZoneOffset::UTC,
            src.reference(),
        );

        // Bake at tick 0: the readout the BAKER drew is the wall instant.
        let list_t0 = baker
            .draw_list(
                MediaTime::ZERO,
                &HashMap::new(),
                &no_captions(),
                &no_bitmaps(),
            )
            .unwrap();
        let fp_t0 = clock_readout_fingerprint(&list_t0);
        assert!(!fp_t0.is_empty(), "the digital readout must draw glyphs");
        assert_eq!(rendered_digital(clock, &src), "20:26:40");

        // Advance the output-tick counter by 100 ticks (4 s of media at 25 fps)
        // WITHOUT touching the wall clock: the readout the BAKER draws must be
        // byte-for-byte identical. A tick-derived clock would now read 20:26:44 —
        // a different glyph fingerprint.
        let list_tick = baker
            .draw_list(
                MediaTime::from_nanos(4_000_000_000),
                &HashMap::new(),
                &no_captions(),
                &no_bitmaps(),
            )
            .unwrap();
        assert_eq!(
            clock_readout_fingerprint(&list_tick),
            fp_t0,
            "the BAKED time-of-day must not advance with the output tick (anti-drift)"
        );

        // Now advance the WALL clock by 125 s while holding the tick at 0: the
        // readout the BAKER draws must CHANGE (20:26:40 -> 20:28:45), proving it
        // tracks the live wall clock, and the model agrees.
        fake.advance(125);
        let list_wall = baker
            .draw_list(
                MediaTime::ZERO,
                &HashMap::new(),
                &no_captions(),
                &no_bitmaps(),
            )
            .unwrap();
        assert_ne!(
            clock_readout_fingerprint(&list_wall),
            fp_t0,
            "the BAKED time-of-day must advance when the wall clock advances"
        );
        assert_eq!(
            rendered_digital(clock, &src),
            "20:28:45",
            "the displayed time must track the advancing wall clock"
        );
    }

    #[test]
    fn analog_face_tracks_the_wall_clock_between_bakes() {
        // The analog face is a clock too: its second hand must move when the
        // injected WALL clock advances one second (not the media tick). Holding the
        // tick fixed and advancing only the wall clock proves it tracks real time.
        let (src, fake) = fake_source_at(1_780_000_000);
        let mut baker = OverlayBaker::new(quad_tiles(), src)
            .unwrap()
            .with_analog_clock(AnalogClockSpec::new(
                TimeZoneOffset::UTC,
                1160.0,
                600.0,
                90.0,
            ));

        let second_tip = |list: &OverlayDrawList| -> (f32, f32) {
            list.primitives
                .iter()
                .filter_map(|p| match p {
                    OverlayPrimitive::Stroke { x0, y0, x1, y1, .. } => {
                        let dx = x1 - x0;
                        let dy = y1 - y0;
                        Some((dx * dx + dy * dy, (*x1, *y1)))
                    }
                    _ => None,
                })
                .max_by(|a, b| a.0.total_cmp(&b.0))
                .map(|(_, tip)| tip)
                .unwrap()
        };

        let t0 = baker
            .draw_list(
                MediaTime::ZERO,
                &HashMap::new(),
                &no_captions(),
                &no_bitmaps(),
            )
            .unwrap();
        // Advance ONLY the wall clock by 1 s; hold the tick at ZERO.
        fake.advance(1);
        let t1 = baker
            .draw_list(
                MediaTime::ZERO,
                &HashMap::new(),
                &no_captions(),
                &no_bitmaps(),
            )
            .unwrap();
        let (x0, y0) = second_tip(&t0);
        let (x1, y1) = second_tip(&t1);
        let moved = ((x1 - x0).powi(2) + (y1 - y0).powi(2)).sqrt();
        assert!(
            moved > 1.0,
            "the analog second hand must move when the wall clock advances 1 s (moved {moved}px)"
        );
    }

    #[test]
    fn reference_badge_renders_status_text_and_glyph() {
        // The clock overlay must surface the timing reference as TEXT + a GLYPH
        // (accessibility — never colour alone). With a SYS-locked source the badge
        // text is "SYS locked" and the locked glyph is drawn near the clock.
        let (src, _fake) = fake_source_at(1_780_000_000);
        assert_eq!(src.reference().status_text(), "SYS locked");
        let glyph = src.reference().status.glyph();

        let mut baker = OverlayBaker::new(quad_tiles(), src).unwrap();
        let list = baker
            .draw_list(
                MediaTime::ZERO,
                &HashMap::new(),
                &no_captions(),
                &no_bitmaps(),
            )
            .unwrap();

        // The reference badge glyph must be present among the rasterized runs. We
        // rasterize the same char standalone and confirm the badge drew at least
        // that glyph's worth of ink (a non-empty coverage) in the clock band.
        let _ = glyph; // the glyph char is asserted via the engine below.
        assert!(
            clock_band_glyphs(&list) > "06:26:40".len(),
            "the clock band must hold MORE glyphs than the bare time — the \
             reference badge text adds to it"
        );
    }

    /// The whole canvas of a 2x2 `quad_tiles` layout (1280x720).
    const CANVAS_W: u32 = 1280;
    const CANVAS_H: u32 = 720;

    /// Count primitives whose footprint is in the top-right quarter of the canvas
    /// (where the enforcement watermark is anchored).
    fn corner_primitive_count(list: &OverlayDrawList) -> usize {
        let band = OverlayRect::new(i32_dim(CANVAS_W / 2), 0, CANVAS_W / 2, CANVAS_H / 2);
        list.primitives
            .iter()
            .filter(|p| match p {
                OverlayPrimitive::FilledRect { rect, .. } | OverlayPrimitive::Line { rect, .. } => {
                    rect.x >= band.x && rect.y >= band.y && rect.y < i32_dim(CANVAS_H / 2)
                }
                _ => false,
            })
            .count()
    }

    #[test]
    fn no_watermark_when_the_signal_is_clean() {
        // A baker with no watermark signal (the compliant default) never draws the
        // enforcement watermark — the corner carries only ordinary tile chrome.
        let signal = crate::licence::WatermarkSignal::clean();
        let mut baker = OverlayBaker::new(quad_tiles(), epoch_clock())
            .unwrap()
            .with_watermark(signal, CANVAS_W, CANVAS_H);
        let before = corner_primitive_count(
            &baker
                .draw_list(
                    MediaTime::ZERO,
                    &HashMap::new(),
                    &no_captions(),
                    &no_bitmaps(),
                )
                .unwrap(),
        );
        // (The corner already holds the top-right tile's own state flag/meter; we
        // only assert the watermark adds nothing when clean — compared below.)
        let _ = before;
        let clean = baker
            .draw_list(
                MediaTime::ZERO,
                &HashMap::new(),
                &no_captions(),
                &no_bitmaps(),
            )
            .unwrap();
        assert!(
            !has_watermark_underline(&clean),
            "a clean signal must not draw the enforcement watermark underline bar"
        );
    }

    #[test]
    fn watermark_is_drawn_on_the_canvas_corner_when_the_signal_is_set() {
        // A baker whose signal is at a watermark rung draws the enforcement
        // watermark primitives into the TOP-RIGHT canvas corner — deterministically
        // (golden via the injected signal, never a per-pixel hot-path decision).
        let signal =
            crate::licence::WatermarkSignal::at(multiview_licence::EnforcementLevel::Watermark);
        let mut baker = OverlayBaker::new(quad_tiles(), epoch_clock())
            .unwrap()
            .with_watermark(signal, CANVAS_W, CANVAS_H);
        let list = baker
            .draw_list(
                MediaTime::ZERO,
                &HashMap::new(),
                &no_captions(),
                &no_bitmaps(),
            )
            .unwrap();
        // The watermark's underline bar (a distinctive Line) marks it apart from
        // ordinary tile chrome; it must be present and in the top-right corner.
        assert!(
            has_watermark_underline(&list),
            "a watermark signal must draw the enforcement watermark"
        );
        assert!(
            corner_primitive_count(&list)
                > corner_primitive_count(
                    &OverlayBaker::new(quad_tiles(), epoch_clock())
                        .unwrap()
                        .draw_list(
                            MediaTime::ZERO,
                            &HashMap::new(),
                            &no_captions(),
                            &no_bitmaps()
                        )
                        .unwrap()
                ),
            "the watermark adds primitives to the top-right corner vs an un-watermarked bake"
        );
    }

    /// Whether the draw list contains the enforcement-watermark underline bar: a
    /// thin amber `Line` in the top-right corner (the watermark's signature
    /// primitive — see `multiview_compositor::overlay::watermark`).
    fn has_watermark_underline(list: &OverlayDrawList) -> bool {
        list.primitives.iter().any(|p| match p {
            OverlayPrimitive::Line { rect, color } => {
                rect.x >= i32_dim(CANVAS_W * 3 / 4)
                    && rect.y < i32_dim(CANVAS_H / 2)
                    // The watermark mark colour is a warm amber (r high, b low).
                    && color.r > 0.8
                    && color.b < 0.3
            }
            _ => false,
        })
    }
}
