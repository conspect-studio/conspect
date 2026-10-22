//! The engine-backed live-preview provider for the control plane.
//!
//! Implements [`multiview_control::PreviewProvider`] by sampling, isolation-safe
//! (invariant #10):
//! * the **program** — a wait-free latest-frame slot ([`ProgramSlot`]) the engine
//!   loop publishes a throttled clone of the composited canvas into; and
//! * each **input** — the lock-free per-source
//!   [`TileStore`](multiview_framestore::TileStore), read at the current
//!   wall-clock instant.
//!
//! NV12→JPEG encoding (via [`multiview_preview::Nv12JpegEncoder`]) runs on the
//! request task, never on the output-clock loop. No path here blocks the engine.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwapOption;
use multiview_audio::format::AudioBlock;
use multiview_compositor::pipeline::Nv12Image;
use multiview_control::PreviewProvider;
use multiview_core::time::MediaTime;
use multiview_engine::{MonotonicTimeSource, TimeSource};
use multiview_preview::{JpegEncoder, Nv12JpegEncoder};

use crate::live_sources::SharedStores;

/// The wait-free slot the engine loop publishes the latest composited program
/// frame into (throttled to the preview rate). Cloned into both the engine
/// (writer) and the provider (reader); neither blocks the other.
pub type ProgramSlot = Arc<ArcSwapOption<Nv12Image>>;

/// Create an empty program slot (no frame published yet).
#[must_use]
pub fn program_slot() -> ProgramSlot {
    Arc::new(ArcSwapOption::empty())
}

/// The ring depth of the program-audio preview slot — a handful of blocks
/// (ADR-P001 shallow preview ring). At a ~25–50 Hz tick that is well under a
/// second of audio: enough that a WHEP driver pumping at its 20 ms cadence never
/// misses a block between samples, shallow enough that a stalled consumer holds
/// only a trivial bounded buffer.
const PROGRAM_AUDIO_RING: usize = 8;

/// A bounded, **drop-oldest** preview tap of the **post-loudnorm program PCM**
/// (ADR-P006 audio): the bake consumer pushes each emitted [`AudioBlock`] (the
/// exact block the stream encodes + the display heads hear) and the WHEP egress
/// provider drains it, Opus-encodes, and feeds the peer.
///
/// ## Isolation (invariant #1 / #10)
///
/// This is a preview tap, **never** on the engine hot loop: the bake consumer
/// pushes off the output-clock thread, and a slow or absent WHEP consumer only
/// loses the oldest blocks — the push is wait-free-bounded and **can never
/// back-pressure** the consumer, the encode, or the engine. Cloneable (an `Arc`
/// over the ring); the producer and consumer hold short, non-overlapping critical
/// sections.
#[derive(Clone, Debug, Default)]
pub struct ProgramAudioSlot {
    ring: Arc<Mutex<VecDeque<AudioBlock>>>,
}

impl ProgramAudioSlot {
    /// A fresh empty slot.
    #[must_use]
    pub fn new() -> Self {
        Self {
            ring: Arc::new(Mutex::new(VecDeque::with_capacity(PROGRAM_AUDIO_RING))),
        }
    }

    /// Push one emitted program block, evicting the oldest if the ring is full
    /// (drop-oldest). Non-blocking; a poisoned lock is a no-op (the tap is
    /// best-effort — losing a preview block never affects the program). Returns
    /// `true` if a block was evicted (the consumer is behind) — a "the consumer is
    /// falling behind" signal a caller that does not care can bind to `_`.
    #[must_use]
    pub fn push(&self, block: AudioBlock) -> bool {
        let Ok(mut ring) = self.ring.lock() else {
            return false;
        };
        let mut evicted = false;
        while ring.len() >= PROGRAM_AUDIO_RING {
            ring.pop_front();
            evicted = true;
        }
        ring.push_back(block);
        evicted
    }

    /// Pop the oldest queued block, or `None` if the slot is empty. Non-blocking;
    /// draining slowly only drops the oldest at the producer — never back-pressures
    /// it (invariant #10).
    #[must_use]
    pub fn pop(&self) -> Option<AudioBlock> {
        self.ring.lock().ok().and_then(|mut r| r.pop_front())
    }

    /// The number of blocks currently queued (tests / telemetry).
    #[must_use]
    pub fn len(&self) -> usize {
        self.ring.lock().map_or(0, |r| r.len())
    }

    /// Whether the slot is currently empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Engine-backed [`PreviewProvider`]: encodes the latest program + per-input
/// frames to JPEG on demand.
pub struct CliPreviewProvider {
    program: ProgramSlot,
    /// The **live-updatable** per-input store map (ADR-W018): the run seeds it
    /// with the startup sources and the live-source hub RCUs additions/removals
    /// in, so live-added inputs are previewable. Reads are wait-free snapshots.
    stores: SharedStores,
    encoder: Nv12JpegEncoder,
    clock: MonotonicTimeSource,
}

impl CliPreviewProvider {
    /// Build a provider reading the program `slot` and the shared per-input
    /// `stores` map (see [`crate::live_sources::shared_stores`]).
    #[must_use]
    pub fn new(program: ProgramSlot, stores: SharedStores) -> Self {
        Self {
            program,
            stores,
            encoder: Nv12JpegEncoder::new(),
            clock: MonotonicTimeSource::new(),
        }
    }

