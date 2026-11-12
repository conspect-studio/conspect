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
    pub(crate) runtime_gate: Arc<AtomicBool>,
    pub(crate) shared_budget: bool,
    #[cfg(test)]
    pub(crate) startup_delay: std::time::Duration,
}

/// Run-owned optional native lifecycle budget. Keep one handle across startup
/// requests; a hung native lease quarantines its slot until the native call ends.
#[derive(Clone, Default)]
pub struct PreviewAdmissionBudget(Arc<AtomicBool>);
impl PreviewAdmissionBudget {
    /// Create the single optional runtime slot for one application/run owner.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(all(test, any(feature = "moq-output", feature = "gpl-codecs")))]
    pub(crate) fn acquire(&self) -> Option<Arc<RuntimePermit>> {
        RuntimePermit::acquire(self.0.clone())
    }
}

impl PreviewStartup {
    /// Bind this request to the run-owned budget shared by all replacement requests.
    #[must_use]
    pub fn with_admission_budget(mut self, budget: &PreviewAdmissionBudget) -> Self {
        self.runtime_gate = budget.0.clone();
        self.shared_budget = true;
        self
    }
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
        // Public typed construction cannot bypass the same cadence validity
        // required by file parsing. Check every axis before value comparisons.
        if production
            .iter()
            .map(|p| &p.key)
            .chain(requests.iter().map(|r| &r.key))
            .chain(measured.iter().map(|(key, _)| key))
            .any(|key| key.cadence.num <= 0 || key.cadence.den <= 0)
        {
            return None;
        }
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
            runtime_gate: Arc::new(AtomicBool::new(false)),
            shared_budget: false,
            #[cfg(test)]
            startup_delay: std::time::Duration::ZERO,
        })
    }
}

