//! Opt-in, measured optional preview startup and isolated A/V assembly.
use crate::binding_delivery::WindowDelivery;
use crate::{pipeline::PipelineError, rendition_workers::DispatchEvent};
use multiview_config::preview_ladder::{
    PreviewKey, PreviewRequest, ProductionReservation, ResourceCost,
};
use multiview_core::time::Rational;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{sync_channel, SyncSender},
    Arc,
};

/// Explicit platform measurements. No capacity is inferred from a GPU name.
#[derive(Clone)]
pub struct PreviewStartup {
    pub(crate) parent_output: String,
    pub(crate) production: Vec<ProductionReservation>,
    pub(crate) requests: Vec<PreviewRequest>,
    pub(crate) capacity: Option<ResourceCost>,
    pub(crate) measured: Vec<(PreviewKey, ResourceCost)>,
    pub(crate) audio_once_bps: u64,
    pub(crate) overhead_bps: u64,
    pub(crate) opened_parent: Option<crate::output_window_registry::EffectiveBinding>,
    #[cfg(test)]
    pub startup_delay: std::time::Duration,
}

impl PreviewStartup {
    /// Build an explicit measured request. Unknown capacity, missing reservations,
    /// unsupported codecs/cadences and unmatched measurements fail closed.
    #[allow(clippy::too_many_arguments)] // All resource inputs are caller-supplied measurements, never defaults.
    #[must_use]
    pub fn new(
        parent_output: String,
        production: Vec<ProductionReservation>,
        requests: Vec<PreviewRequest>,
        capacity: Option<ResourceCost>,
        measured: Vec<(PreviewKey, ResourceCost)>,
        audio_once_bps: u64,
        overhead_bps: u64,
    ) -> Option<Self> {
        let capacity = capacity?;
        let first = requests.first()?;
        if parent_output.is_empty()
            || production.is_empty()
            || requests.len() > 3
            || !production.iter().any(|p| p.output_id == parent_output)
            || requests.iter().any(|r| {
                r.profile_id.is_empty()
                    || r.key.codec != "h264"
                    || r.key.cadence != first.key.cadence
                    || r.key.width == 0
                    || r.key.height == 0
                    || r.key.cadence.num <= 0
                    || r.key.cadence.den <= 0
                    || r.key.bitrate_bps == 0
                    || !measured
                        .iter()
                        .any(|(k, c)| *k == r.key && c.encoder_sessions > 0)
            })
            || capacity.encoder_sessions == 0
            || capacity.uplink_bps == 0
            || requests.iter().enumerate().any(|(i, r)| {
                requests
                    .iter()
                    .take(i)
                    .any(|p| p.profile_id == r.profile_id)
            })
        {
            return None;
        }
        Some(Self {
            parent_output,
            production,
            requests,
            capacity: Some(capacity),
            measured,
            audio_once_bps,
            overhead_bps,
            opened_parent: None,
            #[cfg(test)]
            startup_delay: std::time::Duration::ZERO,
        })
    }
}

