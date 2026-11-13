//! Routed outputs' audio (ADR-0108 slices 1-2, design §5.1-§5.3, §5.5).
//!
//! Every output has an audio **choice** ([`AudioChoice`]): `Follow` (the
//! default: the audio of its video feed), `Program`, `Silence`, one source's
//! audio, or one discrete audio-routing track. The choice resolved against
//! the output's video bus is the audio the output carries. Program keeps the
//! shared program AAC stream (today, unchanged). A change of an output's
//! audio opens a **private encoded audio bus** for that output: one CPU AAC
//! encoder on the **global AAC grid** (first frame at the first multiple of
//! 1024 at or after `total_at(T_admit)`), fed the output's *current* audio
//! until the grid boundary `G` of the move, then an equal-power crossfade of
//! one tick of samples to the new audio, then the new audio only. At `G` the
//! output's program-audio slot moves from the shared stage to the private
//! stage ([`RenditionDispatcher::rebind_audio`]); both encoders coded the same
//! signal up to `G` (Rule 2), so the boundary joins two encodings of one
//! signal.
//!
//! **Return to the shared stream** (§5.2). Once a private bus carries the
//! program mix again, no fade is pending or running, and it has been fed at
//! least [`RETURN_AFTER_SAMPLES`] (4 AAC frames) since its last fade ended,
//! the output's slot moves back to the shared program stage at the next grid
//! boundary `G'` and the bus closes when the dispatcher reports that move.
//! Both encoders coded the identical post-loudnorm program PCM for at least
//! four frames before `G'`.
//!
//! **One fade at a time.** A change requested while a fade runs is queued: it
//! starts from the running fade's target at the first grid boundary at or
//! after the end of the tick block in which that fade ends (never two
//! overlapping fades, never a step). A change not
//! yet started is replaced (latest wins). Per output this is O(1) state.
//!
//! Isolation (invariants #1/#10): nothing here runs on the output clock. The
//! bake consumer only mixes one tick of PCM per private bus and `try_send`s
//! it to that bus's own encode thread (bounded; full = the bus fails alone,
//! counted). Encoded packets come back on a bounded channel (`try_send`;
//! full = the bus fails alone). The dispatcher's dynamic audio stage never
//! gates a base window. The consumer never joins or waits for a bus thread.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::Arc;
use std::time::Instant;

use multiview_audio::cadence::SampleClock;
use multiview_audio::crossfade::{grid_ceil, take_fade_len, TakeCrossfade, AAC_FRAME_SAMPLES};
use multiview_audio::format::AudioBlock;
use multiview_core::time::Rational;
use multiview_ffmpeg::EncodedPacket;
use multiview_framestore::SwapCell;
use multiview_output::{AudioEncodeConfig, AudioRenditionEncoder};

use crate::binding_delivery::WindowDelivery;
use crate::rendition_dispatch::{DispatchError, MoveOutcome, RenditionDispatcher};
use crate::rendition_workers::DtsWatermark;
use crate::routing_aux::PictureBus;

/// One private bus's PCM queue depth, in ticks (a job is a few KB).
pub(crate) const PRIVATE_AUDIO_QUEUE: usize = 8;
/// Encoded private-audio events queued toward the consumer (all buses).
const AUDIO_EVENT_QUEUE: usize = 64;
/// Minimum encoded audio before `G` on a private bus (Rule 2: the encoder
/// has coded the old signal for at least two frames before the boundary).
const WARM_FRAMES: u64 = 2;
/// Samples a private bus carrying the program mix must have been fed since
/// its last fade before the output returns to the shared program stream
/// (4 AAC frames, 85 ms at 48 kHz; design §5.2).
pub(crate) const RETURN_AFTER_SAMPLES: u64 = 4 * AAC_FRAME_SAMPLES;

/// Per-source linear route gain (`gain_db`, `mute`) of `audio-routing`
/// (§5.5), read live on the consumer.
pub(crate) type RouteGain = Arc<dyn Fn(&str) -> f32 + Send + Sync>;

/// Test seam: called by a private bus's encode thread before each job with
/// the job's index (block or panic to inject a stalled or crashing worker).
#[cfg(test)]
pub(crate) type WorkerHook = Arc<dyn Fn(u64) + Send + Sync>;

/// An output's explicit audio choice (ADR-0108 §5.1, §6.1).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AudioChoice {
    /// The audio of the output's video feed (the default).
    #[default]
    Follow,
    /// The program mix.
    Program,
    /// Silence.
    Silence,
    /// One source's own audio, with its `audio-routing` gain and mute.
    Source(String),
    /// One discrete `audio-routing` track: `input` is that route's
    /// `input_id` (resolved by the control plane at take time); its clean
    /// block is carried without the route gain, as discrete tracks are.
    Track {
        /// The track name (`target_track`).
        name: String,
        /// The route's input id.
        input: String,
    },
}

impl AudioChoice {
    /// The choice's label: `follow`, `program`, `none`, `src:<id>`,
    /// `track:<name>`.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Follow => "follow".to_owned(),
            Self::Program => "program".to_owned(),
            Self::Silence => "none".to_owned(),
            Self::Source(id) => format!("src:{id}"),
            Self::Track { name, .. } => format!("track:{name}"),
        }
    }

    fn resolve(&self, bus: &PictureBus) -> AudioSource {
        match self {
            Self::Follow => AudioSource::follow(bus),
            Self::Program => AudioSource::Program,
            Self::Silence => AudioSource::Silence,
            Self::Source(id) => AudioSource::Source(id.clone()),
            Self::Track { name, input } => AudioSource::Track {
                name: name.clone(),
                input: input.clone(),
            },
        }
    }
}

