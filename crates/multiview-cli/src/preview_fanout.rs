//! Optional preview workers over the production bake's immutable canvas.
//!
//! This is the bounded worker substrate, not an advertised ladder. A caller must
//! join its typed video events with shared encoded audio and retain registry
//! ownership before it can publish an optional output.
use crate::pipeline::PipelineError;
use crate::rendition_workers::{
    BakedTick, DispatchEvent, OfferRejection, RenditionWorker, ReorderBound, VideoEncodeWorker,
};
use multiview_compositor::pipeline::Nv12Image;
use multiview_core::time::Rational;
use multiview_ffmpeg::EncodedPacket;
use std::sync::Arc;

/// Maximum substituted optional ticks after a short admission gap.
const MAX_SUBSTITUTIONS: u64 = 4;

/// Exact mapping from the immutable production clock to one optional clock.
/// Fractional rates never use floating point and optional rates cannot exceed
/// production. The first tick of each optional frame interval wins.
pub(crate) struct PreviewClock {
    production: Rational,
    preview: Rational,
    next: u64,
}
impl PreviewClock {
    pub(crate) fn new(production: Rational, preview: Rational) -> Option<Self> {
        if production.num <= 0
            || production.den <= 0
            || preview.num <= 0
            || preview.den <= 0
            || i128::from(preview.num) * i128::from(production.den)
                > i128::from(production.num) * i128::from(preview.den)
        {
            return None;
        }
        Some(Self {
            production,
            preview,
            next: 0,
        })
    }
    pub(crate) fn sample(&mut self, tick: u64) -> Option<u64> {
        let index = u128::from(tick)
            .checked_mul(u128::try_from(self.production.den).ok()?)?
            .checked_mul(u128::try_from(self.preview.num).ok()?)?
            .checked_div(
                u128::try_from(self.production.num)
                    .ok()?
                    .checked_mul(u128::try_from(self.preview.den).ok()?)?,
            )?;
        let index = u64::try_from(index).ok()?;
        if index < self.next {
            return None;
        }
        self.next = index.checked_add(1)?;
        Some(index)
    }
}

struct Encoder {
    lease: crate::preview_runtime::PreviewEncoderLease,
    next: u64,
    last: Option<Arc<Nv12Image>>,
    scaler: Option<multiview_ffmpeg::Scaler>,
}
impl Encoder {
    fn encode_one(
        &mut self,
        canvas: &Nv12Image,
        tick: u64,
    ) -> Result<Vec<EncodedPacket>, PipelineError> {
        let mut frame = crate::pipeline::nv12_to_decoded(canvas)?;
        let (width, height) = self
            .lease
            .encoder
            .codec_params()
            .video_dimensions()
            .ok_or_else(|| PipelineError::Output {
                kind: "optional-preview",
                reason: "missing opened geometry".into(),
            })?;
        if frame.frame.width() != width || frame.frame.height() != height {
            use multiview_ffmpeg::{ScaleSpec, Scaler};
            if self.scaler.is_none() {
                self.scaler = Some(
                    Scaler::new(
                        ScaleSpec::new(
                            frame.frame.format(),
                            frame.frame.width(),
                            frame.frame.height(),
                        ),
                        ScaleSpec::new(frame.frame.format(), width, height),
                    )
                    .map_err(|e| PipelineError::Output {
                        kind: "optional-preview",
                        reason: e.to_string(),
                    })?,
                );
            }
            frame.frame = self
                .scaler
                .as_mut()
                .ok_or_else(|| PipelineError::Output {
                    kind: "optional-preview",
                    reason: "scaler unavailable".into(),
                })?
                .run(&frame.frame)
                .map_err(|e| PipelineError::Output {
                    kind: "optional-preview",
                    reason: e.to_string(),
                })?;
        }
        self.lease
            .encode_frame_at(frame, tick)
            .map_err(|e| PipelineError::Output {
                kind: "optional-preview",
                reason: e.to_string(),
            })
    }
}
impl VideoEncodeWorker for Encoder {
    fn encode(&mut self, item: BakedTick) -> Result<Vec<EncodedPacket>, PipelineError> {
        if item.tick < self.next || item.tick - self.next > MAX_SUBSTITUTIONS {
            return Err(PipelineError::Output {
                kind: "optional-preview",
                reason: "optional admission gap exceeds bounded substitution".into(),
            });
        }
        let mut packets = Vec::new();
        while self.next < item.tick {
            let canvas = self.last.clone().unwrap_or_else(|| item.canvas.clone());
            packets.extend(self.encode_one(&canvas, self.next)?);
            self.next += 1;
        }
        packets.extend(self.encode_one(&item.canvas, item.tick)?);
        self.next = item.tick.saturating_add(1);
        self.last = Some(item.canvas);
        Ok(packets)
    }
    fn reorder_bound(&self) -> ReorderBound {
        if self.lease.encoder.no_reorder_proven() {
            ReorderBound::NoReorder
        } else {
            ReorderBound::Unknown
        }
    }
    fn finish(&mut self) -> Result<Vec<EncodedPacket>, PipelineError> {
        self.lease.finish().map_err(|e| PipelineError::Output {
            kind: "optional-preview",
            reason: e.to_string(),
        })
    }
}

