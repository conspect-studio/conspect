//! RT-11 / ADR-0034 — the engine **apply** of per-stream route intents at the
//! frame-boundary command drain.
//!
//! These tests assert that a drained route intent actually **re-points** the live
//! crosspoint — not merely that it was accepted:
//!
//! * a `RouteIntent::Video` re-points which source a layout cell samples (the
//!   next compose tick draws the new source);
//! * a `RouteIntent::Audio` re-points which `AudioStore` a program-bus channel
//!   pulls (the next mix reads the new source's samples);
//! * a `RouteIntent::Subtitle` re-points which `CueSource` a subtitle layer
//!   samples (the next sample shows the new source's cue);
//! * the `StreamSelector` (Best / Index / Language) resolves against the input's
//!   `StreamInventory` to the concrete stream;
//! * a drained batch coalesces (≤1 effective re-point per destination per tick).
//!
//! Apply happens through the engine's `RouteApplier` exactly as the runtime's
//! per-tick control hook would drive it — never blocking the output clock.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    clippy::as_conversions,
    clippy::similar_names
)]

use std::collections::HashMap;
use std::sync::Arc;

use multiview_audio::{AudioBlock, AudioFormat, AudioStore, ChannelLayout, ProgramBus};
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_config::routing::{StreamRef, StreamSelector};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, FitMode, Layout};
use multiview_core::stream::{
    Bcp47, StableStreamId, StreamDescriptor, StreamDetail, StreamInventory, StreamKind,
};
use multiview_core::time::{MediaTime, Rational};
use multiview_engine::route::{RouteApplier, RouteIntent, RouteResolution};
use multiview_engine::CompositorDrive;
use multiview_framestore::TileStore;
use multiview_overlay::{CueSource, SubtitleLayer};

fn resolved_color() -> ColorInfo {
    ColorInfo::default().resolve_defaults(1920, 1080)
}

fn solid(w: u32, h: u32, y: u8) -> Nv12Image {
    Nv12Image::solid(w, h, y, 128, 128, resolved_color()).unwrap()
}

fn nosignal_card(w: u32, h: u32) -> Nv12Image {
    Nv12Image::solid(w, h, 16, 128, 128, resolved_color()).unwrap()
}

fn one_cell_layout(w: u32, h: u32, source: &str) -> Layout {
    Layout {
        name: "test".to_owned(),
        canvas: Canvas {
            width: w,
            height: h,
            fps_num: 60,
            fps_den: 1,
        },
        cells: vec![Cell {
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
            z: 0,
            fit: FitMode::Contain,
            source: Some(source.to_owned()),
            ..Cell::default()
        }],
    }
}

fn video_stream(codec: &str, pid: u16) -> StreamDescriptor {
    StreamDescriptor::new(
        StableStreamId::from_ts_pid(StreamKind::Video, pid),
        StreamKind::Video,
        codec,
        StreamDetail::Video {
            width: 1920,
            height: 1080,
            frame_rate: None,
        },
    )
}

fn audio_stream(lang: &str, channels: u16, pid: u16) -> StreamDescriptor {
    StreamDescriptor::new(
        StableStreamId::from_ts_pid(StreamKind::Audio, pid),
        StreamKind::Audio,
        "aac",
        StreamDetail::Audio {
            channels,
            sample_rate: 48_000,
        },
    )
    .with_language(Bcp47::parse(lang).ok())
}

fn subtitle_stream(pid: u16) -> StreamDescriptor {
    StreamDescriptor::new(
        StableStreamId::from_ts_pid(StreamKind::Subtitle, pid),
        StreamKind::Subtitle,
        "dvbsub",
        StreamDetail::Subtitle { forced: false },
    )
}