enum Input {
    Frame(u64, Arc<multiview_compositor::pipeline::Nv12Image>),
    Audio(DispatchEvent),
}
/// Nonblocking production-facing handle. Encoder startup never runs on the
/// production consumer, including first-frame metadata discovery.
pub(crate) struct PreviewIngress {
    tx: SyncSender<Input>,
    alive: Arc<AtomicBool>,
}
impl PreviewIngress {
    pub(crate) fn spawn(
        request: PreviewStartup,
        factory: Arc<crate::output_window_registry::SubscriptionFactory>,
        cadence: Rational,
        audio_bases: Vec<crate::rendition_dispatch::DtsTimeBase>,
        tracks: Vec<usize>,
    ) -> Option<Self> {
        let (tx, rx) = sync_channel(16);
        let alive = Arc::new(AtomicBool::new(true));
        let active = alive.clone();
        std::thread::Builder::new()
            .name("optional-startup".into())
            .spawn(move || {
                let mut vertical = None;
                let mut request = Some(request);
                while active.load(Ordering::Acquire) {
                    let input = match rx.recv_timeout(std::time::Duration::from_millis(50)) {
                        Ok(input) => input,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(_) => break,
                    };
                    match input {
                        Input::Frame(tick, canvas) => {
                            if let Some(request) = request.take() {
                                if tick != 0 {
                                    break;
                                }
                                #[cfg(test)]
                                std::thread::sleep(request.startup_delay);
                                if !active.load(Ordering::Acquire) {
                                    break;
                                }
                                vertical = PreviewVertical::start(
                                    request,
                                    factory.clone(),
                                    cadence,
                                    &canvas,
                                    audio_bases.clone(),
                                    &tracks,
                                );
                                if vertical.is_none() {
                                    break;
                                }
                            }
                            if let Some(v) = vertical.as_mut() {
                                v.offer(tick, &canvas);
                                if !v.alive.load(Ordering::Acquire) {
                                    break;
                                }
                            }
                        }
                        Input::Audio(event) => {
                            if let Some(v) = vertical.as_ref() {
                                v.audio(&event);
                            }
                        }
                    }
                }
                active.store(false, Ordering::Release);
            })
            .ok()?;
        Some(Self { tx, alive })
    }
    pub(crate) fn offer(&self, tick: u64, canvas: &Arc<multiview_compositor::pipeline::Nv12Image>) {
        if self.alive.load(Ordering::Acquire)
            && self
                .tx
                .try_send(Input::Frame(tick, canvas.clone()))
                .is_err()
        {
            self.alive.store(false, Ordering::Release);
        }
    }
    pub(crate) fn audio(&self, event: &DispatchEvent) {
        if !self.alive.load(Ordering::Acquire) {
            return;
        }
        let DispatchEvent::Audio {
            tick,
            timebases,
            packets,
            watermarks,
        } = event
        else {
            return;
        };
        let Ok(packets) = packets
            .iter()
            .map(multiview_ffmpeg::EncodedPacket::clone_shared_ref)
            .collect::<Result<Vec<_>, _>>()
        else {
            self.alive.store(false, Ordering::Release);
            return;
        };
        if self
            .tx
            .try_send(Input::Audio(DispatchEvent::Audio {
                tick: *tick,
                timebases: timebases.clone(),
                packets,
                watermarks: watermarks.clone(),
            }))
            .is_err()
        {
            self.alive.store(false, Ordering::Release);
        }
    }
}

impl Drop for PreviewIngress {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
    }
}

