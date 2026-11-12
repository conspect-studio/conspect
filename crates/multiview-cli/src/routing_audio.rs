//! Routed outputs' audio (ADR-0108 slice 1 step 5, design §5.1-§5.3, §5.5).
//!
//! `audio = follow`: an output's audio follows its video feed. Program keeps
//! the shared program AAC stream (today, unchanged). A take that changes an
//! output's audio opens a **private encoded audio bus** for that output at
//! take admission (before the video move): one CPU AAC encoder on the
//! **global AAC grid** (first frame at the first multiple of 1024 at or after
//! `total_at(T_admit)`), fed the output's *current* audio until the grid
//! boundary `G` of the video move, then an equal-power crossfade of one tick
//! of samples to the new audio, then the new audio only. At `G` the output's
//! program-audio slot moves from the shared stage to the private stage
//! ([`RenditionDispatcher::rebind_audio`]); both encoders coded the same
//! signal up to `G` (Rule 2), so the boundary joins two encodings of one
//! signal.
//!
//! Isolation (invariants #1/#10): nothing here runs on the output clock. The
//! bake consumer only mixes one tick of PCM per private bus and `try_send`s
//! it to that bus's own encode thread (bounded; full = the bus fails alone,
//! counted). Encoded packets come back on a bounded channel (`try_send`;
//! full = the bus fails alone). The dispatcher's dynamic audio stage never
//! gates a base window. The consumer never joins or waits for a bus thread.
//!
//! Slice 1 scope: a private bus, once opened, stays the output's own audio
//! bus for the rest of the run (also after a return to Program, which it
//! carries as a crossfade to the program mix). The decoded-identity move back
//! to the shared program stream (§5.2) is not built.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::Arc;
use std::time::Instant;

use multiview_audio::cadence::SampleClock;
use multiview_audio::crossfade::{grid_ceil, TakeCrossfade, AAC_FRAME_SAMPLES};
use multiview_audio::format::AudioBlock;
use multiview_core::time::Rational;
use multiview_ffmpeg::EncodedPacket;
use multiview_framestore::SwapCell;
use multiview_output::{AudioEncodeConfig, AudioRenditionEncoder};

use crate::binding_delivery::WindowDelivery;
use crate::rendition_dispatch::{DispatchError, RenditionDispatcher};
use crate::rendition_workers::DtsWatermark;
use crate::routing_aux::PictureBus;

/// One private bus's PCM queue depth, in ticks (a job is a few KB).
pub(crate) const PRIVATE_AUDIO_QUEUE: usize = 8;
/// Encoded private-audio events queued toward the consumer (all buses).
const AUDIO_EVENT_QUEUE: usize = 64;
/// Minimum encoded audio before `G` on a private bus (Rule 2: the encoder
/// has coded the old signal for at least two frames before the boundary).
const WARM_FRAMES: u64 = 2;

/// Per-source linear route gain (`gain_db`, `mute`) of `audio-routing`
/// (§5.5), read live on the consumer.
pub(crate) type RouteGain = Arc<dyn Fn(&str) -> f32 + Send + Sync>;

/// The audio a feed carries under `audio = follow` (§5.1).
#[derive(Debug, Clone, PartialEq, Eq)]
enum AudioSource {
    /// The program mix.
    Program,
    /// One source's own clean block (silence when it has no strip).
    Source(String),
    /// The PVW source's own block, resolved each tick.
    Preview,
}

impl AudioSource {
    fn follow(bus: &PictureBus) -> Self {
        match bus {
            PictureBus::Program => Self::Program,
            PictureBus::Preview => Self::Preview,
            PictureBus::Source(id) => Self::Source(id.clone()),
        }
    }
}

/// Encoded packets of one private bus for one fed tick.
struct AudioEvent {
    /// The dispatcher stage the bus was opened on, with its generation.
    key: usize,
    generation: u64,
    packets: Vec<EncodedPacket>,
    watermark: Option<DtsWatermark>,
}

struct Fade {
    xf: TakeCrossfade,
    from: AudioSource,
    to: AudioSource,
    /// The output read this bus before the take's audio move was issued (a
    /// refused video move drops that move, so the binding returns to this).
    was_bound: bool,
}

