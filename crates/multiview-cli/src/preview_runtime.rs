//! Off-clock optional preview encoder factory. Never opens per viewer.
//!
//! This module owns one optional `VideoRenditionEncoder` per exact admitted key
//! and publishes only after every opened descriptor is verified. It does NOT
//! mutate the program's fixed production H264/HEVC encoders or hot clock.
#![cfg(feature = "ffmpeg")]
#![allow(dead_code)] // Integration seam: registry optional registration lands separately.
use multiview_config::preview_ladder::{
    OpenedPreview, PreviewKey, PreviewLease, PreviewOpener, ResourceCost,
};
use multiview_output::{EncodeConfig, VideoRenditionEncoder};

/// One optional opened encoder plus its measured reservation lease. The
/// runtime's registry owns this after admission; Drop releases the encoder.
pub struct PreviewEncoderLease {
    descriptor: OpenedPreview,
    /// The opened optional encoder retained for this catalog's lifetime.
    pub encoder: VideoRenditionEncoder,
    producer: std::sync::OnceLock<crate::output_window_registry::OptionalWindowProducer>,
    pub(crate) initial_packets: Option<Vec<multiview_ffmpeg::EncodedPacket>>,
    initial_avc: Option<multiview_ffmpeg::codec_metadata::AvcDecoderConfig>,
    #[cfg(test)]
    before_encode: Option<Box<dyn FnOnce() + Send>>,
    #[cfg(test)]
    before_flush: Option<Box<dyn FnOnce() + Send>>,
    _runtime_permit: Option<std::sync::Arc<crate::preview_vertical::RuntimePermit>>,
}
impl PreviewEncoderLease {
    pub(crate) fn encode_frame_at(
        &mut self,
        frame: multiview_ffmpeg::DecodedVideoFrame,
        tick: u64,
    ) -> multiview_output::Result<Vec<multiview_ffmpeg::EncodedPacket>> {
        #[cfg(test)]
        if let Some(hang) = self.before_encode.take() {
            hang();
        }
        self.encoder.encode_frame_at(frame, tick)
    }

    pub(crate) fn finish(
        &mut self,
    ) -> multiview_output::Result<Vec<multiview_ffmpeg::EncodedPacket>> {
        #[cfg(test)]
        if let Some(hang) = self.before_flush.take() {
            hang();
        }
        self.encoder.finish()
    }
}
impl PreviewLease for PreviewEncoderLease {
    fn descriptor(&self) -> &OpenedPreview {
        &self.descriptor
    }
}
/// Registry publication must atomically bind every opened preview output ID to
/// the exact effective descriptor and `WindowDelivery` producer, or publish none.
/// Only after this returns true may the catalog expose the output IDs.
pub trait OptionalRegistry {
    /// RAII registration guard; dropping it retires exactly these optional IDs.
    type Registration;
    /// Publish every opened descriptor atomically, or publish nothing.
    fn publish_optional(&mut self, opened: &[OpenedPreview]) -> Option<Self::Registration>;
    /// Bind the actual encoder leases when the publisher has a packet ingress.
    fn publish_leases(&mut self, leases: &[PreviewEncoderLease]) -> Option<Self::Registration> {
        self.publish_optional(
            &leases
                .iter()
                .map(|l| l.descriptor.clone())
                .collect::<Vec<_>>(),
        )
    }
}
/// No measured resource reservation means no new encoder is opened. The
/// supplied entries are platform-measured and the pure admission preflight
/// already proved their sum fits AFTER production reservations.
pub struct PreviewRuntimeOpener<R: OptionalRegistry> {
    measured: Vec<(PreviewKey, ResourceCost)>,
    registry: R,
    next_id: u64,
    pub(crate) runtime_permit: Option<std::sync::Arc<crate::preview_vertical::RuntimePermit>>,
    #[cfg(test)]
    before_open: Option<Box<dyn FnOnce() + Send>>,
    pub(crate) first_canvas: Option<std::sync::Arc<multiview_compositor::pipeline::Nv12Image>>,
}
impl<R: OptionalRegistry> PreviewRuntimeOpener<R> {
    /// Construct a factory from measured per-key resource reservations and a
    /// registry publisher; unknown keys cannot open new encoders.
    pub fn new(measured: Vec<(PreviewKey, ResourceCost)>, registry: R) -> Self {
        Self {
            measured,
            registry,
            next_id: 0,
            first_canvas: None,
            runtime_permit: None,
            #[cfg(test)]
            before_open: None,
        }
    }
}
fn preview_codec_name(codec_id: ffmpeg_next::codec::Id) -> Option<&'static str> {
    match codec_id {
        ffmpeg_next::codec::Id::MPEG2VIDEO => Some("mpeg2video"),
        ffmpeg_next::codec::Id::H264 => Some("h264"),
        ffmpeg_next::codec::Id::HEVC => Some("hevc"),
        ffmpeg_next::codec::Id::MJPEG => Some("mjpeg"),
        ffmpeg_next::codec::Id::FFV1 => Some("ffv1"),
        _ => None,
    }
}