/// The audio an output carries (§5.1).
#[derive(Debug, Clone, PartialEq, Eq)]
enum AudioSource {
    /// The program mix.
    Program,
    /// One source's own clean block with its route gain (silence when it has
    /// no strip).
    Source(String),
    /// The PVW source's own block, resolved each tick.
    Preview,
    /// Silence.
    Silence,
    /// A discrete track's input block, without the route gain.
    Track { name: String, input: String },
}

impl AudioSource {
    fn follow(bus: &PictureBus) -> Self {
        match bus {
            // Clean is the Program picture: it follows the program mix. A
            // routed layout carries the program mix too (no stored layout has
            // an `audio` attribute yet; ADR-0108 §5.1).
            PictureBus::Program | PictureBus::Clean | PictureBus::Layout(_) => Self::Program,
            PictureBus::Preview => Self::Preview,
            PictureBus::Source(id) => Self::Source(id.clone()),
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Program => "program".to_owned(),
            Self::Source(id) => format!("src:{id}"),
            Self::Preview => "preview".to_owned(),
            Self::Silence => "none".to_owned(),
            Self::Track { name, .. } => format!("track:{name}"),
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

/// How the output's program-audio slot relates to its private bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bind {
    /// The slot reads the shared program stage; the bus is not on air.
    Unbound,
    /// A move to the bus at the start of its next (unstarted) fade waits
    /// for the encoder's `AudioSpecificConfig` (`after`: the video move it
    /// follows, as [`RenditionDispatcher::rebind_audio`] takes it).
    Deferred { after: Option<usize> },
    /// A move to the bus was issued following a video move.
    Gated,
    /// An unconditional move to the bus was issued, not yet applied.
    Requested,
    /// The slot reads the bus.
    Bound,
    /// A move back to the shared program stage at `G'` was issued.
    Returning(u64),
}

impl Bind {
    /// The output reads (or a move toward) this bus.
    const fn on_air(self) -> bool {
        matches!(self, Self::Gated | Self::Requested | Self::Bound)
    }
}

struct Fade {
    xf: TakeCrossfade,
    from: AudioSource,
    to: AudioSource,
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
    /// A change requested while `fade` runs: it fades from `fade.to` at the
    /// first grid boundary after `fade` ends (latest wins).
    queued: Option<AudioSource>,
    bind: Bind,
    /// A video take admitted this bus and has not issued its move yet.
    reserved: bool,
    /// One past the last sample fed.
    fed: u64,
    /// One past the last faded sample (the bus start before any fade).
    settled_from: u64,
}

impl PrivateBus {
    /// The audio the bus carries once every scheduled change has landed.
    fn target(&self) -> &AudioSource {
        self.queued
            .as_ref()
            .or_else(|| self.fade.as_ref().map(|f| &f.to))
            .unwrap_or(&self.current)
    }
}

/// An audio-only take in flight on one output.
struct Watch {
    /// The grid boundary of its crossfade, once scheduled.
    g: Option<u64>,
    failed: bool,
    /// The output's choice before the take (restored if it does not apply).
    restore: AudioChoice,
}

/// How an audio-only take started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioTakeStart {
    /// The output already carries that audio: applied at once.
    Unchanged,
    /// A crossfade is scheduled; poll [`RoutedAudio::take_progress`].
    Started,
    /// The private bus could not be opened (`encoder-not-ready`).
    Refused,
}

/// Progress of an audio-only take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioTakeProgress {
    /// Not yet on air.
    Waiting,
    /// On air from grid boundary `G` (the sample).
    Done(u64),
    /// The bus failed or its move was refused.
    Failed,
}

/// How a scheduled change of an output's audio is bound.
#[derive(Clone, Copy)]
enum Mode {
    /// The output's slot moves onto the bus (if it is not on it) at the
    /// fade's `G`, following the video move to the key (`None`:
    /// unconditional), once the encoder's configuration is known.
    Bind(Option<usize>),
    /// Only the bus's signal changes (a fade back after a refused move).
    Quiet,
}

enum Change {
    Unchanged,
    Scheduled(Option<u64>),
    Refused,
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
    /// Private buses closed after their output returned to the shared
    /// program stream (§5.2).
    pub returned: u64,
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
    /// Each crossfade as `(output, start, length)`.
    pub(crate) fades: Vec<(usize, u64, u64)>,
    /// Each issued return to the shared stream: `(output, G')`.
    pub(crate) returns: Vec<(usize, u64)>,
}

/// Shared [`AudioTap`] (router, handle and test).
#[cfg(test)]
pub(crate) type AudioTapCell = Arc<std::sync::Mutex<AudioTap>>;

struct OutputAudio {
    choice: AudioChoice,
    /// The choice before the pending take's audio was installed (restored
    /// when that take is refused or superseded).
    restore: Option<AudioChoice>,
    /// The pending video take scheduled an audio change: the audio carried
    /// before it (faded back to on refusal).
    pending_change: Option<AudioSource>,
    bus: Option<PrivateBus>,
    watch: Option<Watch>,
}