    /// Encode one NV12 image to a JPEG, packing the Y and interleaved Cb/Cr
    /// planes into the contiguous NV12 buffer the encoder expects.
    fn encode(&self, image: &Nv12Image, quality: u8) -> Option<Vec<u8>> {
        let y = image.y_plane();
        let uv = image.uv_plane();
        let mut plane = Vec::with_capacity(y.len().saturating_add(uv.len()));
        plane.extend_from_slice(y);
        plane.extend_from_slice(uv);
        self.encoder
            .encode_nv12(&plane, image.width(), image.height(), quality)
            .ok()
    }
}

impl PreviewProvider for CliPreviewProvider {
    fn program_jpeg(&self, quality: u8) -> Option<Vec<u8>> {
        let frame = self.program.load_full()?;
        self.encode(&frame, quality)
    }

    fn input_jpeg(&self, id: &str, quality: u8) -> Option<Vec<u8>> {
        let stores = self.stores.load();
        let store = stores.get(id)?;
        let now = MediaTime::from_nanos(self.clock.now_nanos());
        let read = store.read_at(now);
        let frame = read.frame()?;
        self.encode(frame, quality)
    }

    fn input_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.stores.load().keys().cloned().collect();
        ids.sort();
        ids
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
    use super::*;
    use multiview_audio::format::{AudioFormat, ChannelLayout};
    use multiview_core::color::ColorInfo;

    fn color() -> ColorInfo {
        ColorInfo::default().resolve_defaults(64, 64)
    }

    fn block(format: AudioFormat, value: f32, frames: usize) -> AudioBlock {
        AudioBlock::from_interleaved(format, vec![value; frames * format.channel_count()]).unwrap()
    }

    #[test]
    fn program_audio_slot_delivers_blocks_fifo_then_empties() {
        let fmt = AudioFormat::new(48_000, ChannelLayout::Stereo);
        let slot = ProgramAudioSlot::new();
        assert!(slot.is_empty());
        assert!(slot.pop().is_none(), "empty slot yields None");

        // Exactly-representable f32 sentinels so the FIFO-order assertion is a
        // bit-exact comparison (no float-equality lint trip).
        let (first, second) = (0.25_f32, 0.5_f32);
        assert!(
            !slot.push(block(fmt, first, 960)),
            "first push evicts nothing"
        );
        assert!(!slot.push(block(fmt, second, 960)));
        assert_eq!(slot.len(), 2);
        // FIFO order: oldest out first (bit-exact for the exact sentinels).
        assert_eq!(
            slot.pop().unwrap().interleaved()[0].to_bits(),
            first.to_bits()
        );
        assert_eq!(
            slot.pop().unwrap().interleaved()[0].to_bits(),
            second.to_bits()
        );
        assert!(slot.is_empty());
    }

    #[test]
    fn program_audio_slot_is_bounded_drop_oldest_under_a_stalled_consumer() {
        // INVARIANT #10: a stalled WHEP consumer (never popping) must NEVER let the
        // tap grow or back-pressure the producer. Pushing far more blocks than the
        // ring depth stays bounded and drops the oldest — wait-free, never queued.
        let fmt = AudioFormat::new(48_000, ChannelLayout::Stereo);
        let slot = ProgramAudioSlot::new();
        let started = std::time::Instant::now();
        let mut evicted_any = false;
        for _ in 0..10_000u32 {
            if slot.push(block(fmt, 0.25, 480)) {
                evicted_any = true;
            }
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "pushing into a drop-oldest slot never blocks on a stalled consumer"
        );
        assert!(evicted_any, "a full ring evicts the oldest");
        assert!(
            slot.len() <= PROGRAM_AUDIO_RING,
            "the ring stays bounded (never grows): len {} <= {}",
            slot.len(),
            PROGRAM_AUDIO_RING
        );
    }

    #[test]
    fn program_jpeg_encodes_the_published_frame_and_none_when_empty() {
        let slot = program_slot();
        let provider = CliPreviewProvider::new(
            Arc::clone(&slot),
            crate::live_sources::shared_stores(std::collections::HashMap::new()),
        );
        // Empty slot -> no still.
        assert!(provider.program_jpeg(70).is_none());

        // Publish a solid frame; the provider returns a valid JPEG (SOI marker).
        let frame = Nv12Image::solid(64, 64, 120, 128, 128, color()).unwrap();
        slot.store(Some(Arc::new(frame)));
        let jpeg = provider.program_jpeg(70).expect("a still is produced");
        assert!(
            jpeg.len() > 2 && jpeg[0] == 0xFF && jpeg[1] == 0xD8,
            "JPEG SOI marker"
        );
    }

    #[test]
    fn input_ids_are_sorted_and_unknown_input_is_none() {
        let mut stores = std::collections::HashMap::new();
        for id in ["zeta", "alpha"] {
            stores.insert(
                id.to_owned(),
                Arc::new(multiview_framestore::TileStore::<Nv12Image>::with_defaults(
                    id,
                )),
            );
        }
        let provider =
            CliPreviewProvider::new(program_slot(), crate::live_sources::shared_stores(stores));
        assert_eq!(
            provider.input_ids(),
            vec!["alpha".to_owned(), "zeta".to_owned()]
        );
        // A known-but-empty store yields no still; an unknown id likewise.
        assert!(provider.input_jpeg("alpha", 70).is_none());
        assert!(provider.input_jpeg("nope", 70).is_none());
    }
}