/// The private encoder's codec configuration as the consumer knows it.
enum ConfigState {
    /// The encoder thread has not reported yet.
    Pending,
    /// The opened encoder's configuration (`None`: it failed to open or has
    /// no configuration record).
    Known(Option<Vec<u8>>),
}

struct PrivateBus {
    /// The dispatcher's dynamic audio stage.
    key: usize,
    generation: u64,
    /// First sample the bus encodes (a grid boundary).
    start: u64,
    jobs: Option<SyncSender<Vec<f32>>>,
    /// The opened encoder's codec configuration (`AudioSpecificConfig`).
    config: Receiver<Option<Vec<u8>>>,
    config_seen: ConfigState,
    failed: Arc<AtomicBool>,
    /// What the bus carries outside a fade.
    current: AudioSource,
    fade: Option<Fade>,
    /// The output's program-audio slot reads this bus (rebind issued).
    bound: bool,
    /// One past the last sample fed.
    fed: u64,
}

/// Counters of the routed-audio side (monotonic).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioRoutingCounters {
    /// Private buses opened.
    pub opened: u64,
    /// Private buses that failed (encoder, queue full, stalled stage).
    pub failed: u64,
    /// Audio moves refused (`encoder-format-mismatch` or encoder not ready);
    /// the video still moved.
    pub refused: u64,
    /// Crossfades back to the old audio after the video move was refused.
    pub faded_back: u64,
}

/// Test observation of routed audio: the PCM handed to the shared program
/// encoder and to each private bus, as `(first sample, interleaved PCM)`.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct AudioTap {
    pub(crate) program: Vec<(u64, Vec<f32>)>,
    pub(crate) private: Vec<(usize, u64, Vec<f32>)>,
    /// Each scheduled crossfade: `(output, G)`.
    pub(crate) moves: Vec<(usize, u64)>,
}

/// Shared [`AudioTap`] (router, handle and test).
#[cfg(test)]
pub(crate) type AudioTapCell = Arc<std::sync::Mutex<AudioTap>>;

/// The routed-audio manager, owned by the router on the bake consumer.
pub(crate) struct RoutedAudio {
    config: AudioEncodeConfig,
    clock: SampleClock,
    program_config: Option<Vec<u8>>,
    route_gain: RouteGain,
    preview: Option<Arc<SwapCell<Option<Arc<str>>>>>,
    channels: usize,
    /// Per output: the audio it carries now (by `follow`).
    carried: Vec<AudioSource>,
    buses: Vec<Option<PrivateBus>>,
    events_tx: SyncSender<AudioEvent>,
    events: Receiver<AudioEvent>,
    dropped: Arc<AtomicU64>,
    counters: AudioRoutingCounters,
    #[cfg(test)]
    pub(crate) tap: Option<AudioTapCell>,
}

impl RoutedAudio {
    pub(crate) fn new(
        outputs: usize,
        config: AudioEncodeConfig,
        cadence: Rational,
        program_config: Option<Vec<u8>>,
        route_gain: RouteGain,
        preview: Option<Arc<SwapCell<Option<Arc<str>>>>>,
    ) -> Self {
        let (events_tx, events) = mpsc::sync_channel(AUDIO_EVENT_QUEUE);
        Self {
            clock: SampleClock::new(config.sample_rate, cadence),
            channels: usize::from(config.channels),
            config,
            program_config,
            route_gain,
            preview,
            carried: vec![AudioSource::Program; outputs],
            buses: (0..outputs).map(|_| None).collect(),
            events_tx,
            events,
            dropped: Arc::new(AtomicU64::new(0)),
            counters: AudioRoutingCounters::default(),
            #[cfg(test)]
            tap: None,
        }
    }

    pub(crate) const fn counters(&self) -> AudioRoutingCounters {
        self.counters
    }

    pub(crate) fn private_buses(&self) -> usize {
        self.buses.iter().filter(|b| b.is_some()).count()
    }