impl<R: OptionalRegistry> PreviewOpener for PreviewRuntimeOpener<R> {
    type Lease = PreviewEncoderLease;
    type Registration = R::Registration;
    fn open(&mut self, requested: &PreviewKey) -> Option<Self::Lease> {
        let measured = self.measured.iter().find(|(key, _)| key == requested)?.1;
        if matches!(requested.encoder_name.as_str(), "libx264" | "libx265")
            && !cfg!(feature = "gpl-codecs")
        {
            return None;
        }
        // Never invent a pixel format or silently lower production cadence:
        // this candidate maps only the explicitly supported NV12->YUV420P
        // path, and the final opened parameters are checked below.
        if requested.pixel_format != "nv12" {
            return None;
        }
        let mut config = EncodeConfig::mpeg2(requested.width, requested.height);
        config.codec_name.clone_from(&requested.encoder_name);
        config.cadence = requested.cadence;
        config.bit_rate = usize::try_from(requested.bitrate_bps).ok()?;
        // One-second GOP rounded DOWN at fractional cadence (30000/1001 -> 29).
        // This affects only the optional preview encoder, never production.
        config.gop = u32::try_from(requested.cadence.num.checked_div(requested.cadence.den)?)
            .ok()?
            .max(1);
        config.cuda_ordinal.clone_from(&requested.gpu_pin);
        #[cfg(test)]
        if let Some(hang) = self.before_open.take() {
            hang();
        }
        let mut encoder = VideoRenditionEncoder::new(&config).ok()?;
        let initial_packets = if let Some(canvas) = self.first_canvas.as_ref() {
            let mut frame = crate::pipeline::nv12_to_decoded(canvas).ok()?;
            if (frame.frame.width(), frame.frame.height()) != (requested.width, requested.height) {
                let mut scaler = multiview_ffmpeg::Scaler::new(
                    multiview_ffmpeg::ScaleSpec::new(
                        frame.frame.format(),
                        frame.frame.width(),
                        frame.frame.height(),
                    ),
                    multiview_ffmpeg::ScaleSpec::new(
                        frame.frame.format(),
                        requested.width,
                        requested.height,
                    ),
                )
                .ok()?;
                frame.frame = scaler.run(&frame.frame).ok()?;
            }
            Some(encoder.encode_frame_at(frame, 0).ok()?)
        } else {
            None
        };
        let initial_avc = initial_packets.as_ref().and_then(|packets| {
            packets
                .iter()
                .filter_map(|p| p.payload())
                .find_map(multiview_ffmpeg::codec_metadata::AvcDecoderConfig::from_initial_rap)
        });
        let params = encoder.codec_params();
        let codec = preview_codec_name(params.codec_id())?;
        if codec != requested.codec {
            return None;
        }
        let dimensions = params.video_dimensions()?;
        if requested
            .codec_profile
            .is_some_and(|profile| params.opened_profile() != profile)
        {
            return None;
        }
        if dimensions != (requested.width, requested.height)
            || params.opened_bit_rate() != Some(requested.bitrate_bps)
            || encoder.time_base() != config.time_base()
        {
            return None;
        }
        // Explicit codec configuration is a hard constraint. No record means
        // the codec supplied in-band headers (MPEG-2 may legitimately do so);
        // it is NOT evidence that the encoder has a named profile. The later
        // registry descriptor must publish exactly the opened profile/format,
        // including 'unknown', never a configured substitute.
        if requested.codec_config.is_some() && params.codec_config_bytes() != requested.codec_config
        {
            return None;
        }
        let id = format!("preview:{}:{}", requested.program, self.next_id);
        self.next_id = self.next_id.checked_add(1)?;
        Some(PreviewEncoderLease {
            descriptor: OpenedPreview {
                output_id: id,
                key: requested.clone(),
                cost: measured,
                opened_codec: codec.to_owned(),
                opened_profile: params.opened_profile(),
                opened_level: params.opened_level(),
                opened_pixel_format: params.opened_pixel_format(),
                opened_codec_config: params.codec_config_bytes(),
            },
            encoder,
            producer: std::sync::OnceLock::new(),
            initial_packets,
            initial_avc,
            #[cfg(test)]
            before_encode: None,
            #[cfg(test)]
            before_flush: None,
            _runtime_permit: self.runtime_permit.clone(),
        })
    }
    fn publish(&mut self, staged: &[Self::Lease]) -> Option<Self::Registration> {
        self.registry.publish_leases(staged)
    }
}

