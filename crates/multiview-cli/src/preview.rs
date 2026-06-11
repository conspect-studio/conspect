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

use std::sync::Arc;

use arc_swap::ArcSwapOption;
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
    use multiview_core::color::ColorInfo;

    fn color() -> ColorInfo {
        ColorInfo::default().resolve_defaults(64, 64)
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