    /// Take admission (before the video move): open the output's private bus
    /// when the take changes its audio and it has none.
    pub(crate) fn admit(
        &mut self,
        output: usize,
        target: &PictureBus,
        tick: u64,
        dispatcher: &mut RenditionDispatcher,
    ) {
        let to = AudioSource::follow(target);
        let carried = self.carried.get(output).cloned();
        let has_bus = self.buses.get(output).is_some_and(Option::is_some);
        if has_bus || carried.as_ref() == Some(&to) || carried.is_none() {
            return;
        }
        let start = grid_ceil(self.clock.total_at(tick), AAC_FRAME_SAMPLES);
        let Ok(start_i64) = i64::try_from(start) else {
            return;
        };
        let Ok((key, generation)) = dispatcher.add_audio_key() else {
            self.counters.refused = self.counters.refused.saturating_add(1);
            return;
        };
        let failed = Arc::new(AtomicBool::new(false));
        let (config_tx, config_rx) = mpsc::sync_channel(1);
        let spawned = spawn_bus_worker(
            self.config.clone(),
            start_i64,
            (key, generation),
            self.events_tx.clone(),
            config_tx,
            Arc::clone(&failed),
            Arc::clone(&self.dropped),
        );
        let Ok(jobs) = spawned else {
            let _ = dispatcher.retire_audio_key(key);
            self.counters.failed = self.counters.failed.saturating_add(1);
            return;
        };
        self.counters.opened = self.counters.opened.saturating_add(1);
        if let (Some(slot), Some(current)) = (self.buses.get_mut(output), carried) {
            *slot = Some(PrivateBus {
                key,
                generation,
                start,
                jobs: Some(jobs),
                config: config_rx,
                config_seen: ConfigState::Pending,
                failed,
                current,
                fade: None,
                bound: false,
                fed: start,
            });
        }
    }

    /// The router issued the output's video rebind at move tick `move_tick`:
    /// schedule the crossfade at `G` and move the output's audio slot there.
    ///
    /// `after_video = Some(k)`: the router just issued a video move to key
    /// `k`; the audio slot moves only once it is applied (the dispatcher
    /// drops it if the video move is refused or superseded: the audio never
    /// switches while the video stays). `None` for a video change that
    /// already happened (a fallback landing).
    pub(crate) fn follow(
        &mut self,
        output: usize,
        target: &PictureBus,
        move_tick: u64,
        after_video: Option<usize>,
        dispatcher: &mut RenditionDispatcher,
    ) {
        let to = AudioSource::follow(target);
        let program_config = self.program_config.clone();
        let len = multiview_audio::crossfade::take_fade_len(&self.clock, move_tick);
        let at = grid_ceil(self.clock.total_at(move_tick), AAC_FRAME_SAMPLES);
        let Some(Some(bus)) = self.buses.get_mut(output) else {
            // No private bus: the output's audio does not change (or could
            // not be opened; then it stays on the program mix).
            return;
        };
        if bus.failed.load(Ordering::Acquire) {
            return;
        }
        let target_now = bus.fade.as_ref().map_or(&bus.current, |f| &f.to);
        if *target_now == to {
            return;
        }
        // Rule 2: the bus coded the old signal for WARM_FRAMES before G and
        // G lies after everything already fed.
        let warm = bus
            .start
            .saturating_add(WARM_FRAMES.saturating_mul(AAC_FRAME_SAMPLES));
        let g = grid_ceil(at.max(warm).max(bus.fed), AAC_FRAME_SAMPLES);
        let was_bound = bus.bound;
        if !bus.bound {
            if matches!(bus.config_seen, ConfigState::Pending) {
                match bus.config.try_recv() {
                    Ok(cfg) => bus.config_seen = ConfigState::Known(cfg),
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => {}
                }
            }
            // AudioSpecificConfig identity (§5.2): byte for byte.
            let same = matches!(
                &bus.config_seen,
                ConfigState::Known(Some(c)) if program_config.as_ref() == Some(c)
            );
            let Ok(g_i64) = i64::try_from(g) else {
                return;
            };
            if !same
                || dispatcher
                    .rebind_audio(output, Some(bus.key), g_i64, after_video)
                    .is_err()
            {
                // encoder-format-mismatch / not ready: the video still moves.
                tracing::info!(
                    output,
                    "routing: audio move refused (encoder-format-mismatch or encoder not ready); the video moves"
                );
                self.counters.refused = self.counters.refused.saturating_add(1);
                self.close(output, dispatcher);
                return;
            }
            bus.bound = true;
        }
        let from = match bus.fade.take() {
            Some(f) => f.to,
            None => bus.current.clone(),
        };
        bus.current = from.clone();
        #[cfg(test)]
        if let Some(tap) = self.tap.as_ref() {
            if let Ok(mut t) = tap.lock() {
                t.moves.push((output, g));
            }
        }
        bus.fade = Some(Fade {
            xf: TakeCrossfade { start: g, len },
            from,
            to: to.clone(),
            was_bound,
        });
        if let Some(c) = self.carried.get_mut(output) {
            *c = to;
        }
    }