/// The routed-audio manager, owned by the router on the bake consumer.
pub(crate) struct RoutedAudio {
    config: AudioEncodeConfig,
    clock: SampleClock,
    cadence: Rational,
    program_config: Option<Vec<u8>>,
    route_gain: RouteGain,
    preview: Option<Arc<SwapCell<Option<Arc<str>>>>>,
    channels: usize,
    outputs: Vec<OutputAudio>,
    events_tx: SyncSender<AudioEvent>,
    events: Receiver<AudioEvent>,
    dropped: Arc<AtomicU64>,
    counters: AudioRoutingCounters,
    /// Reused per-tick buffers (old, new, mixed) of `feed` (#329 F5).
    scratch: (Vec<f32>, Vec<f32>, Vec<f32>),
    #[cfg(test)]
    pub(crate) tap: Option<AudioTapCell>,
    #[cfg(test)]
    pub(crate) worker_hook: Option<WorkerHook>,
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
            cadence,
            channels: usize::from(config.channels),
            config,
            program_config,
            route_gain,
            preview,
            outputs: (0..outputs)
                .map(|_| OutputAudio {
                    choice: AudioChoice::Follow,
                    restore: None,
                    pending_change: None,
                    bus: None,
                    watch: None,
                })
                .collect(),
            events_tx,
            events,
            dropped: Arc::new(AtomicU64::new(0)),
            counters: AudioRoutingCounters::default(),
            scratch: (Vec::new(), Vec::new(), Vec::new()),
            #[cfg(test)]
            tap: None,
            #[cfg(test)]
            worker_hook: None,
        }
    }

    pub(crate) const fn counters(&self) -> AudioRoutingCounters {
        self.counters
    }

    pub(crate) fn private_buses(&self) -> usize {
        self.outputs.iter().filter(|o| o.bus.is_some()).count()
    }

    /// Whether `output` runs a private audio bus now.
    pub(crate) fn has_private_bus(&self, output: usize) -> bool {
        self.outputs.get(output).is_some_and(|o| o.bus.is_some())
    }

    /// The output's audio choice.
    pub(crate) fn choice(&self, output: usize) -> AudioChoice {
        self.outputs
            .get(output)
            .map_or(AudioChoice::Follow, |o| o.choice.clone())
    }

    /// The label of the audio the output carries once every scheduled change
    /// has landed: `program`, `none`, `src:<id>`, `track:<name>`, `preview`.
    pub(crate) fn carried_label(&self, output: usize) -> String {
        self.carried(output).label()
    }

    fn carried(&self, output: usize) -> AudioSource {
        self.outputs
            .get(output)
            .and_then(|o| o.bus.as_ref())
            .map_or(AudioSource::Program, |b| b.target().clone())
    }

    /// The tick whose sample block contains sample `g`.
    pub(crate) fn tick_of_sample(&self, g: u64) -> u64 {
        let num = u128::from(self.cadence.num.unsigned_abs().max(1));
        let den = u128::from(self.cadence.den.unsigned_abs().max(1));
        let rate = u128::from(self.config.sample_rate.max(1));
        let est = u128::from(g).saturating_mul(num) / rate.saturating_mul(den);
        let mut t = u64::try_from(est).unwrap_or(u64::MAX);
        while t > 0 && self.clock.total_at(t) > g {
            t -= 1;
        }
        while t < u64::MAX && self.clock.total_at(t.saturating_add(1)) <= g {
            t += 1;
        }
        t
    }

    /// Video take admission (before the video move): open the output's
    /// private bus when the take changes its audio and it has none, so it is
    /// warm at `G`. `choice` is the take's explicit audio (`None`: the
    /// output's current choice).
    pub(crate) fn admit(
        &mut self,
        output: usize,
        target: &PictureBus,
        choice: Option<&AudioChoice>,
        tick: u64,
        dispatcher: &mut RenditionDispatcher,
    ) {
        let Some(o) = self.outputs.get(output) else {
            return;
        };
        let to = choice.unwrap_or(&o.choice).resolve(target);
        let carried = self.carried(output);
        let total = self.clock.total_at(tick);
        if let Some(bus) = self.outputs.get_mut(output).and_then(|o| o.bus.as_mut()) {
            bus.reserved = true;
            if let Bind::Returning(_) = bus.bind {
                // A return to the shared stream not yet applied: keep the
                // slot on the bus (it still carries the program mix, so the
                // take's fade joins one signal).
                let g = grid_ceil(total.max(bus.fed), AAC_FRAME_SAMPLES);
                if let Ok(g) = i64::try_from(g) {
                    if dispatcher
                        .rebind_audio(output, Some(bus.key), g, None)
                        .is_ok()
                    {
                        bus.bind = Bind::Requested;
                    }
                }
            }
            return;
        }
        if carried == to {
            return;
        }
        if self.open(output, tick, dispatcher) {
            if let Some(bus) = self.outputs.get_mut(output).and_then(|o| o.bus.as_mut()) {
                bus.reserved = true;
            }
        }
    }

    /// Open the output's private bus at the grid boundary at or after
    /// `total_at(tick)`, carrying the program mix (what the output carries
    /// without one). Returns whether it opened.
    fn open(&mut self, output: usize, tick: u64, dispatcher: &mut RenditionDispatcher) -> bool {
        if self.outputs.get(output).is_none_or(|o| o.bus.is_some()) {
            return false;
        }
        let start = grid_ceil(self.clock.total_at(tick), AAC_FRAME_SAMPLES);
        let Ok(start_i64) = i64::try_from(start) else {
            return false;
        };
        let Ok((key, generation)) = dispatcher.add_audio_key() else {
            self.counters.refused = self.counters.refused.saturating_add(1);
            return false;
        };
        let failed = Arc::new(AtomicBool::new(false));
        let (config_tx, config_rx) = mpsc::sync_channel(1);
        let spawned = spawn_bus_worker(
            BusWorker {
                config: self.config.clone(),
                start: start_i64,
                stage: (key, generation),
                events: self.events_tx.clone(),
                config_tx,
                dropped: Arc::clone(&self.dropped),
                #[cfg(test)]
                hook: self.worker_hook.clone(),
            },
            Arc::clone(&failed),
        );
        let Ok(jobs) = spawned else {
            let _ = dispatcher.retire_audio_key(key);
            self.counters.failed = self.counters.failed.saturating_add(1);
            return false;
        };
        self.counters.opened = self.counters.opened.saturating_add(1);
        let Some(o) = self.outputs.get_mut(output) else {
            return false;
        };
        o.bus = Some(PrivateBus {
            key,
            generation,
            start,
            jobs: Some(jobs),
            config: config_rx,
            config_seen: ConfigState::Pending,
            failed,
            current: AudioSource::Program,
            fade: None,
            queued: None,
            bind: Bind::Unbound,
            reserved: false,
            fed: start,
            settled_from: start,
        });
        true
    }

    /// The router issued the output's video move at move tick `move_tick`
    /// (or a fallback landed): the output's audio follows its choice
    /// resolved against `target`. `choice = Some(c)` installs a take's
    /// explicit audio with the move (restored if the move is refused).
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
        choice: Option<AudioChoice>,
        move_tick: u64,
        after_video: Option<usize>,
        dispatcher: &mut RenditionDispatcher,
    ) {
        let before = self.carried(output);
        let Some(o) = self.outputs.get_mut(output) else {
            return;
        };
        if let Some(c) = choice {
            let old = std::mem::replace(&mut o.choice, c);
            if after_video.is_some() {
                o.restore = Some(old);
            }
        }
        if let Some(bus) = o.bus.as_mut() {
            bus.reserved = false;
        }
        let to = o.choice.resolve(target);
        let changed = self.change(output, &to, move_tick, Mode::Bind(after_video), dispatcher);
        if let (Change::Scheduled(_), Some(_)) = (changed, after_video) {
            if let Some(o) = self.outputs.get_mut(output) {
                o.pending_change = Some(before);
            }
        }
    }

    /// The output's video fell back to `target` without a take: an explicit
    /// choice stays, `Follow` follows the new video (never undone).
    pub(crate) fn fallback(
        &mut self,
        output: usize,
        target: &PictureBus,
        move_tick: u64,
        after_video: Option<usize>,
        dispatcher: &mut RenditionDispatcher,
    ) {
        let Some(o) = self.outputs.get(output) else {
            return;
        };
        let to = o.choice.resolve(target);
        let _ = self.change(output, &to, move_tick, Mode::Bind(after_video), dispatcher);
    }

    /// The output's video move was applied: the take's audio stays.
    pub(crate) fn video_applied(&mut self, output: usize) {
        if let Some(o) = self.outputs.get_mut(output) {
            o.restore = None;
            o.pending_change = None;
        }
    }

    /// The video move of a take whose audio was already scheduled was
    /// refused (or superseded), or a pending video take ended without a
    /// move: the take's audio choice is restored, and audio already
    /// scheduled fades back to the old audio (never a step).
    pub(crate) fn video_refused(
        &mut self,
        output: usize,
        tick: u64,
        dispatcher: &mut RenditionDispatcher,
    ) {
        let Some(o) = self.outputs.get_mut(output) else {
            return;
        };
        if let Some(old) = o.restore.take() {
            o.choice = old;
        }
        let back = o.pending_change.take();
        let Some(bus) = o.bus.as_mut() else {
            return;
        };
        bus.reserved = false;
        // The dispatcher dropped (or drops at its next drain) an audio move
        // gated on the refused video move: the output reads the shared stage.
        if matches!(bus.bind, Bind::Gated | Bind::Deferred { after: Some(_) }) {
            let reads_bus = dispatcher.audio_slot_key(output) == Some(bus.key);
            bus.bind = if reads_bus {
                Bind::Bound
            } else {
                Bind::Unbound
            };
        }
        let Some(back) = back else {
            return;
        };
        let started = bus.fade.as_ref().is_some_and(|f| f.xf.start < bus.fed);
        if started || bus.fade.is_none() {
            self.counters.faded_back = self.counters.faded_back.saturating_add(1);
            tracing::info!(output, "routing: video move refused; audio fades back");
        }
        let _ = self.change(output, &back, tick, Mode::Quiet, dispatcher);
    }

    /// Adopt an audio-only take at drain tick `tick`: the output's choice
    /// becomes `choice`, resolved against its current video `bus`.
    pub(crate) fn take_audio(
        &mut self,
        output: usize,
        choice: AudioChoice,
        bus: &PictureBus,
        tick: u64,
        dispatcher: &mut RenditionDispatcher,
    ) -> AudioTakeStart {
        let Some(o) = self.outputs.get_mut(output) else {
            return AudioTakeStart::Refused;
        };
        // A superseded audio take never applied: restore what was on air.
        let restore = o
            .watch
            .take()
            .map_or_else(|| o.choice.clone(), |w| w.restore);
        o.choice = choice;
        let to = o.choice.resolve(bus);
        match self.change(output, &to, tick, Mode::Bind(None), dispatcher) {
            Change::Unchanged => AudioTakeStart::Unchanged,
            Change::Refused => {
                if let Some(o) = self.outputs.get_mut(output) {
                    o.choice = restore;
                }
                AudioTakeStart::Refused
            }
            Change::Scheduled(g) => {
                if let Some(o) = self.outputs.get_mut(output) {
                    o.watch = Some(Watch {
                        g,
                        failed: false,
                        restore,
                    });
                }
                AudioTakeStart::Started
            }
        }
    }

    /// Progress of the output's audio-only take.
    pub(crate) fn take_progress(&self, output: usize) -> AudioTakeProgress {
        let Some(o) = self.outputs.get(output) else {
            return AudioTakeProgress::Failed;
        };
        let Some(w) = o.watch.as_ref() else {
            return AudioTakeProgress::Failed;
        };
        if w.failed {
            return AudioTakeProgress::Failed;
        }
        let Some(bus) = o.bus.as_ref() else {
            return AudioTakeProgress::Failed;
        };
        match w.g {
            Some(g) if bus.fed > g && bus.bind == Bind::Bound => AudioTakeProgress::Done(g),
            _ => AudioTakeProgress::Waiting,
        }
    }

    /// The audio-only take ended. `applied = false` (refused, superseded by a
    /// video take): the old choice is restored and the audio fades back.
    pub(crate) fn finish_take(
        &mut self,
        output: usize,
        applied: bool,
        bus: &PictureBus,
        tick: u64,
        dispatcher: &mut RenditionDispatcher,
    ) {
        let Some(o) = self.outputs.get_mut(output) else {
            return;
        };
        let Some(w) = o.watch.take() else {
            return;
        };
        if applied {
            return;
        }
        o.choice = w.restore;
        let to = o.choice.resolve(bus);
        let _ = self.change(output, &to, tick, Mode::Quiet, dispatcher);
    }

    /// Schedule the output's audio to become `to` from the drain/move tick
    /// `tick`: a crossfade of one tick of samples at the first grid boundary
    /// `G >= max(total_at(tick), warm, fed)`; while a fade runs, queued after
    /// it; a fade not yet started is replaced (latest wins).
    fn change(
        &mut self,
        output: usize,
        to: &AudioSource,
        tick: u64,
        mode: Mode,
        dispatcher: &mut RenditionDispatcher,
    ) -> Change {
        let has_bus = self.outputs.get(output).is_some_and(|o| o.bus.is_some());
        if !has_bus {
            if *to == AudioSource::Program || matches!(mode, Mode::Quiet) {
                return Change::Unchanged;
            }
            if !self.open(output, tick, dispatcher) {
                return Change::Refused;
            }
        }
        let len = take_fade_len(&self.clock, tick);
        let at = self.clock.total_at(tick);
        let program_config = self.program_config.clone();
        let Some(bus) = self.outputs.get_mut(output).and_then(|o| o.bus.as_mut()) else {
            return Change::Refused;
        };
        if bus.failed.load(Ordering::Acquire) {
            return Change::Refused;
        }
        // The slot must move onto the bus unless the bus is (moving) on
        // air, or the output keeps the program mix the shared stage carries.
        let needs_bind = matches!(mode, Mode::Bind(_))
            && !bus.bind.on_air()
            && !(*to == AudioSource::Program && *bus.target() == AudioSource::Program);
        if bus.target() == to && !needs_bind {
            return Change::Unchanged;
        }
        let running = bus.fade.as_ref().is_some_and(|f| f.xf.start < bus.fed);
        let mut g = None;
        let mut landed = None;
        if running {
            // Never two overlapping fades: from the running fade's target,
            // at the first grid boundary after it ends (`feed`).
            let target = bus.fade.as_ref().map(|f| (f.to.clone(), f.xf.start));
            bus.queued = (target.as_ref().map(|t| &t.0) != Some(to)).then(|| to.clone());
            if bus.queued.is_none() {
                // Back to the running fade's target: that fade's G.
                landed = target.map(|t| t.1);
            }
        } else {
            let from = match bus.fade.take() {
                Some(f) => f.from,
                None => bus.current.clone(),
            };
            bus.queued = None;
            bus.current = from.clone();
            if from != *to {
                // Rule 2: the bus coded the old signal for WARM_FRAMES
                // before G and G lies after everything already fed.
                let warm = bus
                    .start
                    .saturating_add(WARM_FRAMES.saturating_mul(AAC_FRAME_SAMPLES));
                let at_g = grid_ceil(at.max(warm).max(bus.fed), AAC_FRAME_SAMPLES);
                bus.fade = Some(Fade {
                    xf: TakeCrossfade { start: at_g, len },
                    from,
                    to: to.clone(),
                });
                g = Some(at_g);
            }
        }
        if let Mode::Bind(after) = mode {
            if needs_bind {
                bus.bind = Bind::Deferred { after };
                if !Self::try_bind(output, bus, program_config.as_ref(), dispatcher) {
                    self.counters.refused = self.counters.refused.saturating_add(1);
                    tracing::info!(
                        output,
                        "routing: audio move refused (encoder-format-mismatch or encoder not ready)"
                    );
                    self.close(output, dispatcher);
                    return Change::Refused;
                }
            }
        }
        #[cfg(test)]
        if let (Some(tap), Some(g)) = (self.tap.as_ref(), g) {
            if let Ok(mut t) = tap.lock() {
                t.moves.push((output, g));
            }
        }
        if g.is_none() && !running {
            // The bus already carries `to` once its pending changes are
            // dropped: nothing fades.
            return Change::Unchanged;
        }
        Change::Scheduled(g.or(landed))
    }

    /// Resolve a `Deferred` binding once the encoder's configuration is
    /// known: the slot moves onto the bus at its next fade's `G` (or the next
    /// grid boundary). Returns `false` when the move is refused (format
    /// mismatch, or the dispatcher refused it).
    fn try_bind(
        output: usize,
        bus: &mut PrivateBus,
        program_config: Option<&Vec<u8>>,
        dispatcher: &mut RenditionDispatcher,
    ) -> bool {
        let Bind::Deferred { after } = bus.bind else {
            return true;
        };
        if matches!(bus.config_seen, ConfigState::Pending) {
            match bus.config.try_recv() {
                Ok(cfg) => bus.config_seen = ConfigState::Known(cfg),
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Disconnected) => bus.config_seen = ConfigState::Known(None),
            }
        }
        // AudioSpecificConfig identity (§5.2): byte for byte.
        let same = matches!(
            &bus.config_seen,
            ConfigState::Known(Some(c)) if program_config == Some(c)
        );
        if !same {
            return false;
        }
        let g = match bus.fade.as_ref() {
            Some(f) if f.xf.start >= bus.fed => f.xf.start,
            _ => grid_ceil(bus.fed, AAC_FRAME_SAMPLES),
        };
        let Ok(g) = i64::try_from(g) else {
            return false;
        };
        if dispatcher
            .rebind_audio(output, Some(bus.key), g, after)
            .is_err()
        {
            return false;
        }
        bus.bind = if after.is_some() {
            Bind::Gated
        } else {
            Bind::Requested
        };
        true
    }

    /// A dispatcher move outcome for routed audio.
    pub(crate) fn on_outcome(
        &mut self,
        outcome: &MoveOutcome,
        dispatcher: &mut RenditionDispatcher,
    ) {
        match *outcome {
            MoveOutcome::AudioApplied {
                output_index, key, ..
            } => {
                let Some(bus) = self
                    .outputs
                    .get_mut(output_index)
                    .and_then(|o| o.bus.as_mut())
                else {
                    return;
                };
                match key {
                    Some(k) if k == bus.key => bus.bind = Bind::Bound,
                    None if matches!(bus.bind, Bind::Returning(_)) => {
                        // Back on the shared program stream (§5.2).
                        self.counters.returned = self.counters.returned.saturating_add(1);
                        self.close(output_index, dispatcher);
                    }
                    _ => {}
                }
            }
            MoveOutcome::AudioRefused { output_index } => {
                let Some(o) = self.outputs.get_mut(output_index) else {
                    return;
                };
                if let Some(bus) = o.bus.as_mut() {
                    if matches!(bus.bind, Bind::Gated | Bind::Requested) {
                        bus.bind = Bind::Unbound;
                        if let Some(w) = o.watch.as_mut() {
                            w.failed = true;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Feed one tick of PCM to every private bus. `program` is the
    /// post-loudnorm program block; `inputs` the per-source clean blocks of
    /// the same mix. Never blocks: a full queue fails that bus alone.
    #[allow(clippy::too_many_lines)] // One per-bus feed state machine.
    pub(crate) fn feed(
        &mut self,
        tick: u64,
        program: &AudioBlock,
        inputs: &[(&str, &AudioBlock)],
        dispatcher: &mut RenditionDispatcher,
    ) {
        let frames = program.frame_count();
        if frames == 0 || self.outputs.iter().all(|o| o.bus.is_none()) {
            return;
        }
        let end = self.clock.total_at(tick.saturating_add(1));
        let block_start = end.saturating_sub(u64::try_from(frames).unwrap_or(u64::MAX));
        let len = take_fade_len(&self.clock, tick);
        let preview: Option<Arc<str>> = self.preview.as_ref().and_then(|p| p.load());
        let program_config = self.program_config.clone();
        let mut failed = Vec::new();
        let mut refused = Vec::new();
        for (output, o) in self.outputs.iter_mut().enumerate() {
            let Some(bus) = o.bus.as_mut() else {
                continue;
            };
            if bus.failed.load(Ordering::Acquire) {
                failed.push(output);
                continue;
            }
            if let Bind::Deferred { .. } = bus.bind {
                if !Self::try_bind(output, bus, program_config.as_ref(), dispatcher) {
                    refused.push(output);
                    continue;
                }
                // Still unknown: hold the old signal (both encoders code it)
                // until the slot can move; the fade starts later.
                if let (Bind::Deferred { .. }, Some(f)) = (bus.bind, bus.fade.as_mut()) {
                    if f.xf.start >= bus.fed && f.xf.start < end {
                        f.xf.start = grid_ceil(end, AAC_FRAME_SAMPLES);
                        if let Some(w) = o.watch.as_mut() {
                            w.g = Some(f.xf.start);
                        }
                    }
                }
            }
            if end <= bus.start {
                continue;
            }
            let from = bus.start.max(block_start).max(bus.fed);
            let skip = usize::try_from(from.saturating_sub(block_start)).unwrap_or(usize::MAX);
            let ch = self.channels;
            let gain = &*self.route_gain;
            let (old, new, out) = (
                &mut self.scratch.0,
                &mut self.scratch.1,
                &mut self.scratch.2,
            );
            let pcm_into = |src: &AudioSource, dst: &mut Vec<f32>| {
                source_pcm_into(
                    dst,
                    src,
                    program,
                    inputs,
                    preview.as_deref(),
                    gain,
                    frames,
                    ch,
                );
            };
            out.clear();
            match bus.fade.as_ref() {
                Some(fade) => {
                    pcm_into(&fade.from, old);
                    pcm_into(&fade.to, new);
                    fade.xf.mix_into(old, new, ch, block_start, out);
                    #[cfg(test)]
                    if fade.xf.start >= from && fade.xf.start < end {
                        if let Some(Ok(mut t)) = self.tap.as_ref().map(|t| t.lock()) {
                            t.fades.push((output, fade.xf.start, fade.xf.len));
                        }
                    }
                }
                None => pcm_into(&bus.current, out),
            }
            // The one per-tick allocation: the job crosses to the bus's
            // encode thread by value.
            let tail = out.get(skip.saturating_mul(ch)..).map(<[f32]>::to_vec);
            let Some(tail) = tail else {
                continue;
            };
            if tail.is_empty() {
                continue;
            }
            #[cfg(test)]
            if let Some(Ok(mut t)) = self.tap.as_ref().map(|t| t.lock()) {
                t.private.push((output, from, tail.clone()));
            }
            bus.fed = end;
            let ended = bus
                .fade
                .as_ref()
                .filter(|f| end >= f.xf.end())
                .map(|f| f.xf.end());
            if let Some(fade_end) = ended {
                if let Some(f) = bus.fade.take() {
                    bus.current = f.to;
                }
                bus.settled_from = fade_end;
                if let Some(next) = bus.queued.take() {
                    if next != bus.current {
                        let g = grid_ceil(fade_end.max(end), AAC_FRAME_SAMPLES);
                        bus.fade = Some(Fade {
                            xf: TakeCrossfade { start: g, len },
                            from: bus.current.clone(),
                            to: next,
                        });
                        if let Some(w) = o.watch.as_mut().filter(|w| w.g.is_none()) {
                            w.g = Some(g);
                        }
                        #[cfg(test)]
                        if let Some(Ok(mut t)) = self.tap.as_ref().map(|t| t.lock()) {
                            t.moves.push((output, g));
                        }
                    }
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
        for output in refused {
            self.counters.refused = self.counters.refused.saturating_add(1);
            if let Some(w) = self.outputs.get_mut(output).and_then(|o| o.watch.as_mut()) {
                w.failed = true;
            }
            self.close(output, dispatcher);
        }
        for output in failed {
            self.fail(output, dispatcher);
        }
        self.settle(dispatcher);
    }

    /// Return idle buses to the shared program stream (§5.2) and close buses
    /// nobody reads.
    fn settle(&mut self, dispatcher: &mut RenditionDispatcher) {
        let mut close = Vec::new();
        for (output, o) in self.outputs.iter_mut().enumerate() {
            let Some(bus) = o.bus.as_mut() else {
                continue;
            };
            let busy = bus.reserved
                || bus.fade.is_some()
                || bus.queued.is_some()
                || o.watch.is_some()
                || o.pending_change.is_some();
            if busy {
                continue;
            }
            match bus.bind {
                // Never on air and nothing scheduled: nobody hears it.
                Bind::Unbound => close.push(output),
                Bind::Bound
                    if bus.current == AudioSource::Program
                        && bus.fed.saturating_sub(bus.settled_from) >= RETURN_AFTER_SAMPLES =>
                {
                    // Both encoders coded the identical program PCM for at
                    // least 4 frames: move back at the next grid boundary.
                    let g = grid_ceil(bus.fed, AAC_FRAME_SAMPLES);
                    let Ok(g_i64) = i64::try_from(g) else {
                        continue;
                    };
                    if dispatcher.rebind_audio(output, None, g_i64, None).is_ok() {
                        bus.bind = Bind::Returning(g);
                        #[cfg(test)]
                        if let Some(Ok(mut t)) = self.tap.as_ref().map(|t| t.lock()) {
                            t.returns.push((output, g));
                        }
                    }
                }
                _ => {}
            }
        }
        for output in close {
            self.close(output, dispatcher);
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
                .outputs
                .iter()
                .filter_map(|o| o.bus.as_ref())
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
            .outputs
            .iter()
            .enumerate()
            .filter_map(|(i, o)| {
                o.bus
                    .as_ref()
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
        if let Some(o) = self.outputs.get_mut(output) {
            if let Some(w) = o.watch.as_mut() {
                w.failed = true;
            }
            o.pending_change = None;
        }
        self.close(output, dispatcher);
    }

    /// Retire the bus's stage and close its input (the thread finishes and
    /// ends on its own; never joined here). The output's slot returns to the
    /// shared program stage (the dispatcher's `AudioFallback`).
    fn close(&mut self, output: usize, dispatcher: &mut RenditionDispatcher) {
        if let Some(bus) = self.outputs.get_mut(output).and_then(|o| o.bus.take()) {
            let _ = dispatcher.retire_audio_key(bus.key);
            drop(bus.jobs);
        }
    }

    /// End of run: close every bus (never joins).
    pub(crate) fn shutdown(&mut self, dispatcher: &mut RenditionDispatcher) {
        for output in 0..self.outputs.len() {
            self.close(output, dispatcher);
        }
        while self.events.try_recv().is_ok() {}
    }
}

/// One tick of `src` as interleaved PCM of exactly `frames` frames.
#[cfg(test)]
fn source_pcm(
    src: &AudioSource,
    program: &AudioBlock,
    inputs: &[(&str, &AudioBlock)],
    preview: Option<&str>,
    gain: &(dyn Fn(&str) -> f32 + Send + Sync),
    frames: usize,
    channels: usize,
) -> Vec<f32> {
    let mut out = Vec::new();
    source_pcm_into(
        &mut out, src, program, inputs, preview, gain, frames, channels,
    );
    out
}

/// One tick of `src` into `out` (cleared first; its capacity is reused).
#[allow(clippy::too_many_arguments)] // reason: one tick's inputs; called per bus per tick.
fn source_pcm_into(
    out: &mut Vec<f32>,
    src: &AudioSource,
    program: &AudioBlock,
    inputs: &[(&str, &AudioBlock)],
    preview: Option<&str>,
    gain: &(dyn Fn(&str) -> f32 + Send + Sync),
    frames: usize,
    channels: usize,
) {
    let len = frames.saturating_mul(channels);
    out.clear();
    // (input id, route gain applied?)
    let (id, gained) = match src {
        AudioSource::Program => {
            out.extend_from_slice(program.interleaved());
            return;
        }
        AudioSource::Silence => {
            out.resize(len, 0.0);
            return;
        }
        AudioSource::Source(id) => (Some(id.as_str()), true),
        AudioSource::Preview => (preview, true),
        AudioSource::Track { input, .. } => (Some(input.as_str()), false),
    };
    out.resize(len, 0.0);
    let block = id.and_then(|id| {
        inputs
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, b)| (if gained { gain(id) } else { 1.0 }, *b))
    });
    if let Some((g, block)) = block {
        // input_tracks blocks are pre-gain (the mixer applies the program
        // gain): the route gain/mute is applied here, exactly once (never to
        // a discrete track, which carries its input's clean block as the
        // discrete-track encode does). A block shorter than the tick (a shed
        // catch-up) is aligned to its end.
        let src = block.interleaved();
        let offset = len.saturating_sub(src.len());
        for (dst, s) in out.iter_mut().skip(offset).zip(src) {
            *dst = s * g;
        }
    }
}

/// What a private bus's encode thread needs.
struct BusWorker {
    config: AudioEncodeConfig,
    start: i64,
    stage: (usize, u64),
    events: SyncSender<AudioEvent>,
    config_tx: SyncSender<Option<Vec<u8>>>,
    dropped: Arc<AtomicU64>,
    #[cfg(test)]
    hook: Option<WorkerHook>,
}

/// The private bus's encode thread: open the AAC encoder on the global grid,
/// publish its `AudioSpecificConfig`, encode each tick's PCM, drop the priming
/// packet (never delivered, §5.2), and send packets + a proven watermark.
fn spawn_bus_worker(
    worker: BusWorker,
    failed: Arc<AtomicBool>,
) -> std::io::Result<SyncSender<Vec<f32>>> {
    let (jobs_tx, jobs) = mpsc::sync_channel::<Vec<f32>>(PRIVATE_AUDIO_QUEUE);
    std::thread::Builder::new()
        .name("routing-audio".to_owned())
        .spawn(move || {
            let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                bus_worker(&worker, &jobs)
            }));
            if !matches!(run, Ok(true)) {
                failed.store(true, Ordering::Release);
            }
        })?;
    Ok(jobs_tx)
}

/// Returns `false` on an encoder failure or a lost event.
fn bus_worker(w: &BusWorker, jobs: &Receiver<Vec<f32>>) -> bool {
    let (key, generation) = w.stage;
    let Ok(mut encoder) = AudioRenditionEncoder::starting_at_sample(&w.config, w.start) else {
        let _ = w.config_tx.try_send(None);
        return false;
    };
    let _ = w
        .config_tx
        .try_send(encoder.codec_params().codec_config_bytes());
    let channels = usize::from(w.config.channels).max(1);
    let frame = encoder.frame_samples().unwrap_or(1024);
    #[cfg(test)]
    let mut job_index = 0_u64;
    while let Ok(pcm) = jobs.recv() {
        #[cfg(test)]
        {
            if let Some(hook) = w.hook.as_ref() {
                hook(job_index);
            }
            job_index += 1;
        }
        let Ok(mut packets) = encoder.encode_interleaved(&pcm, pcm.len() / channels) else {
            return false;
        };
        packets.retain(|p| p.dts().is_some_and(|d| d >= w.start));
        let watermark = packets.last().and_then(EncodedPacket::pts).and_then(|pts| {
            crate::rendition_workers::next_frame_lower_bound_ns(pts, frame, w.config.sample_rate)
        });
        if packets.is_empty() && watermark.is_none() {
            continue;
        }
        if w.events
            .try_send(AudioEvent {
                key,
                generation,
                packets,
                watermark,
            })
            .is_err()
        {
            w.dropped.fetch_add(1, Ordering::Relaxed);
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
