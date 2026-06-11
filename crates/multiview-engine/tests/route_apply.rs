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