/// One bridge per production run. Dependencies come from the effective parent
/// output, not from desired configuration or a synthetic source list.
pub(crate) struct RuntimeRegistry {
    factory: std::sync::Arc<crate::output_window_registry::SubscriptionFactory>,
    publisher: Option<crate::optional_rendition::OptionalRegistryPublisher>,
    parent_output: String,
    pub(crate) opened_parent: Option<crate::output_window_registry::EffectiveBinding>,
    next_index: usize,
}
impl RuntimeRegistry {
    pub(crate) fn new(
        factory: std::sync::Arc<crate::output_window_registry::SubscriptionFactory>,
        parent_output: String,
    ) -> Option<Self> {
        let next_index = factory
            .effective()
            .iter()
            .map(|b| b.output_index)
            .max()
            .map_or(Some(0), |i| i.checked_add(1))?;
        Some(Self {
            publisher: Some(
                crate::optional_rendition::OptionalRegistryPublisher::new(factory.clone()).ok()?,
            ),
            factory,
            parent_output,
            opened_parent: None,
            next_index,
        })
    }
}
pub(crate) struct ReadyRegistration {
    publisher: crate::optional_rendition::OptionalRegistryPublisher,
    staged: Option<Vec<crate::optional_rendition::OptionalRendition>>,
    registration: Option<crate::optional_rendition::OptionalRegistration>,
}
impl ReadyRegistration {
    pub(crate) fn activate(&mut self) -> bool {
        if self.registration.is_some() {
            return true;
        }
        let Some(staged) = self.staged.take() else {
            return false;
        };
        self.registration = self.publisher.register_optional(staged).ok();
        self.registration.is_some()
    }
}
impl OptionalRegistry for RuntimeRegistry {
    type Registration = ReadyRegistration;
    fn publish_optional(&mut self, _: &[OpenedPreview]) -> Option<Self::Registration> {
        None // Descriptor-only publication cannot establish a real packet path.
    }
    fn publish_leases(&mut self, leases: &[PreviewEncoderLease]) -> Option<Self::Registration> {
        use crate::optional_rendition::{OptionalRendition, PreviewScope};
        use crate::output_window_registry::{Dependency, DependencyKind, VideoDescriptor};
        let parent = self.opened_parent.clone().or_else(|| {
            self.factory
                .resolve(&multiview_control::WhepScope::Output(
                    self.parent_output.clone(),
                ))
                .ok()
        })?;
        let mut staged = Vec::new();
        for lease in leases {
            let opened = &lease.descriptor;
            if opened.key.program != parent.program_id || opened.key.bus != parent.bus_id {
                return None;
            }
            let index = self.next_index;
            self.next_index = self.next_index.checked_add(1)?;
            let mut video = VideoDescriptor::from_opened(
                lease.encoder.codec_params(),
                lease.encoder.time_base(),
                if lease.encoder.no_reorder_proven() {
                    crate::rendition_workers::ReorderBound::NoReorder
                } else {
                    crate::rendition_workers::ReorderBound::Unknown
                },
                opened.key.cadence,
            )?;
            if let Some(avc) = &lease.initial_avc {
                video.codec.clone_from(&avc.codec);
                video.config = Some(avc.description.clone());
                video.framing = multiview_ffmpeg::NalFraming::AnnexB;
                video.recovery = Some(multiview_output::batch::RecoveryGate::StrictRap {
                    codec: multiview_ffmpeg::CodecKind::H264,
                    framing: video.framing,
                });
            }
            let producer = self
                .factory
                .optional_producer(index, self.factory.generation());
            lease.producer.set(producer.clone()).ok()?;
            let mut deps = parent.dependencies.clone();
            deps.push(Dependency {
                kind: DependencyKind::Output,
                id: opened.output_id.clone(),
            });
            let mut rendition = OptionalRendition::new(
                PreviewScope {
                    profile_id: opened.output_id.clone(),
                    output_id: opened.output_id.clone(),
                    output_index: index,
                    program_id: parent.program_id.clone(),
                    bus_id: parent.bus_id.clone(),
                },
                video,
                deps,
                Vec::new(),
            )
            .with_producer(producer);
            rendition.audio.clone_from(&parent.audio);
            staged.push(rendition);
        }
        Some(ReadyRegistration {
            publisher: self.publisher.take()?,
            staged: Some(staged),
            registration: None,
        })
    }
}
impl PreviewEncoderLease {
    pub(crate) fn producer(&self) -> Option<crate::output_window_registry::OptionalWindowProducer> {
        self.producer.get().cloned()
    }
    /// Offer a complete actual encoder emission, never fabricated publication.
    /// The caller assembles bounded complete windows off the output clock.
    pub(crate) fn offer_complete(
        &self,
        packets: Vec<multiview_ffmpeg::EncodedPacket>,
        span: std::time::Duration,
        now: std::time::Instant,
    ) -> Result<(), crate::output_window_registry::SubscribeError> {
        self.producer
            .get()
            .ok_or(crate::output_window_registry::SubscribeError::Unavailable)?
            .offer_complete(packets, span, now)
    }
}