/// A `CueSource` that always returns a constant-text cue at any time.
struct ConstCue(&'static str);
impl CueSource for ConstCue {
    fn active_at(&self, _now: MediaTime) -> Option<multiview_overlay::Cue> {
        Some(multiview_overlay::Cue {
            start: MediaTime::ZERO,
            end: MediaTime::from_nanos(i64::MAX),
            lines: vec![self.0.to_owned()],
        })
    }
}

// ---------------------------------------------------------------------------
// (a) VIDEO route applies through the drain.
// ---------------------------------------------------------------------------

#[test]
fn route_video_applies_repointing_the_cell() {
    let (w, h) = (64, 64);
    let store_a = Arc::new(TileStore::<Nv12Image>::with_defaults("cam-a"));
    let store_b = Arc::new(TileStore::<Nv12Image>::with_defaults("cam-b"));
    store_a.publish(solid(w, h, 40), MediaTime::ZERO);
    store_b.publish(solid(w, h, 200), MediaTime::ZERO);
    let mut stores = HashMap::new();
    stores.insert("cam-a".to_owned(), store_a);
    stores.insert("cam-b".to_owned(), store_b);

    let mut drive = CompositorDrive::new(
        Arc::new(one_cell_layout(w, h, "cam-a")),
        stores,
        nosignal_card(w, h),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap()
    .with_cell_ids(vec![Some("c0".to_owned())]);

    // cam-b's inventory (a single video stream).
    let mut inventories = HashMap::new();
    inventories.insert(
        "cam-b".to_owned(),
        StreamInventory::from_streams(vec![video_stream("h264", 256)]).with_input_id("cam-b"),
    );
    // Resolution: cam-b's best video stream maps to the cam-b video store key.
    let mut resolution = RouteResolution::new(inventories);
    resolution.set_video_store_key(&StreamRef::best("cam-b", StreamKind::Video), "cam-b");

    // Before: dark cam-a.
    let f0 = drive
        .compose(multiview_engine::clock::Tick {
            index: 1,
            pts: MediaTime::from_nanos(1_000_000),
        })
        .unwrap();
    assert!(f0.canvas.sample(w / 2, h / 2).unwrap().0 < 100);

    // Drain one RouteVideo intent through the applier (the frame-boundary hook).
    let mut applier = RouteApplier::new(&resolution);
    applier
        .apply_video(
            &mut drive,
            &[RouteIntent::Video {
                cell: "c0".to_owned(),
                source: StreamRef::best("cam-b", StreamKind::Video),
            }],
        )
        .expect("video route applies");

    // After: the next compose draws bright cam-b — the cell was actually re-pointed.
    let f1 = drive
        .compose(multiview_engine::clock::Tick {
            index: 2,
            pts: MediaTime::from_nanos(2_000_000),
        })
        .unwrap();
    assert!(
        f1.canvas.sample(w / 2, h / 2).unwrap().0 > 150,
        "RouteVideo must re-point the cell to cam-b"
    );
    assert_eq!(drive.effective_cell_source("c0").as_deref(), Some("cam-b"));
}

// ---------------------------------------------------------------------------
// (a) AUDIO route applies through the drain (the bus channel reads the new store).
// ---------------------------------------------------------------------------

#[test]
fn route_audio_applies_repointing_the_bus_channel() {
    let format = AudioFormat::new(48_000, ChannelLayout::Stereo);
    let fps = Rational::FPS_50;
    let mut bus = ProgramBus::new(format, fps);

    // The bus channel currently reads "old" (silence); "new" will carry a tone.
    let old_store = Arc::new(AudioStore::new(format, 8192));
    old_store
        .publish(&AudioBlock::silence(format, 4096))
        .unwrap();
    let new_store = Arc::new(AudioStore::new(format, 8192));
    let point = bus.add_source("prog", Arc::clone(&old_store), 1.0);

    // Inventory for cam-b's audio + resolution to the NEW store.
    let mut inventories = HashMap::new();
    inventories.insert(
        "cam-b".to_owned(),
        StreamInventory::from_streams(vec![audio_stream("eng", 2, 300)]).with_input_id("cam-b"),
    );
    let mut resolution = RouteResolution::new(inventories);
    resolution.set_audio_store(
        &StreamRef::best("cam-b", StreamKind::Audio),
        Arc::clone(&new_store),
    );
    resolution.set_audio_channel("prog", point);

    let mut applier = RouteApplier::new(&resolution);
    applier
        .apply_audio(
            &mut bus,
            &[RouteIntent::Audio {
                target: "prog".to_owned(),
                source: StreamRef::best("cam-b", StreamKind::Audio),
                gain_db: 0.0,
                mute: false,
            }],
            0, // hard cut (ramp_frames = 0) for a deterministic assertion
        )
        .expect("audio route applies");

    // The re-point seeks the new store to its live edge (RT-8a) so the seam reads
    // fresh audio. Fresh tone now arrives on the new store; the next mix pulls it.
    new_store
        .publish(&AudioBlock::from_interleaved(format, vec![0.5_f32; 4096 * 2]).unwrap())
        .unwrap();

    // After the re-point, the bus pulls from the NEW store: a non-silent block.
    let block = bus.tick();
    let peak = block
        .interleaved()
        .iter()
        .fold(0.0_f32, |m, s| m.max(s.abs()));
    assert!(
        peak > 0.1,
        "RouteAudio must re-point the bus channel to the new store (got peak {peak})"
    );
}

// ---------------------------------------------------------------------------
// (a) AUDIO route honours `gain_db` / `mute` (ADR-0059 §4): the applied route
// carries its gain onto the strip and a mute silences it WITHOUT unrouting it,
// preserving the gain for unmute.
// ---------------------------------------------------------------------------

/// Peak absolute sample in a block.
fn peak(block: &AudioBlock) -> f32 {
    block
        .interleaved()
        .iter()
        .fold(0.0_f32, |m, s| m.max(s.abs()))
}

/// Whether every sample in the block is finite (no `NaN`, no `±inf`).
///
/// `peak` alone cannot see a poisoned block: `f32::max` ignores a `NaN`
/// operand, so an all-`NaN` program mix would still report a peak of `0.0`.
fn all_finite(block: &AudioBlock) -> bool {
    block.interleaved().iter().all(|s| s.is_finite())
}

#[test]
fn route_audio_honours_gain_and_mute_without_unrouting() {
    let format = AudioFormat::new(48_000, ChannelLayout::Stereo);
    let fps = Rational::FPS_50;
    let mut bus = ProgramBus::new(format, fps);
    let store = Arc::new(AudioStore::new(format, 8192));
    let point = bus.add_source("prog", Arc::clone(&store), 1.0);

    let mut inventories = HashMap::new();
    inventories.insert(
        "cam-b".to_owned(),
        StreamInventory::from_streams(vec![audio_stream("eng", 2, 300)]).with_input_id("cam-b"),
    );
    let mut resolution = RouteResolution::new(inventories);
    resolution.set_audio_store(
        &StreamRef::best("cam-b", StreamKind::Audio),
        Arc::clone(&store),
    );
    resolution.set_audio_channel("prog", point);

    let route = |gain_db: f32, mute: bool| RouteIntent::Audio {
        target: "prog".to_owned(),
        source: StreamRef::best("cam-b", StreamKind::Audio),
        gain_db,
        mute,
    };
    let tone = || AudioBlock::from_interleaved(format, vec![0.8_f32; 4096 * 2]).unwrap();
    let mut applier = RouteApplier::new(&resolution);

    // -6.0206 dB ≈ 0.5 linear, unmuted: the applied route sets the strip gain.
    applier
        .apply_audio(&mut bus, &[route(-6.0206, false)], 0)
        .expect("audio route applies");
    assert!(
        (bus.program_gain(point).unwrap() - 0.5).abs() < 1e-3,
        "gain_db must be applied as a linear route gain (got {:?})",
        bus.program_gain(point)
    );
    assert!(!bus.is_muted(point));

    // Fresh tone, then a tick: the contribution is scaled by the applied gain.
    store.publish(&tone()).unwrap();
    let scaled = bus.tick();
    assert!(
        (peak(&scaled) - 0.4).abs() < 0.02,
        "the applied gain scales the bus contribution (got peak {})",
        peak(&scaled)
    );

    // Mute the same route: still routed, gain preserved, bus goes silent once
    // the anti-click envelope has settled (the mute ramp is ≤ one tick).
    applier
        .apply_audio(&mut bus, &[route(-6.0206, true)], 0)
        .expect("muted audio route applies");
    assert!(bus.is_muted(point), "mute keeps the strip routed");
    assert!(
        (bus.program_gain(point).unwrap() - 0.5).abs() < 1e-3,
        "mute preserves the prior gain for unmute (got {:?})",
        bus.program_gain(point)
    );
    store.publish(&tone()).unwrap();
    let _ = bus.tick(); // the anti-click envelope (480 frames < 960)
    let muted = bus.tick();
    assert!(
        peak(&muted) < 1e-6,
        "a muted route contributes silence (got peak {})",
        peak(&muted)
    );

    // Unmute restores the prior gain (the mute never destroyed it).
    applier
        .apply_audio(&mut bus, &[route(-6.0206, false)], 0)
        .expect("unmuted audio route applies");
    store.publish(&tone()).unwrap();
    let _ = bus.tick();
    let restored = bus.tick();
    assert!(
        (peak(&restored) - 0.4).abs() < 0.02,
        "unmute restores the prior gain (got peak {})",
        peak(&restored)
    );
}

#[test]
fn route_audio_mute_silences_outgoing_crossfade_strip_after_short_envelope() {
    let format = AudioFormat::new(48_000, ChannelLayout::Stereo);
    let mut bus = ProgramBus::new(format, Rational::new(100, 1));
    let old = Arc::new(AudioStore::new(format, 16_000));
    let incoming = Arc::new(AudioStore::new(format, 16_000));
    let tone = || AudioBlock::from_interleaved(format, vec![0.5_f32; 12_000 * 2]).unwrap();
    old.publish(&tone()).unwrap();
    incoming.publish(&tone()).unwrap();
    let point = bus.add_source("prog", Arc::clone(&old), 0.8);
    let previous = bus.tick();

    let mut inventories = HashMap::new();
    inventories.insert(
        "cam-b".to_owned(),
        StreamInventory::from_streams(vec![audio_stream("eng", 2, 300)]).with_input_id("cam-b"),
    );
    let mut resolution = RouteResolution::new(inventories);
    resolution.set_audio_store(
        &StreamRef::best("cam-b", StreamKind::Audio),
        Arc::clone(&incoming),
    );
    resolution.set_audio_channel("prog", point);
    let mut applier = RouteApplier::new(&resolution);
    let route = |mute| RouteIntent::Audio {
        target: "prog".to_owned(),
        source: StreamRef::best("cam-b", StreamKind::Audio),
        gain_db: -1.9382, // 0.8 linear
        mute,
    };
    applier.apply_audio(&mut bus, &[route(true)], 4_800).unwrap();
    incoming.publish(&tone()).unwrap(); // repoint seeks the live edge
    let mut samples = Vec::new();
    for _ in 0..4 {
        samples.extend(bus.tick().interleaved().iter().step_by(2).copied());
    }
    assert!(
        samples[480..].iter().all(|&sample| sample == 0.0),
        "route mute must silence every outgoing crossfade sample after the 480-frame envelope"
    );
    let seam = (samples[0] - previous.interleaved().last().copied().unwrap()).abs();
    let max_step = samples.windows(2).map(|w| (w[1] - w[0]).abs()).fold(seam, f32::max);
    assert!(max_step < 5e-3, "mute envelope must not introduce a click: {max_step}");

    // A subsequent route unmute must reopen all remaining crossfade strips.
    applier.apply_audio(&mut bus, &[route(false)], 0).unwrap();
    assert!((bus.program_gain(point).unwrap() - 0.8).abs() < 1e-3);
    let reopened = bus.tick();
    assert!(peak(&reopened) > 0.4);
}

/// The `AudioRoute` fixture for the out-of-range-gain regressions.
fn overflow_route(gain_db: f32, mute: bool) -> RouteIntent {
    RouteIntent::Audio {
        target: "prog".to_owned(),
        source: StreamRef::best("cam-b", StreamKind::Audio),
        gain_db,
        mute,
    }
}

#[test]
fn route_audio_never_lets_an_out_of_range_gain_poison_a_muted_mix() {
    // Cross-vendor review finding (SW-13b): `gain_db = 10000` is finite, so it
    // passed the old finiteness-only guard, yet `10^(10000/20)` overflows to
    // `+inf`; once the mute envelope settles, `inf * 0 = NaN` and the whole
    // program mix goes `NaN` instead of silent. Defence in depth: the apply path
    // must clamp a bad gain to the supported range whatever the caller sent.
    let format = AudioFormat::new(48_000, ChannelLayout::Stereo);
    let fps = Rational::FPS_50;
    let mut bus = ProgramBus::new(format, fps);
    let store = Arc::new(AudioStore::new(format, 8192));
    let point = bus.add_source("prog", Arc::clone(&store), 1.0);

    let mut inventories = HashMap::new();
    inventories.insert(
        "cam-b".to_owned(),
        StreamInventory::from_streams(vec![audio_stream("eng", 2, 300)]).with_input_id("cam-b"),
    );
    let mut resolution = RouteResolution::new(inventories);
    resolution.set_audio_store(
        &StreamRef::best("cam-b", StreamKind::Audio),
        Arc::clone(&store),
    );
    resolution.set_audio_channel("prog", point);

    let mut applier = RouteApplier::new(&resolution);
    applier
        .apply_audio(&mut bus, &[overflow_route(10_000.0, true)], 0)
        .expect("an out-of-range audio route still applies (clamped)");

    store
        .publish(&AudioBlock::from_interleaved(format, vec![0.8_f32; 4096 * 2]).unwrap())
        .unwrap();
    let _ = bus.tick(); // settle the anti-click mute envelope
    let muted = bus.tick();
    assert!(
        all_finite(&muted),
        "a bad route gain must never yield a non-finite program sample"
    );
    assert!(
        peak(&muted) < 1e-6,
        "the muted strip must stay exactly silent (got peak {})",
        peak(&muted)
    );

    // A finite gain is stored, so the strip math can never produce `inf * 0`.
    let gain = bus.program_gain(point).expect("the strip stays routed");
    assert!(
        gain.is_finite(),
        "an out-of-range gain_db must not store a non-finite linear gain (got {gain})"
    );
}

#[test]
fn route_audio_clamps_an_out_of_range_gain_to_the_supported_maximum() {
    // The unmuted half of the same finding: a finite-but-absurd `gain_db` must
    // clamp to the documented supported maximum, never reach the mixer as `inf`.
    let format = AudioFormat::new(48_000, ChannelLayout::Stereo);
    let fps = Rational::FPS_50;
    let mut bus = ProgramBus::new(format, fps);
    let store = Arc::new(AudioStore::new(format, 8192));
    let point = bus.add_source("prog", Arc::clone(&store), 1.0);

    let mut inventories = HashMap::new();
    inventories.insert(
        "cam-b".to_owned(),
        StreamInventory::from_streams(vec![audio_stream("eng", 2, 300)]).with_input_id("cam-b"),
    );
    let mut resolution = RouteResolution::new(inventories);
    resolution.set_audio_store(
        &StreamRef::best("cam-b", StreamKind::Audio),
        Arc::clone(&store),
    );
    resolution.set_audio_channel("prog", point);

    let mut applier = RouteApplier::new(&resolution);
    applier
        .apply_audio(&mut bus, &[overflow_route(10_000.0, false)], 0)
        .expect("an out-of-range audio route still applies (clamped)");

    // `+24.0 dB` ⇒ 10^(24/20) ≈ 15.849 linear — the documented supported maximum.
    let max_linear = 10.0_f64.powf(24.0 / 20.0);
    let gain = bus.program_gain(point).unwrap();
    assert!(
        gain.is_finite(),
        "the applied gain must be finite (got {gain})"
    );
    assert!(
        gain <= max_linear + 1e-9,
        "an out-of-range gain_db must clamp to the supported maximum (got {gain}, max {max_linear})"
    );

    store
        .publish(&AudioBlock::from_interleaved(format, vec![0.8_f32; 4096 * 2]).unwrap())
        .unwrap();
    let out = bus.tick();
    assert!(all_finite(&out), "the program mix must stay finite");
    assert!(
        peak(&out) <= 1.0,
        "the program mix is still clamped to full scale (got peak {})",
        peak(&out)
    );
}

// ---------------------------------------------------------------------------
// (a) SUBTITLE route applies through the drain (the layer samples the new cue).
// ---------------------------------------------------------------------------

#[test]
fn route_subtitle_applies_repointing_the_layer() {
    let mut layer = SubtitleLayer::new(Arc::new(ConstCue("OLD")) as Arc<dyn CueSource>);

    let mut inventories = HashMap::new();
    inventories.insert(
        "cam-c".to_owned(),
        StreamInventory::from_streams(vec![subtitle_stream(500)]).with_input_id("cam-c"),
    );
    let mut resolution = RouteResolution::new(inventories);
    resolution.set_cue_source(
        &StreamRef::best("cam-c", StreamKind::Subtitle),
        Arc::new(ConstCue("NEW")) as Arc<dyn CueSource>,
    );

    // The applier re-points the layer via `&SubtitleLayer` (repoint takes &self),
    // scoped so the immutable borrow ends before the mutable `sample` below.
    {
        let mut layers: HashMap<String, &SubtitleLayer> = HashMap::new();
        layers.insert("subs".to_owned(), &layer);
        let mut applier = RouteApplier::new(&resolution);
        applier
            .apply_subtitle(
                &layers,
                &[RouteIntent::Subtitle {
                    layer: "subs".to_owned(),
                    source: StreamRef::best("cam-c", StreamKind::Subtitle),
                }],
            )
            .expect("subtitle route applies");
    }

    // The next sample shows the NEW source's cue (the layer was re-pointed).
    let cue = layer.sample(MediaTime::from_nanos(1_000_000));
    assert_eq!(cue.map(|c| c.text()).as_deref(), Some("NEW"));
}

// ---------------------------------------------------------------------------
// (f) Selector resolution: Best / Index / Language resolve against the inventory.
// ---------------------------------------------------------------------------

#[test]
fn selector_resolution_picks_the_right_stream() {
    // Two audio tracks: index 0 = French, index 1 = English.
    let inv = StreamInventory::from_streams(vec![
        audio_stream("fra", 2, 300),
        audio_stream("eng", 6, 301),
    ])
    .with_input_id("cam-b");

    // Best → the container default (here, the first), index 0 (French). The
    // BCP-47 newtype lower-cases but preserves the alpha-3 primary subtag.
    let best =
        multiview_engine::route::resolve_selector(&inv, StreamKind::Audio, &StreamSelector::Best)
            .expect("best resolves");
    assert_eq!(best.language.as_ref().map(Bcp47::as_str), Some("fra"));

    // Index 1 → the second audio track (English, 6ch).
    let idx = multiview_engine::route::resolve_selector(
        &inv,
        StreamKind::Audio,
        &StreamSelector::index(1),
    )
    .expect("index resolves");
    assert_eq!(idx.detail.audio_layout(), Some((6, 48_000)));

    // Language "eng" → the English track regardless of position.
    let lang = multiview_engine::route::resolve_selector(
        &inv,
        StreamKind::Audio,
        &StreamSelector::language("eng".to_owned()),
    )
    .expect("language resolves");
    assert_eq!(lang.language.as_ref().map(Bcp47::as_str), Some("eng"));

    // A language with no match resolves to None (deferred-resolution honesty).
    assert!(multiview_engine::route::resolve_selector(
        &inv,
        StreamKind::Audio,
        &StreamSelector::language("deu".to_owned()),
    )
    .is_none());
}

// ---------------------------------------------------------------------------
// Coalescing: a salvo storm onto the same cell applies the LAST source once.
// ---------------------------------------------------------------------------

#[test]
fn coalesces_a_batch_to_one_repoint_per_cell() {
    let (w, h) = (32, 32);
    let store_a = Arc::new(TileStore::<Nv12Image>::with_defaults("cam-a"));
    let store_b = Arc::new(TileStore::<Nv12Image>::with_defaults("cam-b"));
    let store_c = Arc::new(TileStore::<Nv12Image>::with_defaults("cam-c"));
    store_a.publish(solid(w, h, 40), MediaTime::ZERO);
    store_b.publish(solid(w, h, 120), MediaTime::ZERO);
    store_c.publish(solid(w, h, 220), MediaTime::ZERO);
    let mut stores = HashMap::new();
    stores.insert("cam-a".to_owned(), store_a);
    stores.insert("cam-b".to_owned(), store_b);
    stores.insert("cam-c".to_owned(), store_c);

    let mut drive = CompositorDrive::new(
        Arc::new(one_cell_layout(w, h, "cam-a")),
        stores,
        nosignal_card(w, h),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap()
    .with_cell_ids(vec![Some("c0".to_owned())]);

    let mut inventories = HashMap::new();
    for id in ["cam-b", "cam-c"] {
        inventories.insert(
            id.to_owned(),
            StreamInventory::from_streams(vec![video_stream("h264", 256)]).with_input_id(id),
        );
    }
    let mut resolution = RouteResolution::new(inventories);
    resolution.set_video_store_key(&StreamRef::best("cam-b", StreamKind::Video), "cam-b");
    resolution.set_video_store_key(&StreamRef::best("cam-c", StreamKind::Video), "cam-c");

    let mut applier = RouteApplier::new(&resolution);
    let applied = applier
        .apply_video(
            &mut drive,
            &[
                RouteIntent::Video {
                    cell: "c0".to_owned(),
                    source: StreamRef::best("cam-b", StreamKind::Video),
                },
                RouteIntent::Video {
                    cell: "c0".to_owned(),
                    source: StreamRef::best("cam-c", StreamKind::Video),
                },
            ],
        )
        .expect("batch applies");
    // Only ONE effective re-point happened (the last write wins) — coalesced.
    assert_eq!(applied, 1, "a coalesced batch is one re-point per cell");
    assert_eq!(drive.effective_cell_source("c0").as_deref(), Some("cam-c"));
}