/// One worker per distinct admitted optional key, never per subscriber. The
/// owner must only pass this handle the already-baked production Arc. Overflow
/// loses optional work only; the encoder substitutes at most four omitted ticks
/// and fails this worker alone if the gap exceeds that bound.
pub(crate) struct PreviewWorker {
    clock: PreviewClock,
    worker: RenditionWorker,
}
impl PreviewWorker {
    pub(crate) fn spawn(
        index: usize,
        production: Rational,
        cadence: Rational,
        mut lease: crate::preview_runtime::PreviewEncoderLease,
        mut on_event: impl FnMut(DispatchEvent) -> Result<(), PipelineError> + Send + 'static,
    ) -> Result<Self, PipelineError> {
        let clock =
            PreviewClock::new(production, cadence).ok_or_else(|| PipelineError::Output {
                kind: "optional-preview",
                reason: "invalid optional cadence".into(),
            })?;
        let timebase = lease.encoder.time_base();
        let primed = lease.initial_packets.take();
        let next = u64::from(primed.is_some());
        if let Some(packets) = primed {
            on_event(DispatchEvent::Video {
                key_index: index,
                tick: 0,
                timebase,
                packets,
                watermark: if lease.encoder.no_reorder_proven() {
                    crate::rendition_workers::next_tick_lower_bound_ns(0, cadence)
                } else {
                    None
                },
            })?;
        }
        let worker = RenditionWorker::spawn(
            index,
            cadence,
            timebase,
            Box::new(Encoder {
                lease,
                next,
                last: None,
                scaler: None,
            }),
            on_event,
        )?;
        let mut clock = clock;
        clock.next = next;
        Ok(Self { clock, worker })
    }
    pub(crate) fn offer(
        &mut self,
        tick: u64,
        canvas: &Arc<Nv12Image>,
    ) -> Result<(), OfferRejection> {
        let Some(tick) = self.clock.sample(tick) else {
            return Ok(());
        };
        self.worker.offer(BakedTick {
            tick,
            canvas: Arc::clone(canvas),
        })
    }
    pub(crate) fn join(self) -> Result<(), PipelineError> {
        self.worker.join()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rational_optional_ticks_do_not_change_production_cadence() {
        let mut clock =
            PreviewClock::new(Rational::new(30_000, 1001), Rational::new(15_000, 1001)).unwrap();
        let ticks: Vec<_> = (0..180).filter_map(|tick| clock.sample(tick)).collect();
        assert_eq!(ticks, (0..90).collect::<Vec<_>>());
        assert_eq!(clock.sample(179), None);
    }
    fn actual_encoder() -> Encoder {
        use multiview_config::preview_ladder::{
            OpenedPreview, PreviewKey, PreviewOpener, ResourceCost,
        };
        struct NoPublication;
        impl crate::preview_runtime::OptionalRegistry for NoPublication {
            type Registration = ();
            fn publish_optional(&mut self, _: &[OpenedPreview]) -> Option<()> {
                None
            }
        }
        let key = PreviewKey {
            program: "main".into(),
            bus: "main".into(),
            codec: "mpeg2video".into(),
            encoder_name: "mpeg2video".into(),
            width: 160,
            height: 120,
            cadence: Rational::new(30_000, 1001),
            bitrate_bps: 1_000_000,
            codec_profile: None,
            pixel_format: "nv12".into(),
            color: "bt709-limited".into(),
            gpu_pin: None,
            codec_config: None,
        };
        let mut opener = crate::preview_runtime::PreviewRuntimeOpener::new(
            vec![(key.clone(), ResourceCost::default())],
            NoPublication,
        );
        Encoder {
            lease: opener.open(&key).unwrap(),
            next: 0,
            last: None,
            scaler: None,
        }
    }
    fn canvas() -> Arc<Nv12Image> {
        Arc::new(
            Nv12Image::solid(
                160,
                120,
                16,
                128,
                128,
                multiview_core::color::ColorInfo::default(),
            )
            .unwrap(),
        )
    }
    #[test]
    fn actual_encoder_substitutes_only_bounded_gap_and_restamps_every_tick() {
        let mut encoder = actual_encoder();
        let canvas = canvas();
        let mut packets = encoder
            .encode(BakedTick {
                tick: 0,
                canvas: canvas.clone(),
            })
            .unwrap();
        packets.extend(
            encoder
                .encode(BakedTick {
                    tick: 4,
                    canvas: canvas.clone(),
                })
                .unwrap(),
        );
        packets.extend(encoder.finish().unwrap());
        assert_eq!(
            packets
                .iter()
                .filter_map(EncodedPacket::pts)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4]
        );
        assert!(packets.iter().all(|packet| packet.dts() == packet.pts()));
        assert_eq!(Arc::strong_count(&canvas), 2); // test + last-good, never a per-preview composition
    }
    #[test]
    fn actual_encoder_excessive_optional_gap_fails_without_advancing_encoder() {
        let mut encoder = actual_encoder();
        assert!(encoder
            .encode(BakedTick {
                tick: 5,
                canvas: canvas()
            })
            .is_err());
        assert_eq!(encoder.next, 0);
        assert!(encoder
            .encode(BakedTick {
                tick: 0,
                canvas: canvas()
            })
            .is_ok());
    }
    #[test]
    fn invalid_or_faster_optional_clock_fails_closed() {
        assert!(PreviewClock::new(Rational::new(30_000, 1001), Rational::new(60, 1)).is_none());
        assert!(PreviewClock::new(Rational::new(0, 1), Rational::new(1, 1)).is_none());
    }
}