    /// The video move of a take whose audio was already scheduled was
    /// refused (or superseded): fade the private bus back to the old audio
    /// at the next grid boundary (a second crossfade of one tick).
    pub(crate) fn video_refused(&mut self, output: usize, tick: u64) {
        let len = multiview_audio::crossfade::take_fade_len(&self.clock, tick);
        let Some(Some(bus)) = self.buses.get_mut(output) else {
            return;
        };
        let Some(fade) = bus.fade.take() else {
            return;
        };
        // The dispatcher dropped the audio move with the video move: the
        // output reads what it read before the take (a later take binds the
        // bus again; the bus fades back first, so Rule 2 holds then).
        bus.bound = fade.was_bound;
        let start = grid_ceil(fade.xf.end().max(bus.fed), AAC_FRAME_SAMPLES);
        if fade.xf.start >= bus.fed {
            // Nothing of the fade was fed yet: the bus never left the old audio.
            bus.current = fade.from.clone();
        } else {
            bus.current = fade.to.clone();
            bus.fade = Some(Fade {
                xf: TakeCrossfade { start, len },
                from: fade.to,
                to: fade.from.clone(),
                was_bound: fade.was_bound,
            });
            self.counters.faded_back = self.counters.faded_back.saturating_add(1);
            tracing::info!(output, "routing: video move refused; audio fades back");
        }
        if let Some(c) = self.carried.get_mut(output) {
            *c = fade.from;
        }
    }

    /// Feed one tick of PCM to every private bus. `program` is the
    /// post-loudnorm program block; `inputs` the per-source clean blocks of
    /// the same mix. Never blocks: a full queue fails that bus alone.
    pub(crate) fn feed(
        &mut self,
        tick: u64,
        program: &AudioBlock,
        inputs: &[(&str, &AudioBlock)],
        dispatcher: &mut RenditionDispatcher,
    ) {
        let frames = program.frame_count();
        if frames == 0 || self.buses.iter().all(Option::is_none) {
            return;
        }
        let end = self.clock.total_at(tick.saturating_add(1));
        let block_start = end.saturating_sub(u64::try_from(frames).unwrap_or(u64::MAX));
        let preview: Option<Arc<str>> = self.preview.as_ref().and_then(|p| p.load());
        let mut failed = Vec::new();
        for (output, slot) in self.buses.iter_mut().enumerate() {
            let Some(bus) = slot.as_mut() else {
                continue;
            };
            if bus.failed.load(Ordering::Acquire) {
                failed.push(output);
                continue;
            }
            if end <= bus.start {
                continue;
            }
            let from = bus.start.max(block_start).max(bus.fed);
            let skip = usize::try_from(from.saturating_sub(block_start)).unwrap_or(usize::MAX);
            let ch = self.channels;
            let pcm_of = |src: &AudioSource| -> Vec<f32> {
                source_pcm(
                    src,
                    program,
                    inputs,
                    preview.as_deref(),
                    &*self.route_gain,
                    frames,
                    ch,
                )
            };
            let mut out = Vec::new();
            match bus.fade.as_ref() {
                Some(fade) => {
                    let old = pcm_of(&fade.from);
                    let new = pcm_of(&fade.to);
                    fade.xf.mix_into(&old, &new, ch, block_start, &mut out);
                }
                None => out = pcm_of(&bus.current),
            }
            let tail = out.get(skip.saturating_mul(ch)..).map(<[f32]>::to_vec);
            let Some(tail) = tail else {
                continue;
            };
            if tail.is_empty() {
                continue;
            }
            #[cfg(test)]
            if let Some(tap) = self.tap.as_ref() {
                if let Ok(mut t) = tap.lock() {
                    t.private.push((output, from, tail.clone()));
                }
            }
            bus.fed = end;
            if let Some(fade) = bus.fade.as_ref() {
                if end >= fade.xf.end() {
                    bus.current = fade.to.clone();
                    bus.fade = None;
                }
            }
            let sent = bus
                .jobs
                .as_ref()
                .is_some_and(|jobs| match jobs.try_send(tail) {
                    Ok(()) => true,
                    Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
                });
            if !sent {
                failed.push(output);
            }
        }
        for output in failed {
            self.fail(output, dispatcher);
        }
    }