/// Run-owned lifecycle permit retained before native open and by every native
/// encoder. A hung optional worker quarantines all replacements, not production.
pub(crate) struct RuntimePermit(Arc<AtomicBool>);
impl RuntimePermit {
    pub(crate) fn acquire(gate: Arc<AtomicBool>) -> Option<Arc<Self>> {
        gate.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(Arc::new(Self(gate)))
    }
}
impl Drop for RuntimePermit {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// A single budget fences startup, all native leases, and its reaper together.
/// A failed spawn drops the join handles, not the native leases on their threads.
pub(crate) fn bounded_reap(permit: Arc<RuntimePermit>, reap: impl FnOnce() + Send + 'static) {
    bounded_reap_with(permit, reap, |task| {
        std::thread::Builder::new()
            .name("optional-worker-reaper".into())
            .spawn(task)
            .map(|_| ())
    });
}

fn bounded_reap_with(
    permit: Arc<RuntimePermit>,
    reap: impl FnOnce() + Send + 'static,
    spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<()>,
) {
    let (done_tx, done_rx) = sync_channel(1);
    let result = spawn(Box::new(move || {
        let _permit = permit;
        reap();
        let _ = done_tx.try_send(());
    }));
    if let Err(error) = result {
        tracing::warn!(%error, "optional reaper unavailable; native leases quarantined");
        return;
    }
    let _ = done_rx.recv_timeout(std::time::Duration::from_millis(250));
}

enum Input {
    Frame(u64, Arc<multiview_compositor::pipeline::Nv12Image>),
    Audio(DispatchEvent),
}
/// Nonblocking production-facing handle. Encoder startup never runs on the
/// production consumer, including first-frame metadata discovery. Cloneable:
/// the program-audio encode worker holds its own handle (same channel).
#[derive(Clone)]
pub(crate) struct PreviewIngress {
    tx: SyncSender<Input>,
    alive: Arc<AtomicBool>,
}
impl PreviewIngress {
    fn terminal_audio(&self, packets: Vec<multiview_ffmpeg::EncodedPacket>) {
        if self
            .tx
            .try_send(Input::Audio(DispatchEvent::FinishAudio { packets }))
            .is_err()
        {
            self.alive.store(false, Ordering::Release);
        }
    }
    pub(crate) fn spawn(
        request: PreviewStartup,
        factory: Arc<crate::output_window_registry::SubscriptionFactory>,
        cadence: Rational,
        audio_bases: Vec<crate::rendition_dispatch::DtsTimeBase>,
        tracks: Vec<usize>,
    ) -> Option<Self> {
        if !request.shared_budget {
            return None;
        }
        let permit = RuntimePermit::acquire(request.runtime_gate.clone())?;
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
                                    permit.clone(),
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
                            if matches!(event, DispatchEvent::FinishAudio { .. }) {
                                if let Some(v) = vertical.take() {
                                    v.finish();
                                }
                                break;
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
        if let DispatchEvent::FinishAudio { packets } = event {
            let Ok(packets) = packets
                .iter()
                .map(multiview_ffmpeg::EncodedPacket::clone_shared_ref)
                .collect::<Result<Vec<_>, _>>()
            else {
                self.alive.store(false, Ordering::Release);
                return;
            };
            self.terminal_audio(packets);
            return;
        }
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

pub(crate) struct PreviewVertical {
    workers: Vec<crate::preview_fanout::PreviewWorker>,
    tx: SyncSender<DispatchEvent>,
    alive: Arc<AtomicBool>,
    permit: Arc<RuntimePermit>,
}
impl PreviewVertical {
    fn finish(mut self) {
        let workers = std::mem::take(&mut self.workers);
        let alive = self.alive.clone();
        bounded_reap(self.permit.clone(), move || {
            for worker in workers {
                if worker.join().is_err() {
                    alive.store(false, Ordering::Release);
                }
            }
        });
        // Closing the last event sender lets the assembler process its complete
        // video/audio EOF tail before the registration retires.
        self.alive.store(false, Ordering::Release);
    }
    fn terminal_audio(&self, packets: Vec<multiview_ffmpeg::EncodedPacket>) {
        if self
            .tx
            .try_send(DispatchEvent::FinishAudio { packets })
            .is_err()
        {
            self.alive.store(false, Ordering::Release);
        }
    }
    #[allow(clippy::too_many_lines)] // Admission, worker ownership and publication form one transaction.
    pub(crate) fn start(
        request: PreviewStartup,
        factory: Arc<crate::output_window_registry::SubscriptionFactory>,
        production_cadence: Rational,
        first_canvas: &Arc<multiview_compositor::pipeline::Nv12Image>,
        audio_bases: Vec<crate::rendition_dispatch::DtsTimeBase>,
        audio_tracks: &[usize],
        permit: Arc<RuntimePermit>,
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
        opener.runtime_permit = Some(permit.clone());
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
                video_move_gates: Vec::new(),
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
                loop {
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
                            if !thread_alive.load(Ordering::Acquire) {
                                break;
                            }
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
        let mut workers: Vec<crate::preview_fanout::PreviewWorker> = Vec::new();
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
                // Partial startup owns real native leases too. Close their inputs
                // and quarantine any hung flush under the same bounded reaper.
                bounded_reap(permit.clone(), move || {
                    for worker in workers {
                        let _ = worker.join();
                    }
                });
                return None;
            };
            workers.push(worker);
        }
        Some(Self {
            workers,
            tx,
            alive,
            permit,
        })
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
        if let DispatchEvent::FinishAudio { packets } = event {
            let Ok(packets) = packets
                .iter()
                .map(multiview_ffmpeg::EncodedPacket::clone_shared_ref)
                .collect::<Result<Vec<_>, _>>()
            else {
                self.alive.store(false, Ordering::Release);
                return;
            };
            self.terminal_audio(packets);
            return;
        }
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
    fn mark_discontinuity(&mut self, _: usize, _: multiview_output::batch::OfferError) {}
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

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    #[test]
    fn hung_reaper_bounds_shutdown_and_blocks_replacements_until_completion() {
        use std::time::{Duration, Instant};
        let gate = Arc::new(AtomicBool::new(false));
        let permit = RuntimePermit::acquire(gate.clone()).unwrap();
        let (release, wait) = sync_channel(1);
        let (done, completed) = sync_channel(1);
        let start = Instant::now();
        bounded_reap(permit, move || {
            wait.recv().unwrap();
            done.send(()).unwrap();
        });
        assert!(start.elapsed() < Duration::from_secs(1));
        for _ in 0..100 {
            assert!(RuntimePermit::acquire(gate.clone()).is_none());
        }
        release.send(()).unwrap();
        completed.recv_timeout(Duration::from_secs(1)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while gate.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(RuntimePermit::acquire(gate).is_some());
    }

    #[test]
    fn failed_reaper_spawn_does_not_join_or_release_a_native_lease() {
        let gate = Arc::new(AtomicBool::new(false));
        let permit = RuntimePermit::acquire(gate.clone()).unwrap();
        let native = permit.clone();
        bounded_reap_with(
            permit,
            || panic!("must not join"),
            |task| {
                drop(task);
                Err(std::io::Error::other("injected spawn failure"))
            },
        );
        assert!(RuntimePermit::acquire(gate.clone()).is_none());
        drop(native);
        assert!(RuntimePermit::acquire(gate).is_some());
    }

    #[test]
    fn independently_created_requests_share_the_run_quarantine_slot() {
        let budget = PreviewAdmissionBudget::new();
        let first = RuntimePermit::acquire(budget.0.clone()).expect("first native owner");
        let replacement_budget = budget.clone();
        assert!(RuntimePermit::acquire(replacement_budget.0.clone()).is_none());
        drop(first);
        assert!(RuntimePermit::acquire(replacement_budget.0).is_some());
    }

    #[test]
    fn native_lease_quarantines_request_until_last_owner_releases() {
        let gate = Arc::new(AtomicBool::new(false));
        let permit = RuntimePermit::acquire(gate.clone()).expect("first runtime");
        let native = permit.clone();
        drop(permit);
        assert!(RuntimePermit::acquire(gate.clone()).is_none());
        drop(native);
        assert!(RuntimePermit::acquire(gate).is_some());
    }
}