pub(crate) struct PreviewVertical {
    workers: Vec<crate::preview_fanout::PreviewWorker>,
    tx: SyncSender<DispatchEvent>,
    alive: Arc<AtomicBool>,
}
impl PreviewVertical {
    #[allow(clippy::too_many_lines)] // Admission, worker ownership and publication form one transaction.
    pub(crate) fn start(
        request: PreviewStartup,
        factory: Arc<crate::output_window_registry::SubscriptionFactory>,
        production_cadence: Rational,
        first_canvas: &Arc<multiview_compositor::pipeline::Nv12Image>,
        audio_bases: Vec<crate::rendition_dispatch::DtsTimeBase>,
        audio_tracks: &[usize],
    ) -> Option<Self> {
        use multiview_config::preview_ladder::{admit_ladder, PreviewLease};
        // Every opened production output needs a reservation before optional work.
        let effective = factory.effective();
        if effective.iter().any(|binding| {
            !request.production.iter().any(|p| {
                p.output_id == binding.output_id
                    && p.key.width == binding.video.coded_width
                    && p.key.height == binding.video.coded_height
                    && p.key.cadence == production_cadence
                    && p.key.codec.eq_ignore_ascii_case(&binding.video.codec_id)
                    && binding.video.bitrate_bps == Some(p.key.bitrate_bps)
            })
        }) {
            tracing::warn!(?effective, "optional production reservation mismatch");
            return None;
        }
        // One shared optional clock per assembly lane. More profiles can alias
        // this key; independent cadence lanes require independent dispatchers.
        if request.requests.iter().any(|r| r.key.codec != "h264") {
            return None;
        }
        let cadence = request.requests.first()?.key.cadence;
        if request.requests.iter().any(|r| r.key.cadence != cadence) {
            return None;
        }
        let mut registry =
            crate::preview_runtime::RuntimeRegistry::new(factory, request.parent_output)?;
        registry.opened_parent = request.opened_parent.map(|mut parent| {
            parent.audio.retain(|audio| {
                audio_tracks.contains(&match audio.kind {
                    multiview_ffmpeg::StreamKind::Audio => 0,
                    multiview_ffmpeg::StreamKind::AudioTrack(i) => usize::from(i) + 1,
                    _ => usize::MAX,
                })
            });
            parent
        });
        let mut opener =
            crate::preview_runtime::PreviewRuntimeOpener::new(request.measured.clone(), registry);
        opener.first_canvas = Some(first_canvas.clone());
        let admitted = admit_ladder(
            &request.production,
            &request.requests,
            request.capacity,
            &request.measured,
            request.audio_once_bps,
            request.overhead_bps,
            &mut opener,
        )
        .ok()?;
        let (registration, leases) = admitted.into_runtime_parts();
        let registration = registration?;
        let mut producers = Vec::new();
        let mut bases = Vec::new();
        for lease in &leases {
            producers.push(lease.producer()?);
            let tb = lease.encoder.time_base();
            bases.push(crate::rendition_dispatch::DtsTimeBase {
                num: tb.num,
                den: tb.den,
            });
        }
        let bindings = (0..leases.len())
            .map(|i| crate::rendition_dispatch::BindingConfig {
                output_index: i,
                video_key: Some(i),
                audio_tracks: audio_tracks.to_vec(),
            })
            .collect();
        let mut dispatcher = crate::rendition_dispatch::RenditionDispatcher::new(
            crate::rendition_dispatch::DispatchConfig {
                video_count: bases.len(),
                audio_count: audio_bases.len(),
                video_time_base: bases,
                audio_time_base: audio_bases,
                bindings,
                first_tick: 0,
                cadence,
            },
        )
        .ok()?;
        let (tx, rx) = sync_channel(256);
        let alive = Arc::new(AtomicBool::new(true));
        let thread_alive = alive.clone();
        std::thread::Builder::new()
            .name("optional-av-dispatch".into())
            .spawn(move || {
                let mut delivery = PreviewDelivery {
                    producers,
                    registration,
                    ready: std::collections::HashSet::new(),
                    pending: Vec::new(),
                };
                while thread_alive.load(Ordering::Acquire) {
                    match rx.recv_timeout(std::time::Duration::from_millis(50)) {
                        Ok(event) => {
                            if dispatcher
                                .dispatch(event, std::time::Instant::now(), &mut delivery)
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            if dispatcher
                                .watchdog(std::time::Instant::now(), &mut delivery)
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                thread_alive.store(false, Ordering::Release);
            })
            .ok()?;
        let mut workers = Vec::new();
        for (index, lease) in leases.into_iter().enumerate() {
            let cadence = lease.descriptor().key.cadence;
            let sender = tx.clone();
            let worker_alive = alive.clone();
            let worker = crate::preview_fanout::PreviewWorker::spawn(
                index,
                production_cadence,
                cadence,
                lease,
                move |event| {
                    sender.try_send(event).map_err(|e| {
                        worker_alive.store(false, Ordering::Release);
                        PipelineError::Output {
                            kind: "optional-preview",
                            reason: e.to_string(),
                        }
                    })
                },
            );
            let Ok(worker) = worker else {
                alive.store(false, Ordering::Release);
                return None;
            };
            workers.push(worker);
        }
        Some(Self { workers, tx, alive })
    }
    pub(crate) fn offer(
        &mut self,
        tick: u64,
        canvas: &Arc<multiview_compositor::pipeline::Nv12Image>,
    ) {
        if !self.alive.load(Ordering::Acquire) {
            return;
        }
        for worker in &mut self.workers {
            if worker.offer(tick, canvas).is_err() {
                self.alive.store(false, Ordering::Release);
                break;
            }
        }
    }
    pub(crate) fn audio(&self, event: &DispatchEvent) {
        if !self.alive.load(Ordering::Acquire) {
            return;
        }
        let DispatchEvent::Audio {
            tick,
            timebases,
            packets,
            watermarks,
        } = event
        else {
            return;
        };
        let Ok(packets) = packets
            .iter()
            .map(multiview_ffmpeg::EncodedPacket::clone_shared_ref)
            .collect::<Result<Vec<_>, _>>()
        else {
            self.alive.store(false, Ordering::Release);
            return;
        };
        if self
            .tx
            .try_send(DispatchEvent::Audio {
                tick: *tick,
                timebases: timebases.clone(),
                packets,
                watermarks: watermarks.clone(),
            })
            .is_err()
        {
            self.alive.store(false, Ordering::Release);
        }
    }
}
impl Drop for PreviewVertical {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
    }
}

struct PreviewDelivery {
    producers: Vec<crate::output_window_registry::OptionalWindowProducer>,
    registration: crate::preview_runtime::ReadyRegistration,
    ready: std::collections::HashSet<usize>,
    pending: Vec<(
        usize,
        Vec<multiview_ffmpeg::EncodedPacket>,
        std::time::Duration,
        std::time::Instant,
    )>,
}
impl WindowDelivery for PreviewDelivery {
    fn offer_window(
        &mut self,
        index: usize,
        _: u64,
        packets: Vec<multiview_ffmpeg::EncodedPacket>,
        span: std::time::Duration,
        now: std::time::Instant,
    ) -> Result<(), multiview_output::batch::EmissionError> {
        if self.ready.len() < self.producers.len() {
            if self.pending.len() >= 16 {
                return Err(multiview_output::batch::EmissionError::Admission(
                    multiview_output::batch::OfferError::WrongProgramShape,
                ));
            }
            self.ready.insert(index);
            self.pending.push((index, packets, span, now));
            if self.ready.len() != self.producers.len() {
                return Ok(());
            }
            if !self.registration.activate() {
                return Err(multiview_output::batch::EmissionError::Admission(
                    multiview_output::batch::OfferError::WrongProgramShape,
                ));
            }
            for (index, packets, span, now) in self.pending.drain(..) {
                self.producers
                    .get(index)
                    .ok_or(multiview_output::batch::EmissionError::Admission(
                        multiview_output::batch::OfferError::WrongProgramShape,
                    ))?
                    .offer_complete(packets, span, now)
                    .map_err(|_| {
                        multiview_output::batch::EmissionError::Admission(
                            multiview_output::batch::OfferError::WrongProgramShape,
                        )
                    })?;
            }
            return Ok(());
        }
        self.producers
            .get(index)
            .ok_or(multiview_output::batch::EmissionError::Admission(
                multiview_output::batch::OfferError::WrongProgramShape,
            ))?
            .offer_complete(packets, span, now)
            .map_err(|_| {
                multiview_output::batch::EmissionError::Admission(
                    multiview_output::batch::OfferError::WrongProgramShape,
                )
            })
    }
    fn report_loss(&mut self, _: usize, _: multiview_output::batch::OfferError) {}
    fn take_discontinuity(&mut self, _: usize) -> Option<multiview_output::batch::Discontinuity> {
        None
    }
    fn generation(&self, _: usize) -> u64 {
        0
    }
    fn statuses(&self) -> Vec<crate::binding_delivery::BindingStatus> {
        Vec::new()
    }
    fn finish(&mut self) {}
    fn abort(&mut self) {}
}