    /// Record the block handed to the shared program encoder for `tick`.
    #[cfg(test)]
    pub(crate) fn tap_program(&self, tick: u64, program: &AudioBlock) {
        if let Some(tap) = self.tap.as_ref() {
            let end = self.clock.total_at(tick.saturating_add(1));
            let frames = u64::try_from(program.frame_count()).unwrap_or(0);
            if let Ok(mut t) = tap.lock() {
                t.program
                    .push((end.saturating_sub(frames), program.interleaved().to_vec()));
            }
        }
    }

    /// Dispatch the private buses' encoded packets (before the base drain).
    ///
    /// # Errors
    /// A genuine (non-admission) dispatcher fault.
    pub(crate) fn drain<D: WindowDelivery>(
        &mut self,
        dispatcher: &mut RenditionDispatcher,
        delivery: &mut D,
    ) -> Result<(), DispatchError> {
        while let Ok(event) = self.events.try_recv() {
            // The event names its stage by (key, generation): the
            // dispatcher stages it only while that slot still carries this
            // generation (a reused slot drops it, counted).
            let Some(key) = self
                .buses
                .iter()
                .flatten()
                .find(|b| b.generation == event.generation && b.key == event.key)
                .map(|b| b.key)
            else {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            if let Err(e) = dispatcher.dispatch_dynamic_audio(
                key,
                event.generation,
                event.packets,
                event.watermark,
                false,
                Instant::now(),
                delivery,
            ) {
                if !matches!(e, DispatchError::Admission { .. }) {
                    return Err(e);
                }
            }
        }
        // A stage the dispatcher failed (stall, watchdog) fails its bus.
        let gone: Vec<usize> = self
            .buses
            .iter()
            .enumerate()
            .filter_map(|(i, b)| {
                b.as_ref()
                    .filter(|b| dispatcher.is_audio_key_failed(b.key))
                    .map(|_| i)
            })
            .collect();
        for output in gone {
            self.fail(output, dispatcher);
        }
        Ok(())
    }

    fn fail(&mut self, output: usize, dispatcher: &mut RenditionDispatcher) {
        self.counters.failed = self.counters.failed.saturating_add(1);
        tracing::warn!(
            output,
            "routing: the output's audio bus failed; it returns to the program mix"
        );
        self.close(output, dispatcher);
        if let Some(c) = self.carried.get_mut(output) {
            *c = AudioSource::Program;
        }
    }

    /// Retire the bus's stage and close its input (the thread finishes and
    /// ends on its own; never joined here).
    fn close(&mut self, output: usize, dispatcher: &mut RenditionDispatcher) {
        if let Some(bus) = self.buses.get_mut(output).and_then(Option::take) {
            let _ = dispatcher.retire_audio_key(bus.key);
            drop(bus.jobs);
        }
    }

    /// End of run: close every bus (never joins).
    pub(crate) fn shutdown(&mut self, dispatcher: &mut RenditionDispatcher) {
        for output in 0..self.buses.len() {
            self.close(output, dispatcher);
        }
        while self.events.try_recv().is_ok() {}
    }
}

/// One tick of `src` as interleaved PCM of exactly `frames` frames.
fn source_pcm(
    src: &AudioSource,
    program: &AudioBlock,
    inputs: &[(&str, &AudioBlock)],
    preview: Option<&str>,
    gain: &(dyn Fn(&str) -> f32 + Send + Sync),
    frames: usize,
    channels: usize,
) -> Vec<f32> {
    let len = frames.saturating_mul(channels);
    let id = match src {
        AudioSource::Program => return program.interleaved().to_vec(),
        AudioSource::Source(id) => Some(id.as_str()),
        AudioSource::Preview => preview,
    };
    let mut out = vec![0.0; len];
    let block = id.and_then(|id| {
        inputs
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, b)| (gain(id), *b))
    });
    if let Some((g, block)) = block {
        // input_tracks blocks are pre-gain (the mixer applies the program
        // gain): the route gain/mute is applied here, exactly once. A block
        // shorter than the tick (a shed catch-up) is aligned to its end.
        let src = block.interleaved();
        let offset = len.saturating_sub(src.len());
        for (dst, s) in out.iter_mut().skip(offset).zip(src) {
            *dst = s * g;
        }
    }
    out
}