#[cfg(all(test, feature = "gpl-codecs"))]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod actual_bridge_tests {
    use super::*;
    use crate::output_window_registry::*;
    use multiview_core::time::Rational;
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };
    struct NoPublication;
    impl OptionalRegistry for NoPublication {
        type Registration = ();
        fn publish_optional(&mut self, _: &[OpenedPreview]) -> Option<()> {
            None
        }
    }

    fn actual_preview_setup() -> (PreviewKey, ResourceCost, EffectiveBinding) {
        let key = PreviewKey {
            program: "main".into(),
            bus: "main".into(),
            codec: "h264".into(),
            encoder_name: "libx264".into(),
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
        let cost = ResourceCost {
            encoder_sessions: 1,
            cpu_units: 1,
            gpu_units: 0,
            memory_bytes: 100,
            uplink_bps: 0,
        };
        let mut config = EncodeConfig::mpeg2(160, 120);
        config.codec_name = "libx264".into();
        let production_encoder = VideoRenditionEncoder::new(&config).unwrap();
        let production = EffectiveBinding {
            program_id: "main".into(),
            bus_id: "main".into(),
            output_id: "production".into(),
            output_index: 42,
            config_generation: 7,
            video: VideoDescriptor::from_opened(
                production_encoder.codec_params(),
                production_encoder.time_base(),
                crate::rendition_workers::ReorderBound::NoReorder,
                key.cadence,
            )
            .unwrap(),
            audio: vec![],
            dependencies: vec![
                Dependency {
                    kind: DependencyKind::Source,
                    id: "real-camera".into(),
                },
                Dependency {
                    kind: DependencyKind::Program,
                    id: "main".into(),
                },
            ],
        };
        (key, cost, production)
    }
    #[test]
    fn hung_native_open_blocks_replacements_until_native_completion() {
        let (key, cost, _) = actual_preview_setup();
        let budget = crate::preview_vertical::PreviewAdmissionBudget::new();
        let permit = budget.acquire().unwrap();
        let mut opener = PreviewRuntimeOpener::new(vec![(key.clone(), cost)], NoPublication);
        opener.runtime_permit = Some(permit);
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        opener.before_open = Some(Box::new(move || {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        }));
        let worker = std::thread::spawn(move || opener.open(&key).unwrap());
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        for _ in 0..100 {
            assert!(budget.acquire().is_none());
        }
        release_tx.send(()).unwrap();
        let lease = worker.join().unwrap();
        assert!(
            budget.acquire().is_none(),
            "opened lease must retain the slot"
        );
        drop(lease);
        assert!(budget.acquire().is_some());
    }

    fn assert_native_worker_hang_quarantines_budget(flush: bool) {
        let (key, cost, _) = actual_preview_setup();
        let budget = crate::preview_vertical::PreviewAdmissionBudget::new();
        let mut opener = PreviewRuntimeOpener::new(vec![(key.clone(), cost)], NoPublication);
        opener.runtime_permit = Some(budget.acquire().unwrap());
        let mut lease = opener.open(&key).unwrap();
        let reaper_permit = opener.runtime_permit.as_ref().unwrap().clone();
        drop(opener);
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let hang: Box<dyn FnOnce() + Send> = Box::new(move || {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        if flush {
            lease.before_flush = Some(hang);
        } else {
            lease.before_encode = Some(hang);
        }
        let mut worker =
            crate::preview_fanout::PreviewWorker::spawn(0, key.cadence, key.cadence, lease, |_| {
                Ok(())
            })
            .unwrap();
        if !flush {
            let canvas = Arc::new(
                multiview_compositor::pipeline::Nv12Image::solid(
                    160,
                    120,
                    16,
                    128,
                    128,
                    multiview_core::color::ColorInfo::default(),
                )
                .unwrap(),
            );
            worker.offer(0, &canvas).unwrap();
        }
        let start = Instant::now();
        crate::preview_vertical::bounded_reap(reaper_permit, move || {
            worker.join().unwrap();
        });
        assert!(start.elapsed() < Duration::from_secs(1));
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        for _ in 0..100 {
            assert!(budget.acquire().is_none());
        }
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if budget.acquire().is_some() {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
    }

    #[test]
    fn actual_optional_worker_encode_hang_retains_native_permit() {
        assert_native_worker_hang_quarantines_budget(false);
    }

    #[test]
    fn actual_optional_worker_flush_hang_retains_native_permit() {
        assert_native_worker_hang_quarantines_budget(true);
    }

    #[test]
    fn actual_factory_packets_reach_registry_subscriber_and_retire_independently() {
        fn register_independently(
            opener: &mut PreviewRuntimeOpener<RuntimeRegistry>,
            lease: &PreviewEncoderLease,
        ) -> crate::optional_rendition::OptionalRegistration {
            let mut ready = opener.publish(std::slice::from_ref(lease)).unwrap();
            // This direct registry test drives activation itself. Production
            // transfers the publisher once to PreviewDelivery, which activates
            // after first windows; retain the same owner for independent guards.
            assert!(ready.activate());
            let registration = ready.registration.take().unwrap();
            opener.registry.publisher = Some(ready.publisher);
            registration
        }

        let (key, cost, production) = actual_preview_setup();
        let (mut registry, factory) = OutputWindowRegistry::bare(vec![production.clone()], 7);
        let factory = Arc::new(factory);
        let bridge = RuntimeRegistry::new(factory.clone(), "production".into()).unwrap();
        let mut opener = PreviewRuntimeOpener::new(vec![(key.clone(), cost)], bridge);
        let mut first = opener.open(&key).unwrap();
        let first_guard = register_independently(&mut opener, &first);
        let second = opener.open(&key).unwrap();
        let second_guard = register_independently(&mut opener, &second);
        let scope = multiview_control::WhepScope::Output(first.descriptor.output_id.clone());
        let binding = factory.resolve(&scope).unwrap();
        assert_eq!(binding.output_index, 43);
        assert_eq!(binding.video.coded_width, 160);
        assert_eq!(
            binding.video.config,
            first.encoder.codec_params().codec_config_bytes()
        );
        assert_eq!(
            &binding.dependencies[..2],
            production.dependencies.as_slice()
        );
        let sub = factory.subscribe(&scope, 7).unwrap();
        let now = Instant::now();
        registry.pump(now);
        let mut received = Vec::new();
        for tick in 0..90 {
            let mut image =
                ffmpeg_next::util::frame::Video::new(ffmpeg_next::format::Pixel::NV12, 160, 120);
            for plane in 0..image.planes() {
                image.data_mut(plane).fill(128);
            }
            let frame = multiview_ffmpeg::DecodedVideoFrame {
                frame: image,
                meta: multiview_core::frame::FrameMeta {
                    pts: multiview_core::time::MediaTime::ZERO,
                    width: 160,
                    height: 120,
                    format: multiview_core::pixel::PixelFormat::Nv12,
                    color: multiview_core::color::ColorInfo::default(),
                },
                raw_pts: None,
                a53_cc: None,
            };
            let packets = first.encoder.encode_frame_at(frame, tick).unwrap();
            assert!(!packets.is_empty());
            let expected: Vec<_> = packets
                .iter()
                .map(|p| p.payload().unwrap().to_vec())
                .collect();
            first
                .offer_complete(packets, Duration::from_millis(34), now)
                .unwrap();
            registry.pump(now);
            let lease = sub.try_recv_window().unwrap().unwrap();
            assert_eq!(lease.permit.packets().len(), expected.len());
            for (packet, bytes) in lease.permit.packets().iter().zip(expected) {
                assert_eq!(packet.payload().unwrap(), bytes);
                received.push(packet.pts().unwrap());
            }
        }
        assert_eq!(received, (0..90).collect::<Vec<_>>());
        assert!(first.encoder.finish().unwrap().is_empty());
        drop(first_guard);
        assert!(factory.resolve(&scope).is_err());
        assert_eq!(
            factory
                .resolve(&multiview_control::WhepScope::Output("production".into()))
                .unwrap(),
            production
        );
        let second_scope =
            multiview_control::WhepScope::Output(second.descriptor.output_id.clone());
        assert_eq!(factory.resolve(&second_scope).unwrap().output_index, 44);
        let third = opener.open(&key).unwrap();
        let third_guard = register_independently(&mut opener, &third);
        assert_eq!(
            factory
                .resolve(&multiview_control::WhepScope::Output(
                    third.descriptor.output_id.clone()
                ))
                .unwrap()
                .output_index,
            45
        );
        drop((second_guard, third_guard));
    }
}