/// The private bus's encode thread: open the AAC encoder on the global grid,
/// publish its `AudioSpecificConfig`, encode each tick's PCM, drop the priming
/// packet (never delivered, §5.2), and send packets + a proven watermark.
fn spawn_bus_worker(
    config: AudioEncodeConfig,
    start: i64,
    stage: (usize, u64),
    events: SyncSender<AudioEvent>,
    config_tx: SyncSender<Option<Vec<u8>>>,
    failed: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
) -> std::io::Result<SyncSender<Vec<f32>>> {
    let (jobs_tx, jobs) = mpsc::sync_channel::<Vec<f32>>(PRIVATE_AUDIO_QUEUE);
    std::thread::Builder::new()
        .name("routing-audio".to_owned())
        .spawn(move || {
            let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                bus_worker(&config, start, stage, &events, &config_tx, &jobs, &dropped)
            }));
            if !matches!(run, Ok(true)) {
                failed.store(true, Ordering::Release);
            }
        })?;
    Ok(jobs_tx)
}

/// Returns `false` on an encoder failure or a lost event.
fn bus_worker(
    config: &AudioEncodeConfig,
    start: i64,
    (key, generation): (usize, u64),
    events: &SyncSender<AudioEvent>,
    config_tx: &SyncSender<Option<Vec<u8>>>,
    jobs: &Receiver<Vec<f32>>,
    dropped: &AtomicU64,
) -> bool {
    let Ok(mut encoder) = AudioRenditionEncoder::starting_at_sample(config, start) else {
        let _ = config_tx.try_send(None);
        return false;
    };
    let _ = config_tx.try_send(encoder.codec_params().codec_config_bytes());
    let channels = usize::from(config.channels).max(1);
    let frame = encoder.frame_samples().unwrap_or(1024);
    while let Ok(pcm) = jobs.recv() {
        let Ok(mut packets) = encoder.encode_interleaved(&pcm, pcm.len() / channels) else {
            return false;
        };
        packets.retain(|p| p.dts().is_some_and(|d| d >= start));
        let watermark = packets.last().and_then(EncodedPacket::pts).and_then(|pts| {
            crate::rendition_workers::next_frame_lower_bound_ns(pts, frame, config.sample_rate)
        });
        if packets.is_empty() && watermark.is_none() {
            continue;
        }
        if events
            .try_send(AudioEvent {
                key,
                generation,
                packets,
                watermark,
            })
            .is_err()
        {
            dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    use multiview_audio::format::AudioFormat;
    use multiview_audio::ChannelLayout;

    fn block(v: f32, frames: usize) -> AudioBlock {
        AudioBlock::from_interleaved(
            AudioFormat::new(48_000, ChannelLayout::Stereo),
            vec![v; frames * 2],
        )
        .unwrap()
    }

    #[test]
    fn source_pcm_applies_the_route_gain_once_and_silences_a_missing_strip() {
        let program = block(0.5, 1920);
        let a = block(0.25, 1920);
        let inputs = [("a", &a)];
        let gain: &(dyn Fn(&str) -> f32 + Send + Sync) = &|id| if id == "a" { 0.5 } else { 1.0 };
        let pcm =
            |s: AudioSource, pv: Option<&str>| source_pcm(&s, &program, &inputs, pv, gain, 1920, 2);
        assert!(pcm(AudioSource::Program, None)
            .iter()
            .all(|v| (*v - 0.5).abs() < 1e-7));
        assert!(pcm(AudioSource::Source("a".into()), None)
            .iter()
            .all(|v| (*v - 0.125).abs() < 1e-7));
        assert!(pcm(AudioSource::Source("solid".into()), None)
            .iter()
            .all(|v| v.abs() < 1e-9));
        assert!(pcm(AudioSource::Preview, Some("a"))
            .iter()
            .all(|v| (*v - 0.125).abs() < 1e-7));
        assert!(pcm(AudioSource::Preview, None)
            .iter()
            .all(|v| v.abs() < 1e-9));
        assert_eq!(pcm(AudioSource::Program, None).len(), 3840);
    }
}
